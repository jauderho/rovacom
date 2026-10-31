//! Port forwarding (`-P`, `-f local:remote,...`): emulation of C
//! `tcprelay.c`.
//!
//! As in C: one thread per forwarded port accepts local connections; each
//! accepted connection gets a new connection to the device port, the
//! command `connect tcp-port://<remote>\n`, and two threads that copy data
//! (device to host, host to device). The main thread never returns.
//! `SIGPIPE` is ignored. Threads that fail to start are ignored, as C
//! ignores `pthread_create` errors.
//!
//! Output: stdout lines go through one shared [`StdioOut`] (C `stdout` is
//! one `FILE`): line-buffered on a terminal, fully buffered otherwise, so on
//! a pipe they appear only once 4 KiB has accumulated and are lost when
//! the process is killed. Errors are C `REPORT_ERROR()` lines on stderr:
//! `src/tcprelay.c:<line>: <strerror(errno)>`, with the C source line of
//! each call site.
//!
//! Intentional change B16: C closes a socket that the other thread of the
//! tunnel may still use (and whose fd number a new `accept` may reuse, so
//! data could go to the wrong connection). Here both threads share both
//! sockets, and where C calls `close` the thread calls `shutdown(Both)`; the
//! fds are released when both threads are done. Observable effect on
//! Linux: when one side of a tunnel ends, the other side gets its FIN at
//! once and the other thread ends at once, as if its peer had closed (C
//! keeps the other thread blocked in `recv` until that peer sends or
//! closes). macOS `close` already wakes the blocked `recv` with `EBADF` and
//! sends the FIN; that result is emulated (see [`CLOSE_WAKES_WITH_EBADF`]).

use std::io::Write;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::cli::PortPair;
use crate::net;
use crate::session;
use crate::stdio::StdioOut;
use crate::sys::{self, FdWriter, STDERR_FD, STDOUT_FD};

/// C `PACKET_MAGIC`, `PACKET_VERSION`, packet types.
const MAGIC: u32 = 0xdeca_fbad;
const VERSION: u32 = 1;
const TYPE_DATA: u32 = 0;
const TYPE_OOB: u32 = 2;
/// C `sizeof(PacketHeader)`.
const HEADER_LEN: usize = 16;
/// C `char buffer[0x2000]` of both copy threads.
const BUFFER_LEN: usize = 0x2000;
/// C main thread: `for (;;) { sleep(100000); }`.
const MAIN_SLEEP: Duration = Duration::from_secs(100_000);

/// Whether C `close` on a socket wakes another thread blocked in `recv` on
/// it with `EBADF` (Darwin). On Linux the other thread stays blocked until
/// its peer sends or closes; there, the `shutdown` used instead (B16) makes
/// `recv` return 0 at once, as if the peer had closed.
const CLOSE_WAKES_WITH_EBADF: bool = cfg!(target_vendor = "apple");

/// C `REPORT_ERROR()` call sites (`tcprelay.c` line numbers).
mod line {
    pub const D2H_HEADER: u32 = 174;
    pub const D2H_BODY: u32 = 191;
    pub const D2H_SEND: u32 = 197;
    pub const PACKET_HEADER: u32 = 224;
    pub const PACKET_DATA: u32 = 229;
    pub const H2D_RECV: u32 = 247;
    pub const H2D_SEND: u32 = 252;
    pub const ACCEPT: u32 = 273;
    pub const COMMAND: u32 = 285;
    pub const LISTEN: u32 = 333;
}

type Sink = Box<dyn Write + Send>;

/// State shared by all relay threads.
pub struct Relay {
    stdout: Mutex<StdioOut<Sink>>,
    stderr: Mutex<Sink>,
    /// C `serverip` and `devport`.
    host: Vec<u8>,
    devport: i32,
}

/// The two sockets of one tunnel, shared by its two threads (B16). A flag
/// is set when a thread "closes" (shuts down) a socket, for
/// [`CLOSE_WAKES_WITH_EBADF`].
struct Tunnel {
    local: TcpStream,
    remote: TcpStream,
    local_closed: AtomicBool,
    remote_closed: AtomicBool,
}

impl Relay {
    pub fn new(stdout: StdioOut<Sink>, stderr: Sink, host: &[u8], devport: i32) -> Self {
        Self {
            stdout: Mutex::new(stdout),
            stderr: Mutex::new(stderr),
            host: host.to_vec(),
            devport,
        }
    }

    /// C `printf` to `stdout`. Like a successful `printf`, it leaves
    /// `errno` unchanged (the lock may change it), so a later
    /// `REPORT_ERROR` sees the same value as in C. Write errors are ignored.
    fn print(&self, text: &[u8]) {
        let saved = sys::errno();
        {
            let mut out = self.stdout.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = out.write_all(text);
        }
        sys::set_errno(saved);
    }

    /// Writes `text` to stderr in one write (C `stderr` is unbuffered),
    /// leaving `errno` unchanged.
    fn eprint(&self, text: &[u8]) {
        let saved = sys::errno();
        {
            let mut err = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = err.write_all(text);
        }
        sys::set_errno(saved);
    }

    /// C `REPORT_ERROR()` at `tcprelay.c` line `line`, for `errno`.
    fn report_error(&self, line: u32, errno: i32) {
        let mut text = format!("src/tcprelay.c:{line}: ").into_bytes();
        text.extend_from_slice(&sys::strerror(errno));
        text.push(b'\n');
        self.eprint(&text);
    }
}

/// `errno` of a failed call.
fn errno_of(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}

/// C `startTcpRelay()` on the real stdout and stderr; never returns. Only a
/// signal ends the process.
pub fn run(forwards: &[PortPair], host: &[u8], devport: i32) -> ! {
    let stdout: Sink = Box::new(FdWriter(STDOUT_FD));
    let relay = Arc::new(Relay::new(
        StdioOut::new(stdout, sys::isatty(STDOUT_FD)),
        Box::new(FdWriter(STDERR_FD)),
        host,
        devport,
    ));
    start_services(&relay, forwards);
    loop {
        std::thread::sleep(MAIN_SLEEP);
    }
}

/// C `startTcpRelayService()`: ignores `SIGPIPE`, then starts one service
/// per pair, in order.
pub fn start_services(relay: &Arc<Relay>, forwards: &[PortPair]) {
    sys::ignore_sigpipe();
    for pair in forwards {
        // C prints the `unsigned int` ports with `%i`; `-f` parsing keeps
        // them at most `INT_MAX`, so the text is the same.
        relay.print(format!("Forwarding Port {}<->{}\n", pair.local, pair.remote).as_bytes());
        create_service(relay, pair.local as i32, pair.remote as i32);
    }
}

/// C `createService()`.
fn create_service(relay: &Arc<Relay>, local_port: i32, remote_port: i32) {
    let listener = match sys::create_listen_socket(local_port) {
        Ok(l) => l,
        Err(e) => {
            relay.report_error(line::LISTEN, errno_of(&e));
            return;
        }
    };
    relay.print(
        format!(
            "created service fd {} local: {local_port} remote {remote_port}\n",
            listener.as_raw_fd()
        )
        .as_bytes(),
    );
    let relay = Arc::clone(relay);
    let _ =
        std::thread::Builder::new().spawn(move || service_thread(&relay, &listener, remote_port));
}

/// C `serviceThread()`.
fn service_thread(relay: &Arc<Relay>, listener: &TcpListener, remote_port: i32) {
    relay.print(b"started service thread\n");
    loop {
        create_tunnel(relay, listener, remote_port);
    }
}

/// C `createTunnel()`. Dropping a socket closes it (C `error2`, `error3`).
fn create_tunnel(relay: &Arc<Relay>, listener: &TcpListener, remote_port: i32) {
    let local = match listener.accept() {
        Ok((s, _)) => s,
        Err(e) => {
            relay.report_error(line::ACCEPT, errno_of(&e));
            return;
        }
    };
    let Some(remote) = net::connect_socket(&relay.host, relay.devport) else {
        return;
    };
    let cmd = format!("connect tcp-port://{remote_port}\n").into_bytes();
    // C `printf("%s\n", cmd)`: the command's own newline, then a blank line.
    relay.print(&[&cmd[..], b"\n"].concat());
    if let Err(errno) = send_once(&remote, &cmd) {
        relay.report_error(line::COMMAND, errno);
        return;
    }
    if let Err(msg) = session::parse_response(&mut &remote) {
        relay.eprint(&msg);
        // Intentional change B9: C prints its uninitialized `response`
        // buffer after "got response "; here nothing is printed there.
        relay.print(b"error attempting to tunnel socket: got response \n");
        return;
    }
    let tunnel = Arc::new(Tunnel {
        local,
        remote,
        local_closed: AtomicBool::new(false),
        remote_closed: AtomicBool::new(false),
    });
    let copies: [fn(&Relay, &Tunnel); 2] = [device_to_host, host_to_device];
    for copy in copies {
        let (relay, tunnel) = (Arc::clone(relay), Arc::clone(&tunnel));
        let _ = std::thread::Builder::new().spawn(move || {
            // A new C thread starts with `errno` 0; the Rust thread start-up
            // may have left another value. `REPORT_ERROR` after a `recv`
            // that returned 0 prints this value.
            sys::set_errno(0);
            copy(&relay, &tunnel);
        });
    }
}

/// One C `send(sock, buf, len, 0)`. `Err` is the `errno` to report: from the
/// failed call, or the current (stale) value after a short send, as in C.
fn send_once(sock: &TcpStream, buf: &[u8]) -> Result<(), i32> {
    match sys::send(sock, buf) {
        Ok(n) if n == buf.len() => Ok(()),
        Ok(_) => Err(sys::errno()),
        Err(e) => Err(errno_of(&e)),
    }
}

/// One C `recv` on a tunnel socket. On Darwin, a socket the other thread
/// has closed gives `EBADF` (see [`CLOSE_WAKES_WITH_EBADF`]).
fn recv_tunnel(sock: &TcpStream, closed: &AtomicBool, buf: &mut [u8]) -> Result<usize, i32> {
    let r = sys::recv(sock, buf);
    if CLOSE_WAKES_WITH_EBADF && closed.load(Ordering::SeqCst) {
        return Err(libc::EBADF);
    }
    match r {
        Ok(n) => Ok(n),
        Err(e) => Err(errno_of(&e)),
    }
}

/// C `readComplete()`: fills `buf`. `Ok(0)` at end of stream, even after a
/// partial read (the partial bytes are dropped, as in C); `Err(errno)` on
/// error.
fn read_complete(sock: &TcpStream, closed: &AtomicBool, buf: &mut [u8]) -> Result<usize, i32> {
    let mut total = 0;
    while total < buf.len() {
        match recv_tunnel(sock, closed, &mut buf[total..])? {
            0 => return Ok(0),
            n => total += n,
        }
    }
    Ok(total)
}

/// Packet header field `i` (host byte order on the wire; little-endian on
/// all supported targets, as elsewhere in rovacom).
fn header_word(h: &[u8; HEADER_LEN], i: usize) -> u32 {
    u32::from_le_bytes([h[4 * i], h[4 * i + 1], h[4 * i + 2], h[4 * i + 3]])
}

/// C `deviceToHostThread()`: copies DATA packet bodies from the device to
/// the local socket. OOB ends the tunnel; other types are dropped.
fn device_to_host(relay: &Relay, t: &Tunnel) {
    relay.print(b"device to host thread started\n");
    let mut buffer = vec![0u8; BUFFER_LEN];
    'packets: loop {
        let mut header = [0u8; HEADER_LEN];
        match read_complete(&t.remote, &t.remote_closed, &mut header) {
            Err(errno) => {
                relay.report_error(line::D2H_HEADER, errno);
                break;
            }
            Ok(0) => break,
            Ok(_) => {}
        }
        let [magic, version, size, kind] = [0, 1, 2, 3].map(|i| header_word(&header, i));
        if version != VERSION || magic != MAGIC {
            relay.print(b"bad packet!\n");
            break;
        }
        let mut total: u32 = 0;
        while total < size {
            let want = (size - total).min(BUFFER_LEN as u32) as usize;
            let got = match read_complete(&t.remote, &t.remote_closed, &mut buffer[..want]) {
                Err(errno) => {
                    relay.report_error(line::D2H_BODY, errno);
                    break 'packets;
                }
                // Intentional change B15: the device closed in the middle of
                // the body. C sends 0 bytes, adds 0 to its count and loops
                // forever at full CPU; here the tunnel ends.
                Ok(0) => break 'packets,
                Ok(n) => n,
            };
            if kind == TYPE_DATA {
                if let Err(errno) = send_once(&t.local, &buffer[..got]) {
                    relay.report_error(line::D2H_SEND, errno);
                    break 'packets;
                }
            } else if kind == TYPE_OOB {
                // Usually a remote disconnect.
                break 'packets;
            }
            total += got as u32;
        }
    }
    // C `close(localSocketFd)`; B16: shut down instead.
    t.local_closed.store(true, Ordering::SeqCst);
    let _ = t.local.shutdown(Shutdown::Both);
    relay.print(b"device to host thread exited\n");
}

/// C `sendNovacomPacket()`: a DATA header, then the data, one `send` each.
/// `Err` is the `errno` after the reported failure.
fn send_packet(relay: &Relay, sock: &TcpStream, data: &[u8]) -> Result<(), i32> {
    let mut header = [0u8; HEADER_LEN];
    for (i, word) in [MAGIC, VERSION, data.len() as u32, TYPE_DATA]
        .into_iter()
        .enumerate()
    {
        header[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    for (buf, line) in [
        (&header[..], line::PACKET_HEADER),
        (data, line::PACKET_DATA),
    ] {
        if let Err(errno) = send_once(sock, buf) {
            relay.report_error(line, errno);
            return Err(errno);
        }
    }
    Ok(())
}

/// C `hostToDeviceThread()`: sends what the local socket receives as DATA
/// packets of at most 0x2000 bytes. No OOB EOF is sent at the end (as C).
fn host_to_device(relay: &Relay, t: &Tunnel) {
    relay.print(b"host to device thread started\n");
    let mut buffer = vec![0u8; BUFFER_LEN];
    loop {
        let got = match recv_tunnel(&t.local, &t.local_closed, &mut buffer) {
            // C reports end of stream with the thread's current `errno`,
            // which `recv` did not set (normally 0).
            Ok(0) => Err(sys::errno()),
            other => other,
        };
        match got {
            Err(errno) => {
                relay.report_error(line::H2D_RECV, errno);
                break;
            }
            Ok(n) => {
                if let Err(errno) = send_packet(relay, &t.remote, &buffer[..n]) {
                    relay.report_error(line::H2D_SEND, errno);
                    break;
                }
            }
        }
    }
    relay.print(b"host to device thread exited\n");
    // C `close(remoteSocketFd)`; B16: shut down instead.
    t.remote_closed.store(true, Ordering::SeqCst);
    let _ = t.remote.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (b, _) = l.accept().unwrap();
        (a, b)
    }

    fn relay() -> Relay {
        Relay::new(
            StdioOut::new(Box::new(Vec::new()), false),
            Box::new(Vec::new()),
            b"127.0.0.1",
            1,
        )
    }

    #[test]
    fn read_complete_drops_partial_data_at_eof() {
        let (mut a, b) = pair();
        let closed = AtomicBool::new(false);
        a.write_all(b"0123456789").unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(read_complete(&b, &closed, &mut buf), Ok(4));
        assert_eq!(&buf, b"0123");
        drop(a);
        let mut big = [0u8; 100];
        assert_eq!(read_complete(&b, &closed, &mut big), Ok(0));
    }

    #[test]
    fn packet_layout() {
        let (a, mut b) = pair();
        send_packet(&relay(), &a, b"xyz").unwrap();
        drop(a);
        let mut got = Vec::new();
        b.read_to_end(&mut got).unwrap();
        let mut expected = Vec::new();
        for w in [MAGIC, 1, 3, 0] {
            expected.extend_from_slice(&w.to_le_bytes());
        }
        expected.extend_from_slice(b"xyz");
        assert_eq!(got, expected);
    }

    #[test]
    fn listen_port_truncated_to_16_bits() {
        // 65536 truncates to port 0: any free port.
        let l = sys::create_listen_socket(65536).unwrap();
        let addr = l.local_addr().unwrap();
        assert!(addr.ip().is_unspecified() && addr.port() != 0);
        assert!(TcpStream::connect(("127.0.0.1", addr.port())).is_ok());
    }

    #[test]
    fn strerror_text() {
        assert_eq!(sys::strerror(libc::EBADF), b"Bad file descriptor");
        let zero: &[u8] = if cfg!(target_vendor = "apple") {
            b"Undefined error: 0"
        } else {
            b"Success"
        };
        assert_eq!(sys::strerror(0), zero);
    }
}
