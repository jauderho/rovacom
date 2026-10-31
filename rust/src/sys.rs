//! Unix system calls the C client makes directly: `select`, raw `read` /
//! `write` / `close` on the standard fds, `send` without `MSG_NOSIGNAL`,
//! the default `SIGPIPE` disposition, `isatty`, and `tcgetattr` /
//! `tcsetattr`.
//!
//! This is the only module with `unsafe` code.

use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
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

/// Which fds [`select`] reported ready.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ready {
    pub sock_read: bool,
    pub sock_write: bool,
    pub stdin_read: bool,
}

/// C `select` over the socket (always for read; for write if `want_write`)
/// and stdin (for read if `want_stdin`). `timeout` `None` blocks.
///
/// `select` is used, not `poll`, because macOS `poll(2)` does not support
/// devices: a `/dev/null` stdin reports `POLLNVAL`, while `select` (and C)
/// report it readable.
pub fn select(
    sock: &TcpStream,
    want_stdin: bool,
    want_write: bool,
    timeout: Option<Duration>,
) -> io::Result<Ready> {
    let fd = sock.as_raw_fd();
    let setsize = RawFd::try_from(libc::FD_SETSIZE).unwrap_or(RawFd::MAX);
    if !(0..setsize).contains(&fd) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut rfds = empty_fd_set();
    let mut wfds = empty_fd_set();
    // SAFETY: both sets are initialized by `empty_fd_set`, and `fd` and
    // `STDIN_FD` are within `0..FD_SETSIZE` (checked above).
    unsafe {
        libc::FD_SET(fd, &raw mut rfds);
        if want_write {
            libc::FD_SET(fd, &raw mut wfds);
        }
        if want_stdin {
            libc::FD_SET(STDIN_FD, &raw mut rfds);
        }
    }
    let mut tv = timeout.map(|t| libc::timeval {
        tv_sec: t.as_secs().try_into().unwrap_or(libc::time_t::MAX),
        // Below 1_000_000, so it fits `suseconds_t` (i32 on macOS, i64 on Linux).
        tv_usec: t.subsec_micros() as libc::suseconds_t,
    });
    let tv_ptr = tv.as_mut().map_or(std::ptr::null_mut(), std::ptr::from_mut);
    // SAFETY: the sets are initialized and hold only fds below `fd + 1`;
    // the timeout pointer is null or points at a live `timeval`.
    let n = unsafe {
        libc::select(
            fd.max(STDIN_FD) + 1,
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
        let r = select(&client, false, true, short).unwrap();
        assert!(r.sock_write && !r.sock_read && !r.stdin_read);
        let r = select(&client, false, false, short).unwrap();
        assert_eq!(r, Ready::default());
        server.write_all(b"x").unwrap();
        let r = select(&client, false, false, None).unwrap();
        assert!(r.sock_read && !r.sock_write);
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
    }
}
