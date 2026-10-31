//! Control commands (`-c`): differential tests against a mock control port.
//!
//! Safety rule: `-c` clients connect to the fixed port 6971 on the server
//! host, where a real novacomd with an attached device may listen. Every
//! run of a client with `-c` first binds 127.0.0.1:6971 itself
//! ([`CtrlMock::start`]) and keeps it bound until the client has exited; if
//! the bind fails the test panics before any client is spawned. A
//! process-wide lock serializes all users of the port. The "refused" case
//! re-checks that the port is free (by binding it) just before each spawn.
//!
//! On macOS, `SO_REUSEADDR` (set by std) lets 127.0.0.1:6971 be bound while
//! a wildcard listener exists; the more specific socket (the mock) then gets
//! every connection to 127.0.0.1, so the client still never reaches the
//! other listener. On Linux the bind fails in that case. (Both checked with
//! a wildcard listener on another port.)
//!
//! The listener and its connections are created under `FD_LOCK`, so no
//! concurrently spawned child inherits them and keeps the port bound.

use super::*;
use std::fs::OpenOptions;
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::ExitStatus;
use std::sync::MutexGuard;

const CTRL_PORT: u16 = 6971;
const CTRL_HOST: &str = "127.0.0.1";
/// The mock reads until the client has been quiet this long.
const QUIET: Duration = Duration::from_millis(200);
/// Upper bound for waiting on pty output.
const PTY_TIMEOUT: Duration = Duration::from_secs(10);

static CTRL_LOCK: Mutex<()> = Mutex::new(());

fn ctrl_lock() -> MutexGuard<'static, ()> {
    // A panic in another test must not block the rest.
    CTRL_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn bind_ctrl_or_panic() -> TcpListener {
    // A bind alone is not enough: on macOS, SO_REUSEADDR lets this bind
    // succeed next to a wildcard listener (a real novacomd), and the refused
    // test drops the mock before spawning the client. Refuse if anything
    // accepts a connection on the port.
    if TcpStream::connect((CTRL_HOST, CTRL_PORT)).is_ok() {
        panic!("port 6971 accepts connections — is novacomd running? refusing to run");
    }
    let _fds = fd_create_lock();
    TcpListener::bind((CTRL_HOST, CTRL_PORT))
        .unwrap_or_else(|e| panic!("port 6971 busy — is novacomd running? refusing to run ({e})"))
}

/// Mock control port: holds the lock and 127.0.0.1:6971. Each connection
/// is read until [`QUIET`], then gets `reply` (or just a close for `None`).
pub(super) struct CtrlMock {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<Vec<u8>>>>,
    // Dropped last: the port stays reserved until the thread has stopped.
    _guard: MutexGuard<'static, ()>,
}

impl CtrlMock {
    pub(super) fn start(reply: Option<&[u8]>) -> Self {
        let guard = ctrl_lock();
        let listener = bind_ctrl_or_panic();
        listener.set_nonblocking(true).expect("nonblocking");
        let reply = reply.map(<[u8]>::to_vec);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut conns = Vec::new();
            while !stop2.load(Ordering::Relaxed) {
                let accepted = {
                    let _fds = fd_create_lock();
                    listener.accept()
                };
                match accepted {
                    Ok((s, _)) => conns.push(serve_ctrl(s, reply.as_deref())),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(POLL)
                    }
                    Err(e) => panic!("control mock accept: {e}"),
                }
            }
            conns
        });
        Self {
            stop,
            handle: Some(handle),
            _guard: guard,
        }
    }

    /// Stops the mock; returns the bytes received on each connection.
    pub(super) fn finish(mut self) -> Vec<Vec<u8>> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .map(|h| h.join().expect("control mock thread"))
            .unwrap_or_default()
    }
}

impl Drop for CtrlMock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve_ctrl(mut s: TcpStream, reply: Option<&[u8]>) -> Vec<u8> {
    s.set_nonblocking(false).expect("blocking stream");
    s.set_read_timeout(Some(QUIET)).expect("timeout");
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break, // quiet for QUIET (or reset)
        }
    }
    if let Some(r) = reply {
        let _ = s.write_all(r);
    }
    got
}

fn status_string(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        (None, None) => "unknown".to_string(),
    }
}

fn wait_child(child: &mut std::process::Child, what: &str) -> String {
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        if let Some(s) = child.try_wait().expect("wait") {
            return status_string(s);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{what} timed out");
        }
        std::thread::sleep(POLL);
    }
}

const NDUID: &str = "FEDCBA9876543210FEDCBA9876543210FEDCBA98";
/// Device with a session id; its port is never used by `-c`.
const SESS: &[u8] = b"7000 FEDCBA9876543210FEDCBA9876543210FEDCBA98 tcp Topaz-Linux sess123456\n";
const NOSESS: &[u8] = b"7000 0123456789abcdef0123456789abcdef01234567 usb castle-linux\n";
/// `hex(SHA1(hex(SHA1(NDUID "secret")) "sess123456"))`, computed with `shasum -a 1`.
const SECRET_HASH: &str = "3d43c4aba217ee365d9bdd4af58f0a3a83d16797";

fn auth_cmd(name: &str) -> Vec<u8> {
    format!("{name} dev://{NDUID} {SECRET_HASH}").into_bytes()
}

/// `-a 127.0.0.1 -p {P}` followed by `args`.
fn ctrl_args(args: &[&str], port: u16) -> Vec<Vec<u8>> {
    let mut parts = vec!["-a", CTRL_HOST, "-p", "{P}"];
    parts.extend_from_slice(args);
    argv(&parts, port)
}

/// Runs `bin` with a device-list mock and the control mock.
fn ctrl_run(
    bin: &Path,
    devlist: &[&[u8]],
    args: &[&str],
    stdin: &[u8],
    reply: Option<&[u8]>,
) -> (Outcome, Vec<Vec<u8>>) {
    let ctrl = CtrlMock::start(reply);
    let mock = Mock::start(devlist);
    let stdin = StdinFile::new(stdin);
    let o = run_io(
        bin,
        "rovacom",
        &ctrl_args(args, mock.port),
        false,
        stdin.open(),
        StdoutMode::Normal,
    );
    drop(mock);
    (o, ctrl.finish())
}

/// Runs both binaries; asserts identical outcome and control-port bytes.
fn ctrl_check(
    devlist: &[&[u8]],
    args: &[&str],
    stdin: &[u8],
    reply: Option<&[u8]>,
) -> (Outcome, Vec<Vec<u8>>) {
    let (c, c_sent) = ctrl_run(&reference(), devlist, args, stdin, reply);
    let c = normalize_c(c, false);
    let (r, r_sent) = ctrl_run(&rovacom(), devlist, args, stdin, reply);
    let what = format!("args {args:?}, stdin {:?}", String::from_utf8_lossy(stdin));
    assert_eq!(r, c, "{what}");
    assert_eq!(r_sent, c_sent, "control bytes, {what}");
    (r, r_sent)
}

fn ok(o: &Outcome) {
    assert_eq!(
        (o.status.as_str(), o.stderr.as_slice()),
        ("exit 0", &b""[..])
    );
}

fn fails(o: &Outcome, stderr: &str) {
    assert_eq!(
        (o.status.as_str(), o.stderr.as_slice()),
        ("exit 1", stderr.as_bytes())
    );
}

const PROMPT: &[u8] = b"Please enter password: \n";

#[test]
fn ctrl_list_and_logout() {
    let logout = format!("logout dev://{NDUID}\n").into_bytes();
    let logout_nosess = b"logout dev://0123456789abcdef0123456789abcdef01234567\n".to_vec();
    for (dev, logout) in [(SESS, &logout), (NOSESS, &logout_nosess)] {
        for cmd in ["list", "listfoo"] {
            let (o, sent) = ctrl_check(&[dev], &["-c", cmd], b"", Some(b"ok\n"));
            ok(&o);
            assert!(o.stdout.is_empty());
            assert_eq!(sent, [b"list host://".to_vec()]);
        }
        for cmd in ["logout", "logoutnow"] {
            let (o, sent) = ctrl_check(&[dev], &["-c", cmd], b"", Some(b"ok\n"));
            ok(&o);
            assert_eq!(sent, [logout.as_slice()]);
        }
    }
    // B12: the list body is never read.
    let (o, _) = ctrl_check(&[NOSESS], &["-c", "list"], b"", Some(b"ok\nhost1\nhost2\n"));
    ok(&o);
    assert!(o.stdout.is_empty());
}

#[test]
fn ctrl_auth_with_password_option() {
    for (cmd, name) in [
        ("login", "login"),
        ("loginxyz", "login"),
        ("add", "add"),
        ("addme", "add"),
        ("remove", "remove"),
        ("removeall", "remove"),
    ] {
        let (o, sent) = ctrl_check(&[SESS], &["-c", cmd, "-r", "secret"], b"", Some(b"ok\n"));
        ok(&o);
        assert!(o.stdout.is_empty());
        assert_eq!(sent, [auth_cmd(name)], "{cmd}");
    }
    // `-d` selects the device with the session id.
    let both = [NOSESS, SESS].concat();
    let (o, sent) = ctrl_check(
        &[&both],
        &["-d", "topaz-linux", "--cmd=login", "--password", "secret"],
        b"",
        Some(b"ok\n"),
    );
    ok(&o);
    assert_eq!(sent, [auth_cmd("login")]);
    // Empty -r with a session id.
    let (o, sent) = ctrl_check(&[SESS], &["-c", "login", "-r", ""], b"", Some(b"ok\n"));
    fails(&o, "Please specify password\n");
    assert!(o.stdout.is_empty() && sent.is_empty());
    // No session id: with or without -r, and no prompt.
    for args in [
        &["-c", "login", "-r", "secret"][..],
        &["-c", "add"],
        &["-c", "remove", "-r", ""],
    ] {
        let (o, sent) = ctrl_check(&[NOSESS], args, b"secret\n", Some(b"ok\n"));
        fails(&o, "Device does not require authentication\n");
        assert!(o.stdout.is_empty() && sent.is_empty(), "{args:?}");
    }
}

#[test]
fn ctrl_password_from_stdin() {
    let seventy = "q".repeat(70);
    let sixty_four = format!("{}\n", "z".repeat(64));
    // (stdin, the password is non-empty)
    let cases: [(&[u8], bool); 9] = [
        (b"secret\n", true),
        (b"   \t\n secret more\n", true),
        (b"secret", true),
        (seventy.as_bytes(), true),
        (sixty_four.as_bytes(), true),
        (b"ab\0cd\n", true),
        (b"\xffpw\x80\n", true),
        (b"", false),
        (b"\0abc\n", false),
    ];
    for (stdin, non_empty) in cases {
        let (o, sent) = ctrl_check(&[SESS], &["-c", "login"], stdin, Some(b"ok\n"));
        assert_eq!(o.stdout, PROMPT);
        if non_empty {
            ok(&o);
            assert_eq!(sent.len(), 1);
        } else {
            fails(&o, "Please specify password\n");
            assert!(sent.is_empty());
        }
        if stdin.starts_with(b"secret") || stdin.starts_with(b" ") {
            assert_eq!(sent, [auth_cmd("login")]);
        }
    }
}

#[test]
fn ctrl_responses() {
    let replies: [(Option<&[u8]>, &str); 7] = [
        (Some(b"ok\n"), ""),
        (Some(b"ok\0junk\n"), ""),
        (Some(b"req:auth\n"), "password required\n"),
        (Some(b"unrecognized command login\n"), "unknown command\n"),
        (
            Some(b"error: bad hash\n"),
            "bad or error response from other side: 'error: bad hash'\n",
        ),
        (Some(b"\n"), "bad or error response from other side: ''\n"),
        (None, "bad or error response from other side: ''\n"),
    ];
    for args in [
        &["-c", "list"][..],
        &["-c", "logout"],
        &["-c", "login", "-r", "secret"],
        &["-c", "remove"],
    ] {
        for (reply, err) in replies {
            let (o, sent) = ctrl_check(&[SESS], args, b"secret\n", reply);
            assert_eq!(sent.len(), 1, "{args:?} {reply:?}");
            if err.is_empty() {
                ok(&o);
            } else {
                fails(&o, err);
            }
        }
    }
}

#[test]
fn ctrl_unsupported_and_no_device() {
    for cmd in ["", "bogus", "log", "logou", "LIST", " list", "lis"] {
        let (o, sent) = ctrl_check(&[SESS], &["-c", cmd, "-r", "secret"], b"", Some(b"ok\n"));
        fails(&o, &format!("unsupported command({cmd})\n"));
        assert!(sent.is_empty());
    }
    let (o, sent) = ctrl_check(&[b""], &["-c", "list"], b"", Some(b"ok\n"));
    fails(&o, "unable to find device\n");
    assert!(sent.is_empty());
    let (o, sent) = ctrl_check(
        &[SESS],
        &["-d", "nomatch", "-c", "login"],
        b"",
        Some(b"ok\n"),
    );
    fails(&o, "unable to find device\n");
    assert!(sent.is_empty());
}

#[test]
fn ctrl_operands_and_list_flag() {
    let (o, sent) = ctrl_check(
        &[SESS],
        &["-c", "list", "run", "file:///bin/ls"],
        b"",
        Some(b"ok\n"),
    );
    ok(&o);
    assert_eq!(sent, [b"list host://".to_vec()]);
    let (o, sent) = ctrl_check(
        &[SESS],
        &["run", "-c", "login", "x", "-r", "secret"],
        b"",
        Some(b"ok\n"),
    );
    ok(&o);
    assert_eq!(sent, [auth_cmd("login")]);
    // -l wins: the list is printed and port 6971 is never used.
    let (o, sent) = ctrl_check(&[SESS], &["-l", "-c", "list"], b"", Some(b"ok\n"));
    assert_eq!((o.status.as_str(), o.stdout.as_slice()), ("exit 0", SESS));
    assert!(sent.is_empty());
}

/// With `-w`, a matching device with a negative port leaves its session id
/// in the C global; the next match without one inherits it.
#[test]
fn ctrl_wait_keeps_earlier_session_id() {
    let first: &[u8] = b"-5 AAAA usb castle sessA\n";
    let second: &[u8] = b"7000 BBBB usb castle\n";
    let (o, sent) = ctrl_check(
        &[first, second],
        &["-w", "-c", "login", "-r", "pw"],
        b"",
        Some(b"ok\n"),
    );
    ok(&o);
    let hash = rovacom::control::auth_hash(b"BBBB", b"pw", b"sessA");
    assert_eq!(sent, [cat(&[b"login dev://BBBB ", &hash])]);
}

#[test]
fn ctrl_refused() {
    let _guard = ctrl_lock();
    let mut outcomes = Vec::new();
    for (bin, is_c) in [(reference(), true), (rovacom(), false)] {
        let mock = Mock::start(&[SESS]);
        // Re-check just before spawning: the port must be free, and it is
        // left unbound so the connect is refused.
        drop(bind_ctrl_or_panic());
        let o = run(
            &bin,
            "rovacom",
            &ctrl_args(&["-c", "login", "-r", "secret"], mock.port),
            false,
        );
        outcomes.push(if is_c { normalize_c(o, false) } else { o });
    }
    assert_eq!(outcomes[1], outcomes[0]);
    fails(&outcomes[1], "failed to connect to server\n");
}

/// `-c` with `-t` or `-s` is not implemented yet (C would connect).
#[test]
fn ctrl_with_terminal_or_signals_rust_only() {
    for (args, what) in [
        (&["-c", "login", "-t"][..], "terminal mode"),
        (&["-s", "-c", "list"], "signal forwarding"),
        (&["-ts", "-c", "bogus"], "terminal mode"),
    ] {
        let (o, sent) = ctrl_run(&rovacom(), &[SESS], args, b"", Some(b"ok\n"));
        assert_eq!(o.status, "exit 70");
        assert_eq!(
            o.stderr,
            format!("rovacom: {what} is not implemented in this build\n").into_bytes()
        );
        assert!(o.stdout.is_empty() && sent.is_empty());
    }
}

/// B14: C hashes past its 4096-byte buffer for long passwords (undefined
/// behavior); rovacom hashes the full input. Expected value from `shasum`.
#[test]
fn ctrl_long_password_rust_only() {
    let long = "p".repeat(5000);
    let (o, sent) = ctrl_run(
        &rovacom(),
        &[b"7000 n1 usb castle s1\n"],
        &["-c", "login", "-r", &long],
        b"",
        Some(b"ok\n"),
    );
    ok(&o);
    assert_eq!(
        sent,
        [b"login dev://n1 083ddf9f63c7e31fe4b80d9ea141aaf9b825cf76".to_vec()]
    );
}

// ---------------------------------------------------------------------------
// stdout and stderr merged into one pipe.
// ---------------------------------------------------------------------------

static FIFO_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Runs `bin` with stdout and stderr on one named pipe; returns the exit
/// status and everything read from the pipe.
fn run_merged(bin: &Path, args: &[Vec<u8>], stdin: Stdio) -> (String, Vec<u8>) {
    let fifo = std::env::temp_dir().join(format!(
        "rovacom-fifo-{}-{}",
        std::process::id(),
        FIFO_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let made = {
        let _spawn = spawn_lock();
        Command::new("mkfifo").arg(&fifo).status()
    }
    .expect("run mkfifo");
    assert!(made.success(), "mkfifo failed");
    let reader = {
        let path = fifo.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            File::open(&path)
                .expect("open fifo for reading")
                .read_to_end(&mut buf)
                .expect("read fifo");
            buf
        })
    };
    // Blocks until the reader has opened its end.
    let w = OpenOptions::new()
        .write(true)
        .open(&fifo)
        .expect("open fifo for writing");
    let w2 = w.try_clone().expect("clone fifo");
    let mut cmd = Command::new(bin);
    cmd.arg0("rovacom")
        .args(args.iter().map(|a| OsString::from_vec(a.clone())))
        .stdin(stdin)
        .stdout(Stdio::from(w))
        .stderr(Stdio::from(w2))
        .env_remove("POSIXLY_CORRECT");
    let spawned = {
        let _spawn = spawn_lock();
        cmd.spawn()
    };
    let mut child = spawned.expect("spawn");
    // The Command holds copies of the write end; the reader sees EOF only
    // after they are closed.
    drop(cmd);
    let status = wait_child(&mut child, &bin.display().to_string());
    let bytes = reader.join().expect("fifo reader");
    let _ = std::fs::remove_file(&fifo);
    (status, bytes)
}

/// Normalizes merged C output like [`normalize_c`].
fn normalize_merged(bytes: &[u8]) -> Vec<u8> {
    let o = normalize_c(
        Outcome {
            status: String::new(),
            stdout: bytes.to_vec(),
            stderr: Vec::new(),
        },
        false,
    );
    if cfg!(target_os = "macos") {
        replace_at_line_start(&o.stdout, b"novacom: ", b"rovacom: ")
    } else {
        o.stdout
    }
}

/// Merged-stream run of both binaries, with the control mock held.
fn merged_check(devlist: &[&[u8]], args: &[&str], stdin: &[u8], reply: Option<&[u8]>) -> Vec<u8> {
    let mut results = Vec::new();
    for (bin, is_c) in [(reference(), true), (rovacom(), false)] {
        let ctrl = CtrlMock::start(reply);
        let mock = Mock::start(devlist);
        let file = StdinFile::new(stdin);
        let (status, mut bytes) = run_merged(&bin, &ctrl_args(args, mock.port), file.open());
        if is_c {
            bytes = normalize_merged(&bytes);
        }
        results.push((status, bytes, ctrl.finish()));
    }
    let what = format!("merged, args {args:?}");
    assert_eq!(
        String::from_utf8_lossy(&results[1].1),
        String::from_utf8_lossy(&results[0].1),
        "{what}"
    );
    assert_eq!(results[1], results[0], "{what}");
    results.swap_remove(1).1
}

#[test]
fn merged_streams() {
    // stdout is a pipe, so the prompt waits in the stdio buffer and stderr
    // comes first.
    let b = merged_check(&[SESS], &["-c", "login"], b"", Some(b"ok\n"));
    assert_eq!(b, cat(&[b"Please specify password\n", PROMPT]));
    let b = merged_check(&[SESS], &["-c", "add"], b"secret\n", Some(b"req:auth\n"));
    assert_eq!(b, cat(&[b"password required\n", PROMPT]));
    let b = merged_check(&[SESS], &["-c", "remove"], b"secret\n", Some(b"ok\n"));
    assert_eq!(b, PROMPT);
    let b = merged_check(&[SESS], &["-c", "login"], b"secret\n", None);
    assert_eq!(
        b,
        cat(&[b"bad or error response from other side: ''\n", PROMPT])
    );
    let b = merged_check(&[NOSESS], &["-c", "login"], b"", Some(b"ok\n"));
    assert_eq!(b, b"Device does not require authentication\n");
    // Paths without -c (the control mock is held anyway).
    let b = merged_check(&[SESS], &["-Z"], b"", None);
    assert!(b.windows(6).any(|w| w == b"usage:"));
    merged_check(&[SESS], &["-l", "-a"], b"", None);
    let b = merged_check(&[SESS, NOSESS], &["-l"], b"", None);
    assert_eq!(b, SESS);
    merged_check(&[SESS], &[], b"", None);
    merged_check(&[b""], &["x"], b"", None);
}

// ---------------------------------------------------------------------------
// Password prompt on a pseudo-terminal.
// ---------------------------------------------------------------------------

/// Opens a pty pair; both fds are close-on-exec.
fn openpty() -> (File, File) {
    // openpty fds are inheritable until replaced by close-on-exec dups.
    let _fds = fd_create_lock();
    let (mut master, mut slave): (libc::c_int, libc::c_int) = (-1, -1);
    // SAFETY: the two out-pointers point at live `c_int`s; name, termios
    // and window size are optional and passed as null.
    let rc = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: on success `openpty` returns two new open fds that nothing
    // else owns.
    let (m, s) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // `try_clone` duplicates with close-on-exec; the originals are closed.
    (
        File::from(m.try_clone().expect("dup master")),
        File::from(s.try_clone().expect("dup slave")),
    )
}

/// `stty -g` of the terminal `tty`.
fn stty(tty: &File) -> String {
    let mut cmd = Command::new("stty");
    cmd.arg("-g").stdin(tty.try_clone().expect("clone tty"));
    let out = {
        let _spawn = spawn_lock();
        cmd.output()
    }
    .expect("run stty");
    assert!(out.status.success(), "stty failed");
    String::from_utf8(out.stdout).expect("stty output")
}

#[derive(Debug, PartialEq, Eq)]
struct PtyOutcome {
    status: String,
    /// Everything read from the pty master.
    master: Vec<u8>,
    /// stdout when it is a pipe.
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    sent: Vec<Vec<u8>>,
    /// `stty -g` after exit.
    after: String,
}

/// `stty -g` text with PENDIN cleared on macOS. After B6 has cleared
/// ICANON, restoring a canonical mode makes the BSD kernel set PENDIN
/// (reprint pending input); C and rovacom both leave it set.
fn without_pendin(stty_g: &str) -> String {
    if !cfg!(target_os = "macos") {
        return stty_g.to_string();
    }
    stty_g
        .split(':')
        .map(|field| match field.strip_prefix("lflag=") {
            Some(hex) => {
                let v = libc::tcflag_t::from_str_radix(hex, 16).expect("lflag hex");
                format!("lflag={:x}", v & !libc::PENDIN)
            }
            None => field.to_string(),
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// Runs `bin -c login` with stdin and/or stdout on a pty slave; the
/// password goes to the master (stdin tty) or a file (stdin not a tty).
/// Asserts the terminal attributes are unchanged after exit (except PENDIN
/// on macOS, see [`without_pendin`]).
fn pty_run(bin: &Path, stdin_tty: bool, stdout_tty: bool, password: &[u8]) -> PtyOutcome {
    let ctrl = CtrlMock::start(Some(b"ok\n"));
    let mock = Mock::start(&[SESS]);
    let (mut master, slave) = openpty();
    let before = stty(&slave);
    let file = StdinFile::new(password);
    let mut cmd = Command::new(bin);
    cmd.arg0("rovacom")
        .args(
            ctrl_args(&["-c", "login"], mock.port)
                .into_iter()
                .map(OsString::from_vec),
        )
        .stderr(Stdio::piped())
        .env_remove("POSIXLY_CORRECT");
    if stdin_tty {
        cmd.stdin(slave.try_clone().expect("clone slave"));
    } else {
        cmd.stdin(file.open());
    }
    if stdout_tty {
        cmd.stdout(slave.try_clone().expect("clone slave"));
    } else {
        cmd.stdout(Stdio::piped());
    }
    let spawned = {
        let _spawn = spawn_lock();
        cmd.spawn()
    };
    let mut child = spawned.expect("spawn");
    drop(cmd);
    let out = child.stdout.take().map(spawn_reader);
    let err = spawn_reader(child.stderr.take().expect("stderr pipe"));
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
    if stdin_tty {
        if stdout_tty {
            // Echo is off once the prompt is visible.
            let deadline = Instant::now() + PTY_TIMEOUT;
            while !seen
                .lock()
                .expect("pty lock")
                .starts_with(b"Please enter password: ")
            {
                assert!(Instant::now() < deadline, "no prompt on the pty");
                std::thread::sleep(POLL);
            }
        }
        master.write_all(password).expect("write password");
    }
    let status = wait_child(&mut child, &bin.display().to_string());
    let after = stty(&slave);
    assert_eq!(
        without_pendin(&after),
        before,
        "terminal attributes changed by {}",
        bin.display()
    );
    drop(slave);
    reader.join().expect("pty reader");
    drop(mock);
    let master_bytes = seen.lock().expect("pty lock").clone();
    PtyOutcome {
        status,
        master: master_bytes,
        stdout: out.map(|h| h.join().expect("stdout")).unwrap_or_default(),
        stderr: err.join().expect("stderr"),
        sent: ctrl.finish(),
        after,
    }
}

fn pty_check(stdin_tty: bool, stdout_tty: bool, password: &[u8]) -> PtyOutcome {
    let c = pty_run(&reference(), stdin_tty, stdout_tty, password);
    let r = pty_run(&rovacom(), stdin_tty, stdout_tty, password);
    assert_eq!(
        (
            String::from_utf8_lossy(&r.master),
            String::from_utf8_lossy(&r.stdout)
        ),
        (
            String::from_utf8_lossy(&c.master),
            String::from_utf8_lossy(&c.stdout)
        ),
        "pty stdin_tty={stdin_tty} stdout_tty={stdout_tty}"
    );
    assert_eq!(r, c, "pty stdin_tty={stdin_tty} stdout_tty={stdout_tty}");
    r
}

#[test]
fn pty_password_prompt() {
    // Both on the terminal: prompt flushed before the read, no echo, and
    // the newline goes through output processing (ONLCR).
    let o = pty_check(true, true, b"secret\n");
    assert_eq!(o.status, "exit 0");
    assert_eq!(o.master, b"Please enter password: \r\n");
    assert_eq!(o.sent, [auth_cmd("login")]);
    // stdin on the terminal, stdout a pipe: tcgetattr(1) fails, so echo
    // stays on (C quirk), and the prompt waits in the stdout buffer.
    let o = pty_check(true, false, b"secret\n");
    assert_eq!(o.status, "exit 0");
    assert_eq!(o.master, b"secret\r\n");
    assert_eq!(o.stdout, PROMPT);
    assert_eq!(o.sent, [auth_cmd("login")]);
    // stdout on the terminal, stdin a file: line-buffered prompt, written
    // with the newline after the read.
    let o = pty_check(false, true, b"secret\n");
    assert_eq!(o.status, "exit 0");
    assert_eq!(o.master, b"Please enter password: \r\n");
    assert_eq!(o.sent, [auth_cmd("login")]);
}
