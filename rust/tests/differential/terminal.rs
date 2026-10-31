//! Terminal mode (`-t`) and signal forwarding (`-s`): differential tests
//! with stdin and stdout on a pseudo-terminal and a device mock that the
//! test drives step by step.
//!
//! The pty is not the client's controlling terminal (except in
//! [`terminal_nul_key`]), so changing the window size sends no SIGWINCH;
//! the tests send signals with `kill`.

use super::control::{
    CtrlMock, PTY_TIMEOUT, SESS, auth_cmd, ctrl_args, openpty, stty, wait_child, without_pendin,
};
use super::*;
use std::io::Seek;
use std::os::fd::AsRawFd;
use std::process::Child;

/// The device mock waits at most this long for a client packet.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Window size set before the client starts, and after it is running.
const FIRST_SIZE: (u16, u16) = (37, 101);
const SECOND_SIZE: (u16, u16) = (50, 132);
/// Bytes typed on the terminal in `-t` mode: Ctrl-C, Ctrl-\, Ctrl-Z, DEL
/// and BS must reach the device as data. (Ctrl-Q/Ctrl-S are left out: IXON
/// stays on.)
const TYPED: &[u8] = b"typed \x03 \x1c \x1a \x7f\x08 x\n";

/// Device port driven by the test: one connection, read step by step.
struct LiveDevice {
    listener: TcpListener,
    port: u16,
}

impl LiveDevice {
    fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind device mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        Self { listener, port }
    }

    /// Device-list server whose only device is this port.
    fn devlist(&self) -> Mock {
        let line = format!(
            "{} 0123456789abcdef0123456789abcdef01234567 usb castle\n",
            self.port
        );
        Mock::start(&[line.as_bytes()])
    }

    /// Accepts the client and reads its command line.
    fn accept(&self) -> Conn {
        let deadline = Instant::now() + IO_TIMEOUT;
        let s = loop {
            let accepted = {
                let _fds = fd_create_lock();
                self.listener.accept()
            };
            match accepted {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client did not connect");
                    std::thread::sleep(POLL);
                }
                Err(e) => panic!("device mock accept: {e}"),
            }
        };
        s.set_nonblocking(false).expect("blocking stream");
        s.set_nodelay(true).expect("nodelay");
        s.set_read_timeout(Some(IO_TIMEOUT)).expect("timeout");
        let mut conn = Conn {
            s,
            command: Vec::new(),
        };
        let mut byte = [0u8; 1];
        while let Ok(1) = conn.s.read(&mut byte) {
            conn.command.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        conn
    }
}

/// One OOB message: (message, first word, second word).
type Oob = [u32; 3];

fn resize((rows, cols): (u16, u16)) -> Oob {
    [OOB_RESIZE, rows.into(), cols.into()]
}

fn signal(signo: libc::c_int) -> Oob {
    [OOB_SIGNAL, signo.unsigned_abs(), 0]
}

/// A client packet: OOB message or DATA payload.
#[derive(Debug, PartialEq, Eq)]
enum Packet {
    Oob(Oob),
    Data(Vec<u8>),
}

struct Conn {
    s: TcpStream,
    command: Vec<u8>,
}

impl Conn {
    fn send(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).expect("send to client");
    }

    fn packet(&mut self) -> Packet {
        let mut h = [0u8; 16];
        self.s.read_exact(&mut h).expect("client packet header");
        let word = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().expect("4 bytes"));
        assert_eq!(word(0), MAGIC);
        let mut body = vec![0u8; word(8) as usize];
        self.s.read_exact(&mut body).expect("client packet body");
        let body_word = |i: usize| u32::from_le_bytes(body[i..i + 4].try_into().expect("4 bytes"));
        match word(12) {
            T_DATA => Packet::Data(body),
            T_OOB => {
                assert_eq!(body.len(), 20);
                let mut m = [body_word(0), body_word(4), 0];
                if m[0] == OOB_RESIZE {
                    m[2] = body_word(8);
                }
                Packet::Oob(m)
            }
            k => panic!("unexpected packet type {k}"),
        }
    }

    fn oob(&mut self) -> Oob {
        match self.packet() {
            Packet::Oob(m) => m,
            p => panic!("expected an OOB packet, got {p:?}"),
        }
    }

    /// Reads DATA packets until `n` payload bytes have arrived.
    fn data(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < n {
            match self.packet() {
                Packet::Data(d) => out.extend(d),
                p => panic!("expected DATA, got {p:?}"),
            }
        }
        out
    }
}

/// A pty pair whose master output is collected by a thread.
struct Pty {
    master: File,
    slave: File,
    seen: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
}

impl Pty {
    fn open() -> Self {
        let (master, slave) = openpty();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let mut m = master.try_clone().expect("clone master");
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                // EOF or EIO once every slave fd is closed.
                while let Ok(n @ 1..) = m.read(&mut buf) {
                    seen.lock().expect("pty lock").extend_from_slice(&buf[..n]);
                }
            })
        };
        Self {
            master,
            slave,
            seen,
            reader,
        }
    }

    fn slave(&self) -> Stdio {
        Stdio::from(self.slave.try_clone().expect("clone slave"))
    }

    fn set_size(&self, (rows, cols): (u16, u16)) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the slave fd is open; `ws` is a live `winsize` that
        // TIOCSWINSZ only reads.
        let rc = unsafe { libc::ioctl(self.slave.as_raw_fd(), libc::TIOCSWINSZ, &raw const ws) };
        assert_eq!(rc, 0, "TIOCSWINSZ: {}", std::io::Error::last_os_error());
    }

    fn termios(&self) -> libc::termios {
        // SAFETY: all-zero bytes are a valid `termios`.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: the slave fd is open; `t` is a live `termios`.
        let rc = unsafe { libc::tcgetattr(self.slave.as_raw_fd(), &raw mut t) };
        assert_eq!(rc, 0, "tcgetattr: {}", std::io::Error::last_os_error());
        t
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("write to pty master");
    }

    /// Closes the slave and returns everything read from the master.
    fn finish(self) -> Vec<u8> {
        drop(self.slave);
        self.reader.join().expect("pty reader");
        self.seen.lock().expect("pty lock").clone()
    }
}

/// A spawned client with its stdout (if piped) and stderr collected.
struct Client {
    child: Child,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: JoinHandle<Vec<u8>>,
    what: String,
}

impl Client {
    /// Spawns `bin`; stdout is piped if `stdout` is `None`. With `ctty`, the
    /// child starts a new session with its stdin as controlling terminal.
    fn spawn(
        bin: &Path,
        args: &[Vec<u8>],
        stdin: Stdio,
        stdout: Option<Stdio>,
        ctty: bool,
    ) -> Self {
        let mut cmd = Command::new(bin);
        cmd.arg0("rovacom")
            .args(args.iter().map(|a| OsString::from_vec(a.clone())))
            .stdin(stdin)
            .stdout(stdout.unwrap_or_else(Stdio::piped))
            .stderr(Stdio::piped())
            .env_remove("POSIXLY_CORRECT");
        if ctty {
            // SAFETY: the closure runs in the child after fork and calls
            // only `setsid` and `ioctl`, which are async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let spawned = {
            let _spawn = spawn_lock();
            cmd.spawn()
        };
        let mut child = spawned.unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
        drop(cmd);
        Self {
            stdout: child.stdout.take().map(spawn_reader),
            stderr: spawn_reader(child.stderr.take().expect("stderr pipe")),
            child,
            what: bin.display().to_string(),
        }
    }

    fn kill(&self, signo: libc::c_int) {
        let pid = libc::pid_t::try_from(self.child.id()).expect("pid");
        // SAFETY: `kill` only sends a signal to our own child process.
        let rc = unsafe { libc::kill(pid, signo) };
        assert_eq!(rc, 0, "kill: {}", std::io::Error::last_os_error());
    }

    fn assert_running(&mut self) {
        assert!(
            self.child.try_wait().expect("try_wait").is_none(),
            "{} exited",
            self.what
        );
    }

    /// Waits for exit: (status, stdout, stderr).
    fn finish(mut self) -> (String, Vec<u8>, Vec<u8>) {
        let status = wait_child(&mut self.child, &self.what);
        let out = self
            .stdout
            .map(|h| h.join().expect("stdout"))
            .unwrap_or_default();
        (status, out, self.stderr.join().expect("stderr"))
    }
}

fn args(parts: &[&str], port: u16) -> Vec<Vec<u8>> {
    let mut all = vec!["-a", "127.0.0.1", "-p", "{P}"];
    all.extend_from_slice(parts);
    argv(&all, port)
}

// ---------------------------------------------------------------------------
// `-t` on a pty.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct TermOutcome {
    status: String,
    stderr: Vec<u8>,
    master: Vec<u8>,
    command: Vec<u8>,
    /// `stty -g` while the client is in its data loop.
    during: String,
    oob: Vec<Oob>,
    typed: Vec<u8>,
    /// `stty -g` after exit.
    after: String,
}

/// `-t open tty://0` with stdin and stdout on a pty: terminal attributes,
/// initial RESIZE, typed bytes, SIGWINCH, output and exit.
fn term_run(bin: &Path) -> TermOutcome {
    let dev = LiveDevice::bind();
    let list = dev.devlist();
    let mut pty = Pty::open();
    pty.set_size(FIRST_SIZE);
    let before = stty(&pty.slave);
    let mut client = Client::spawn(
        bin,
        &args(&["-t", "open", "tty://0"], list.port),
        pty.slave(),
        Some(pty.slave()),
        false,
    );
    // The client sets the terminal mode before it sends the command.
    let mut conn = dev.accept();
    let during = stty(&pty.slave);
    let t = pty.termios();
    assert_eq!(t.c_lflag, libc::ISIG, "{}", client.what);
    for i in [libc::VINTR, libc::VQUIT, libc::VSUSP] {
        assert_eq!(t.c_cc[i], 0, "c_cc[{i}], {}", client.what);
    }
    conn.send(b"ok\n");
    let mut oob = vec![conn.oob()];
    let mut typed_all = TYPED.to_vec();
    if cfg!(target_os = "linux") {
        // NUL is `_POSIX_VDISABLE` on Linux: plain data.
        typed_all.extend_from_slice(b"\0nul\0");
    }
    pty.type_bytes(&typed_all);
    let typed = conn.data(typed_all.len());
    pty.set_size(SECOND_SIZE);
    client.kill(libc::SIGWINCH);
    oob.push(conn.oob());
    client.assert_running();
    conn.send(&pkt(T_DATA, b"out\n"));
    conn.send(&pkt(T_ERR, b"err\n"));
    conn.send(&ret(0));
    let (status, _, stderr) = client.finish();
    let after = stty(&pty.slave);
    assert_eq!(
        without_pendin(&after),
        before,
        "terminal attributes not restored by {bin:?}"
    );
    drop(list);
    TermOutcome {
        status,
        stderr,
        master: pty.finish(),
        command: conn.command,
        during,
        oob,
        typed,
        after,
    }
}

#[test]
fn terminal_mode_on_pty() {
    let c = term_run(&reference());
    let r = term_run(&rovacom());
    assert_eq!(r, c);
    assert_eq!(r.status, "exit 0");
    assert_eq!(r.command, b"open tty://0\n");
    assert_eq!(r.oob, [resize(FIRST_SIZE), resize(SECOND_SIZE)]);
    let mut expected = TYPED.to_vec();
    if cfg!(target_os = "linux") {
        expected.extend_from_slice(b"\0nul\0");
    }
    assert_eq!(r.typed, expected);
    // No echo; output processing (ONLCR) is unchanged.
    assert_eq!(r.master, b"out\r\n");
    assert_eq!(r.stderr, b"err\n");
}

/// NUL typed in `-t` mode with the pty as controlling terminal. On macOS
/// NUL is the interrupt key (platform quirk P2): without `-s` the client is
/// killed by SIGINT, as C. On Linux NUL is data.
fn nul_run(bin: &Path) -> (String, Vec<u8>, Vec<u8>, Oob) {
    let dev = LiveDevice::bind();
    let list = dev.devlist();
    let mut pty = Pty::open();
    let client = Client::spawn(
        bin,
        &args(&["-t", "run", "x"], list.port),
        pty.slave(),
        Some(pty.slave()),
        true,
    );
    let mut conn = dev.accept();
    conn.send(b"ok\n");
    let first = conn.oob();
    pty.type_bytes(b"ab\x03");
    let mut typed = conn.data(3);
    pty.type_bytes(b"\0");
    if cfg!(target_os = "linux") {
        typed.extend(conn.data(1));
        conn.send(&ret(0));
    }
    let (status, _, stderr) = client.finish();
    drop(list);
    // The terminal attributes are not checked: on macOS, the exit of the
    // session leader revokes the terminal.
    drop(pty.finish());
    (status, stderr, typed, first)
}

#[test]
fn terminal_nul_key() {
    let c = nul_run(&reference());
    let r = nul_run(&rovacom());
    assert_eq!(r, c);
    assert_eq!(r.3[0], OOB_RESIZE);
    if cfg!(target_os = "macos") {
        assert_eq!((r.0.as_str(), r.2.as_slice()), ("signal 2", &b"ab\x03"[..]));
    } else {
        assert_eq!((r.0.as_str(), r.2.as_slice()), ("exit 0", &b"ab\x03\0"[..]));
    }
}

// ---------------------------------------------------------------------------
// `-s` signal forwarding.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct SigOutcome {
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    oob: Vec<Oob>,
}

/// `-s` with stdin on a pty (so the C loop sits in `select`) and stdout a
/// pipe (no window size: 0x0). Each signal is sent after the previous one
/// has been forwarded; the client must survive each.
fn sig_run(bin: &Path) -> SigOutcome {
    let dev = LiveDevice::bind();
    let list = dev.devlist();
    let pty = Pty::open();
    pty.set_size(FIRST_SIZE);
    let mut client = Client::spawn(
        bin,
        &args(&["-s", "run", "x"], list.port),
        pty.slave(),
        None,
        false,
    );
    let mut conn = dev.accept();
    conn.send(b"ok\n");
    let mut oob = vec![conn.oob()];
    for signo in [
        libc::SIGINT,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGWINCH,
    ] {
        client.kill(signo);
        oob.push(conn.oob());
        client.assert_running();
    }
    conn.send(&ret(0));
    let (status, stdout, stderr) = client.finish();
    drop(list);
    drop(pty.finish());
    SigOutcome {
        status,
        stdout,
        stderr,
        oob,
    }
}

#[test]
fn signals_forwarded() {
    let c = sig_run(&reference());
    let r = sig_run(&rovacom());
    assert_eq!(r, c);
    assert_eq!(
        r.oob,
        [
            resize((0, 0)),
            signal(libc::SIGINT),
            signal(libc::SIGHUP),
            signal(libc::SIGQUIT),
            signal(libc::SIGTERM),
            resize((0, 0)),
        ]
    );
    assert_eq!(r.status, "exit 0");
    assert!(r.stdout.is_empty() && r.stderr.is_empty());
}

/// Size of the stdin file for [`signals_while_blocked_rust_only`]: larger
/// than the loopback socket buffers, so the client blocks on a write.
const BLOCKING_STDIN: usize = 32 * 1024 * 1024;
/// The client is taken as blocked when its stdin offset has not moved for
/// this many polls.
const STALL_POLLS: usize = 3;
const STALL_POLL: Duration = Duration::from_millis(100);
/// Time for the signals sent to reach the client.
const SIGNAL_SETTLE: Duration = Duration::from_millis(200);

/// Signals sent while the client is blocked writing a packet (the device
/// does not read) are all forwarded once the write completes: each distinct
/// signal once, lowest number first. Rust only: C reads its signal pipe
/// only when a stale `errno` is EINTR (bug B4). Here it is not (ENOTTY
/// observed on macOS), so C prints "data_xfer: Error reading from signal
/// pipe" one or more times and forwards none of them.
#[test]
fn signals_while_blocked_rust_only() {
    let data: Vec<u8> = (0..BLOCKING_STDIN).map(|i| (i % 253) as u8).collect();
    let file = StdinFile::new(&data);
    let stdin = File::open(&file.0).expect("open stdin file");
    // Shares the file offset with the child's stdin.
    let mut offset = stdin.try_clone().expect("clone stdin");
    let dev = LiveDevice::bind();
    let list = dev.devlist();
    let mut client = Client::spawn(
        &rovacom(),
        &args(&["-s", "run", "x"], list.port),
        Stdio::from(stdin),
        None,
        false,
    );
    let mut conn = dev.accept();
    conn.send(b"ok\n");
    // Wait until the client stops reading stdin: it is blocked on a write.
    let deadline = Instant::now() + PTY_TIMEOUT;
    let (mut last, mut still) = (u64::MAX, 0);
    while still < STALL_POLLS {
        assert!(Instant::now() < deadline, "client never blocked");
        std::thread::sleep(STALL_POLL);
        let pos = offset.stream_position().expect("stdin offset");
        still = if pos == last { still + 1 } else { 0 };
        last = pos;
    }
    assert!(last < BLOCKING_STDIN as u64, "stdin fully read");
    for signo in [
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGWINCH,
        libc::SIGINT,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGINT,
        libc::SIGTERM,
    ] {
        client.kill(signo);
    }
    std::thread::sleep(SIGNAL_SETTLE);
    client.assert_running();
    let mut packets = Vec::new();
    loop {
        let p = conn.packet();
        let eof = p == Packet::Oob([OOB_EOF, 0, 0]);
        packets.push(p);
        if eof {
            break;
        }
    }
    conn.send(&ret(0));
    let (status, _, stderr) = client.finish();
    drop(list);
    assert_eq!((status.as_str(), stderr.as_slice()), ("exit 0", &b""[..]));
    let payload: Vec<u8> = packets
        .iter()
        .filter_map(|p| match p {
            Packet::Data(d) => Some(d.as_slice()),
            Packet::Oob(_) => None,
        })
        .flatten()
        .copied()
        .collect();
    assert!(payload == data, "uploaded payload differs");
    let oob: Vec<(usize, Oob)> = packets
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match p {
            Packet::Oob(m) => Some((i, *m)),
            Packet::Data(_) => None,
        })
        .collect();
    let messages: Vec<Oob> = oob.iter().map(|&(_, m)| m).collect();
    assert_eq!(
        messages,
        [
            resize((0, 0)),
            signal(libc::SIGHUP),
            signal(libc::SIGINT),
            signal(libc::SIGQUIT),
            signal(libc::SIGTERM),
            resize((0, 0)),
            [OOB_EOF, 0, 0],
        ]
    );
    // The five forwarded signals follow each other with no DATA between.
    let first = oob[1].0;
    let idx: Vec<usize> = oob[1..6].iter().map(|&(i, _)| i).collect();
    assert_eq!(idx, (first..first + 5).collect::<Vec<_>>());
}

// ---------------------------------------------------------------------------
// `-c` with `-t -s` on a pty.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct CtrlTermOutcome {
    status: String,
    stderr: Vec<u8>,
    master: Vec<u8>,
    sent: Vec<Vec<u8>>,
    after: String,
}

/// `-c login -r secret -t -s` with stdin and stdout on a pty. Port 6971 is
/// held by the control mock (see the `control` module safety rule).
fn ctrl_term_run(bin: &Path) -> CtrlTermOutcome {
    let ctrl = CtrlMock::start(Some(b"ok\n"));
    let mock = Mock::start(&[SESS]);
    let pty = Pty::open();
    let before = stty(&pty.slave);
    let client = Client::spawn(
        bin,
        &ctrl_args(&["-c", "login", "-r", "secret", "-t", "-s"], mock.port),
        pty.slave(),
        Some(pty.slave()),
        false,
    );
    let (status, _, stderr) = client.finish();
    let after = stty(&pty.slave);
    assert_eq!(
        without_pendin(&after),
        before,
        "terminal attributes not restored by {bin:?}"
    );
    drop(mock);
    CtrlTermOutcome {
        status,
        stderr,
        master: pty.finish(),
        sent: ctrl.finish(),
        after,
    }
}

#[test]
fn ctrl_terminal_mode() {
    let c = ctrl_term_run(&reference());
    let r = ctrl_term_run(&rovacom());
    assert_eq!(r, c);
    assert_eq!(
        (r.status.as_str(), r.stderr.as_slice()),
        ("exit 0", &b""[..])
    );
    assert_eq!(r.sent, [auth_cmd("login")]);
}
