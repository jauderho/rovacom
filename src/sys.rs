//! Unix system calls the C client makes directly: `select`, raw `read` /
//! `write` / `close` on the standard fds, `send` without `MSG_NOSIGNAL`,
//! `recv`, the `SIGPIPE` disposition, `isatty`, `tcgetattr` /
//! `tcsetattr`, `TIOCGWINSZ`, the signal self-pipe, `errno` / `strerror`
//! and the relay's listening socket.
//!
//! This is the only module with `unsafe` code.

use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::Duration;

pub const STDIN_FD: RawFd = 0;
pub const STDOUT_FD: RawFd = 1;
pub const STDERR_FD: RawFd = 2;

/// Converts a C `ssize_t` result to `io::Result`.
fn check(n: libc::ssize_t) -> io::Result<usize> {
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

/// Restores the default `SIGPIPE` disposition.
///
/// The Rust runtime ignores `SIGPIPE` before `main`; the C client does not,
/// so a write to a closed pipe or socket must kill the process.
pub fn default_sigpipe() {
    // SAFETY: `signal` with `SIG_DFL` installs no handler code; it only
    // changes the disposition of a valid signal number.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Lets `send` on `sock` raise `SIGPIPE`, as for a C socket.
///
/// On Apple targets std sets `SO_NOSIGPIPE` on every socket it creates.
/// Errors are ignored: the C client never set the option.
#[cfg(target_vendor = "apple")]
pub fn allow_sigpipe(sock: &TcpStream) {
    let off: libc::c_int = 0;
    // SAFETY: `sock` owns a valid socket fd; `off` is a live `c_int` and the
    // length passed is its size.
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw const off).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Lets `send` on `sock` raise `SIGPIPE`, as for a C socket. Nothing to do
/// here: std uses `MSG_NOSIGNAL` per call, and [`send`] does not.
#[cfg(not(target_vendor = "apple"))]
pub fn allow_sigpipe(_sock: &TcpStream) {}

/// C `send(fd, buf, len, 0)`: no `MSG_NOSIGNAL`, so a closed peer raises
/// `SIGPIPE` (see [`allow_sigpipe`]).
pub fn send(sock: &TcpStream, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: `sock` owns a valid fd; `buf` is valid for `buf.len()` bytes.
    let n = unsafe { libc::send(sock.as_raw_fd(), buf.as_ptr().cast(), buf.len(), 0) };
    check(n)
}

/// C `recv(fd, buf, len, 0)`: one call, no retry.
pub fn recv(sock: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `sock` owns a valid fd; `buf` is valid for writes of
    // `buf.len()` bytes.
    let n = unsafe { libc::recv(sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
    check(n)
}

/// C `signal(SIGPIPE, SIG_IGN)` (relay): a write to a closed socket fails
/// with `EPIPE` instead of killing the process.
pub fn ignore_sigpipe() {
    // SAFETY: `signal` with `SIG_IGN` installs no handler code; it only
    // changes the disposition of a valid signal number.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// The calling thread's `errno`.
pub fn errno() -> libc::c_int {
    // SAFETY: `errno_location` returns the calling thread's errno slot,
    // which is valid for the life of the thread.
    unsafe { *errno_location() }
}

/// Sets the calling thread's `errno`.
pub fn set_errno(value: libc::c_int) {
    // SAFETY: as in `errno`.
    unsafe { *errno_location() = value }
}

/// C `strerror(errnum)` text, from the thread-safe XSI `strerror_r` (glibc
/// `__xpg_strerror_r`), which gives the same text. Unknown numbers give the
/// libc "unknown error" text, which `strerror_r` writes even when it
/// reports `EINVAL`.
pub fn strerror(errnum: libc::c_int) -> Vec<u8> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes; `strerror_r`
    // writes a NUL-terminated string of at most that many bytes.
    unsafe {
        libc::strerror_r(errnum, buf.as_mut_ptr().cast(), buf.len());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf[..len].to_vec()
}

/// C `create_listen_socket(port)` (`socket.c`): an `AF_INET` stream socket
/// with `SO_REUSEADDR`, bound to `INADDR_LOOPBACK` on `port` truncated to
/// 16 bits (C `htons`), listening with backlog `SOMAXCONN`. Like C, the socket is
/// not close-on-exec. On failure the socket is closed and the error is the
/// `errno` of the failed call.
///
/// Intentional change: C binds `INADDR_ANY`, which lets any host on the
/// network use the forwarded device ports. Here only this host can.
pub fn create_listen_socket(port: i32) -> io::Result<std::net::TcpListener> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: `socket` takes plain integers and returns a new fd or -1.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new socket that nothing else owns; `OwnedFd` closes
    // it on every path below.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let reuse: libc::c_int = 1;
    // SAFETY: `zeroed` is a valid `sockaddr_in` (all integer fields).
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = (port as u16).to_be();
    addr.sin_addr.s_addr = libc::INADDR_LOOPBACK.to_be();
    // SAFETY: `fd` is a valid socket; the option and address pointers point
    // at live values of the sizes passed.
    let ok = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&raw const reuse).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
            && libc::bind(
                fd,
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ) == 0
            && libc::listen(fd, libc::SOMAXCONN) == 0
    };
    if !ok {
        // Read before `owned` is dropped: `close` succeeds and C reports
        // the errno of the failed call.
        return Err(io::Error::last_os_error());
    }
    Ok(std::net::TcpListener::from(owned))
}

/// C `read(fd, buf, len)`.
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes; an invalid fd
    // only makes the call fail with `EBADF`.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    check(n)
}

/// C `close(fd)`; the result is ignored, as in C.
///
/// Only for the standard fds 0 and 1, which no Rust object owns here.
pub fn close(fd: RawFd) {
    debug_assert!(fd == STDIN_FD || fd == STDOUT_FD);
    // SAFETY: fds 0 and 1 are not owned by any Rust object in this program
    // (std `Stdin`/`Stdout` are never used), so closing them cannot cause a
    // double close or a use of a reused fd by safe code.
    unsafe {
        libc::close(fd);
    }
}

/// Unbuffered writer on a raw fd (C `write(2)`), used for fds 1 and 2.
///
/// `write_all` retries short writes and `EINTR` (intentional change B7: C
/// ignores short writes).
#[derive(Clone, Copy, Debug)]
pub struct FdWriter(pub RawFd);

impl io::Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid for reads of `buf.len()` bytes; an invalid
        // or closed fd only makes the call fail with `EBADF`.
        let n = unsafe { libc::write(self.0, buf.as_ptr().cast(), buf.len()) };
        check(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Unbuffered reader on a raw fd (C `read(2)`), used for fd 0.
#[derive(Clone, Copy, Debug)]
pub struct FdReader(pub RawFd);

impl io::Read for FdReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read(self.0, buf)
    }
}

/// C `isatty(fd)`.
pub fn isatty(fd: RawFd) -> bool {
    // SAFETY: `isatty` only inspects the fd; an invalid fd returns 0.
    unsafe { libc::isatty(fd) == 1 }
}

/// Terminal attributes (C `struct termios`).
pub type Termios = libc::termios;

/// An all-zero `struct termios`, like a zero-initialized C static.
pub fn zeroed_termios() -> Termios {
    // SAFETY: `termios` is a plain C struct of integers and integer arrays,
    // for which all-zero bytes are a valid value.
    unsafe { std::mem::zeroed() }
}

/// C `tcgetattr(fd, &t)`; `None` if the call fails.
pub fn tcgetattr(fd: RawFd) -> Option<Termios> {
    let mut t = zeroed_termios();
    // SAFETY: `t` is a live, initialized `termios` the call may write to; an
    // invalid fd or a non-terminal only makes the call fail.
    let rc = unsafe { libc::tcgetattr(fd, &raw mut t) };
    (rc == 0).then_some(t)
}

/// C `tcsetattr(fd, TCSANOW, t)`; the result is ignored, as in C.
pub fn tcsetattr_now(fd: RawFd, t: &Termios) {
    // SAFETY: `t` points at a live `termios` that the call only reads; an
    // invalid fd or a non-terminal only makes the call fail.
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, t);
    }
}

/// C `ioctl(fd, TIOCGWINSZ)`: `(rows, cols)`, or `None` if the call fails
/// (for example when `fd` is not a terminal).
pub fn winsize(fd: RawFd) -> Option<(u16, u16)> {
    // SAFETY: `winsize` is a plain C struct of integers; all-zero bytes are
    // a valid value.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `TIOCGWINSZ` writes one `struct winsize` through the pointer,
    // which points at a live `winsize`; an invalid fd only makes it fail.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut ws) };
    (rc == 0).then_some((ws.ws_row, ws.ws_col))
}

// ---------------------------------------------------------------------------
// Signal forwarding (`-s`, `-t`).
//
// Intentional change B4: the C handler keeps a `sigset_t` in the pipe and
// `data_xfer` reads it only when a stale `errno` is `EINTR`, so signals can
// be dropped. Here the handler sets the signal's bit in `CAUGHT` and writes
// one wakeup byte to the pipe; the main loop drains the pipe and takes the
// bits, so no signal is lost. Duplicates of a signal not yet forwarded are
// coalesced, as in a C `sigset_t`.
// ---------------------------------------------------------------------------

/// Signals caught and not yet forwarded: bit `n` is signal `n`.
static CAUGHT: AtomicU64 = AtomicU64::new(0);
/// Read and write ends of the self-pipe; -1 until [`signal_pipe`] has run.
static PIPE_READ: AtomicI32 = AtomicI32::new(-1);
static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

/// Handle to the self-pipe. Its fds stay open until the process exits,
/// because the handlers stay installed until then.
#[derive(Clone, Copy, Debug)]
pub struct SignalPipe {
    read_fd: RawFd,
}

/// Records `signo` in the caught set. Async-signal-safe (one atomic op).
fn record_signal(signo: libc::c_int) {
    if (0..64).contains(&signo) {
        CAUGHT.fetch_or(1u64 << signo, Ordering::SeqCst);
    }
}

/// The signal handler: records the signal, then wakes the main loop.
///
/// Only an atomic operation, `write(2)` and `errno` access (all
/// async-signal-safe) are used. Unlike C, the window size is not read here:
/// the main loop reads it when it forwards the SIGWINCH, which gives the
/// same (latest) size. `errno` is saved and restored, so an interrupted
/// call in the main loop never sees a value set by the handler.
extern "C" fn on_signal(signo: libc::c_int) {
    let errno = errno_location();
    // SAFETY: `errno_location` returns the calling thread's errno slot,
    // which is valid for the life of the thread.
    let saved = unsafe { *errno };
    record_signal(signo);
    let fd = PIPE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: `byte` is a live 1-byte buffer; the fd is the pipe write
        // end, which is never closed. `write` is async-signal-safe.
        unsafe {
            libc::write(fd, (&raw const byte).cast(), 1);
        }
    }
    // SAFETY: as above.
    unsafe { *errno = saved };
}

/// Address of the calling thread's `errno`.
fn errno_location() -> *mut libc::c_int {
    // SAFETY: these libc functions take no arguments and always return the
    // calling thread's errno address.
    #[cfg(target_vendor = "apple")]
    unsafe {
        libc::__error()
    }
    #[cfg(not(target_vendor = "apple"))]
    unsafe {
        libc::__errno_location()
    }
}

/// Sets `O_NONBLOCK` on `fd`.
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `F_GETFL` / `F_SETFL` take and return plain integers; an
    // invalid fd only makes the calls fail.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// C `pipe(signalpipe)`; both ends non-blocking (C makes only the read end
/// non-blocking; a non-blocking write end lets the handler never block).
/// Created once per process: later calls return the same pipe.
pub fn signal_pipe() -> io::Result<SignalPipe> {
    let existing = PIPE_READ.load(Ordering::SeqCst);
    if existing >= 0 {
        return Ok(SignalPipe { read_fd: existing });
    }
    let mut fds: [libc::c_int; 2] = [-1, -1];
    // SAFETY: `fds` is a live array of two `c_int`s that `pipe` fills.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let [r, w] = fds;
    if let Err(e) = set_nonblocking(r).and_then(|()| set_nonblocking(w)) {
        // SAFETY: `r` and `w` were just created by `pipe` and nothing else
        // uses them.
        unsafe {
            libc::close(r);
            libc::close(w);
        }
        return Err(e);
    }
    PIPE_WRITE.store(w, Ordering::SeqCst);
    PIPE_READ.store(r, Ordering::SeqCst);
    Ok(SignalPipe { read_fd: r })
}

/// C `sigaction(signo, {signal_to_pipe, SA_RESTART})` for each signal. The
/// handlers stay installed until the process exits. Errors are ignored, as
/// in C.
pub fn catch_signals(signals: &[libc::c_int]) {
    for &signo in signals {
        // SAFETY: `sigaction` is a plain C struct; all-zero bytes are a valid
        // value (empty mask, no flags).
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        let handler: extern "C" fn(libc::c_int) = on_signal;
        sa.sa_sigaction = handler as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART;
        // SAFETY: `sa` is a live, initialized `sigaction` whose handler only
        // performs async-signal-safe operations (see `on_signal`); the old
        // action is not requested.
        unsafe {
            libc::sigaction(signo, &raw const sa, std::ptr::null_mut());
        }
    }
}

/// Lowest signal number in `mask`, if any.
pub fn lowest_signal(mask: u64) -> Option<libc::c_int> {
    (mask != 0).then(|| mask.trailing_zeros() as libc::c_int)
}

impl SignalPipe {
    /// Reads every pending wakeup byte.
    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        while matches!(read(self.read_fd, &mut buf), Ok(n) if n > 0) {}
    }

    /// Takes the lowest caught signal out of the set (C processes one
    /// signal per loop iteration, lowest number first). If more remain, a
    /// wakeup byte is written so the main loop comes back to them, as C
    /// writes the remaining set back to the pipe.
    pub fn take(&self) -> Option<libc::c_int> {
        let signo = lowest_signal(CAUGHT.load(Ordering::SeqCst))?;
        let bit = 1u64 << signo;
        let before = CAUGHT.fetch_and(!bit, Ordering::SeqCst);
        if before & !bit != 0 {
            let w = PIPE_WRITE.load(Ordering::SeqCst);
            // A full pipe (EAGAIN) is already readable.
            let _ = io::Write::write(&mut FdWriter(w), &[1]);
        }
        Some(signo)
    }
}

/// Which fds [`select`] reported ready.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ready {
    pub sock_read: bool,
    pub sock_write: bool,
    pub stdin_read: bool,
    pub signal_read: bool,
}

/// C `select` over the socket (always for read; for write if `want_write`),
/// stdin (for read if `want_stdin`) and the signal pipe (for read if
/// `signals` is given). `timeout` `None` blocks.
///
/// `select` is used, not `poll`, because macOS `poll(2)` does not support
/// devices: a `/dev/null` stdin reports `POLLNVAL`, while `select` (and C)
/// report it readable.
pub fn select(
    sock: &TcpStream,
    want_stdin: bool,
    want_write: bool,
    signals: Option<SignalPipe>,
    timeout: Option<Duration>,
) -> io::Result<Ready> {
    let fd = sock.as_raw_fd();
    let sig_fd = signals.map(|p| p.read_fd);
    let setsize = RawFd::try_from(libc::FD_SETSIZE).unwrap_or(RawFd::MAX);
    let in_range = |f: RawFd| (0..setsize).contains(&f);
    if !in_range(fd) || !sig_fd.is_none_or(in_range) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut rfds = empty_fd_set();
    let mut wfds = empty_fd_set();
    // SAFETY: both sets are initialized by `empty_fd_set`, and `fd`,
    // `STDIN_FD` and `sig_fd` are within `0..FD_SETSIZE` (checked above).
    unsafe {
        libc::FD_SET(fd, &raw mut rfds);
        if want_write {
            libc::FD_SET(fd, &raw mut wfds);
        }
        if want_stdin {
            libc::FD_SET(STDIN_FD, &raw mut rfds);
        }
        if let Some(s) = sig_fd {
            libc::FD_SET(s, &raw mut rfds);
        }
    }
    let mut tv = timeout.map(|t| libc::timeval {
        tv_sec: t.as_secs().try_into().unwrap_or(libc::time_t::MAX),
        // Below 1_000_000, so it fits `suseconds_t` (i32 on macOS, i64 on Linux).
        tv_usec: t.subsec_micros() as libc::suseconds_t,
    });
    let tv_ptr = tv.as_mut().map_or(std::ptr::null_mut(), std::ptr::from_mut);
    let nfds = fd.max(STDIN_FD).max(sig_fd.unwrap_or(0)) + 1;
    // SAFETY: the sets are initialized and hold only fds below `nfds`; the
    // timeout pointer is null or points at a live `timeval`.
    let n = unsafe {
        libc::select(
            nfds,
            &raw mut rfds,
            &raw mut wfds,
            std::ptr::null_mut(),
            tv_ptr,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the sets are initialized and the fds are below `FD_SETSIZE`.
    unsafe {
        Ok(Ready {
            sock_read: libc::FD_ISSET(fd, &raw const rfds),
            sock_write: want_write && libc::FD_ISSET(fd, &raw const wfds),
            stdin_read: want_stdin && libc::FD_ISSET(STDIN_FD, &raw const rfds),
            signal_read: sig_fd.is_some_and(|s| libc::FD_ISSET(s, &raw const rfds)),
        })
    }
}

fn empty_fd_set() -> libc::fd_set {
    let mut set = std::mem::MaybeUninit::<libc::fd_set>::uninit();
    // SAFETY: `FD_ZERO` initializes every byte of the set it is given.
    unsafe {
        libc::FD_ZERO(set.as_mut_ptr());
        set.assume_init()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn select_reports_socket_states() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let short = Some(Duration::from_millis(1));
        let r = select(&client, false, true, None, short).unwrap();
        assert!(r.sock_write && !r.sock_read && !r.stdin_read);
        let r = select(&client, false, false, None, short).unwrap();
        assert_eq!(r, Ready::default());
        server.write_all(b"x").unwrap();
        let r = select(&client, false, false, None, None).unwrap();
        assert!(r.sock_read && !r.sock_write);
    }

    #[test]
    fn lowest_signal_number() {
        assert_eq!(lowest_signal(0), None);
        assert_eq!(lowest_signal(1 << 15 | 1 << 2), Some(2));
        assert_eq!(lowest_signal(1 << 63), Some(63));
    }

    /// The only test that touches the process-wide caught set and pipe.
    #[test]
    fn caught_signals_lowest_first_and_coalesced() {
        let pipe = signal_pipe().unwrap();
        assert_eq!(signal_pipe().unwrap().read_fd, pipe.read_fd);
        pipe.drain();
        assert_eq!(pipe.take(), None);
        for signo in [
            libc::SIGTERM,
            libc::SIGWINCH,
            libc::SIGHUP,
            libc::SIGTERM,
            libc::SIGINT,
        ] {
            record_signal(signo);
        }
        record_signal(-1);
        record_signal(64);
        let mut taken = Vec::new();
        while let Some(s) = pipe.take() {
            taken.push(s);
            // A wakeup byte is left for the rest, none after the last one.
            let readable = select_signal_only(pipe);
            assert_eq!(readable, taken.len() < 4, "after {taken:?}");
            pipe.drain();
        }
        assert_eq!(
            taken,
            [libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGWINCH]
        );

        // A real signal: the handler records it and writes a wakeup byte.
        catch_signals(&[libc::SIGWINCH]);
        // SAFETY: `raise` with a valid signal whose handler is installed.
        unsafe {
            libc::raise(libc::SIGWINCH);
            libc::raise(libc::SIGWINCH);
        }
        assert!(select_signal_only(pipe));
        pipe.drain();
        assert!(!select_signal_only(pipe));
        assert_eq!(pipe.take(), Some(libc::SIGWINCH));
        assert_eq!(pipe.take(), None);
    }

    /// Whether the signal pipe is readable right now.
    fn select_signal_only(pipe: SignalPipe) -> bool {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let r = select(&client, false, false, Some(pipe), Some(Duration::ZERO)).unwrap();
        r.signal_read
    }

    #[test]
    fn send_and_write() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        allow_sigpipe(&client);
        assert_eq!(send(&client, b"abc").unwrap(), 3);
        assert!(FdWriter(-1).write(b"x").is_err());
        assert!(read(-1, &mut [0u8; 1]).is_err());
        assert!(io::Read::read(&mut FdReader(-1), &mut [0u8; 1]).is_err());
    }

    #[test]
    fn terminal_calls_on_non_terminals() {
        assert!(!isatty(-1));
        assert!(tcgetattr(-1).is_none());
        // Fails with EBADF; must not panic.
        tcsetattr_now(-1, &zeroed_termios());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(!isatty(listener.as_raw_fd()));
        assert!(tcgetattr(listener.as_raw_fd()).is_none());
        assert_eq!(winsize(-1), None);
        assert_eq!(winsize(listener.as_raw_fd()), None);
    }
}
