//! Differential tests: run the C reference `novacom` and `rovacom` with the
//! same arguments against the same mock servers and compare exit status,
//! stdout and stderr byte for byte. Session cases also compare the bytes the
//! client sent to the mock device port.
//!
//! Only mock servers on 127.0.0.1 are used. Runs with `-c` connect to the
//! fixed control port 6971; see the `control` module for the safety rule.

#[path = "differential/control.rs"]
mod control;
#[path = "differential/relay.rs"]
mod relay;
#[path = "differential/terminal.rs"]
mod terminal;

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Version line printed by the reference build (`scripts/build-reference.sh`).
const C_VERSION_LINE: &[u8] = b"version: novacom-ref-d9d8b77\n";
/// Upper bound for one binary run.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// Poll interval of the mock server's accept loop and of child waits.
const POLL: Duration = Duration::from_millis(5);

/// On macOS, std marks new sockets and accepted connections close-on-exec
/// with a second call, so a child spawned by another test thread in between
/// inherits the fd. Fds whose leak would matter (the port 6971 listener and
/// its connections, pty fds) are created under the write lock; children are
/// spawned under the read lock.
static FD_LOCK: RwLock<()> = RwLock::new(());

fn spawn_lock() -> RwLockReadGuard<'static, ()> {
    FD_LOCK.read().unwrap_or_else(PoisonError::into_inner)
}

fn fd_create_lock() -> RwLockWriteGuard<'static, ()> {
    FD_LOCK.write().unwrap_or_else(PoisonError::into_inner)
}

fn reference() -> PathBuf {
    let path = std::env::var_os("ROVACOM_REFERENCE").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("reference/novacom/build-novacom/novacom"),
        PathBuf::from,
    );
    assert!(
        path.is_file(),
        "C reference binary not found at {}: run scripts/build-reference.sh (or set ROVACOM_REFERENCE)",
        path.display()
    );
    path
}

fn rovacom() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rovacom"))
}

/// Device-list server: connection `i` gets `responses[min(i, len - 1)]`,
/// then the server closes the connection.
struct Mock {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Mock {
    fn start(responses: &[&[u8]]) -> Self {
        let responses: Vec<Vec<u8>> = responses.iter().map(|r| r.to_vec()).collect();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("mock address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut served = 0usize;
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).expect("blocking stream");
                        let body = responses.get(served.min(responses.len().saturating_sub(1)));
                        served += 1;
                        // The client may close early (find_device stops at a match).
                        let _ = stream.write_all(body.map_or(&[][..], Vec::as_slice));
                        let _ = stream.shutdown(Shutdown::Write);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(POLL)
                    }
                    Err(e) => panic!("mock accept: {e}"),
                }
            }
        });
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A port with nothing listening on it.
fn refused_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

#[derive(PartialEq, Eq)]
struct Outcome {
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outcome")
            .field("status", &self.status)
            .field("stdout", &String::from_utf8_lossy(&self.stdout))
            .field("stderr", &String::from_utf8_lossy(&self.stderr))
            .finish()
    }
}

fn spawn_reader<R: Read + Send + 'static>(mut r: R) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).expect("read child output");
        buf
    })
}

/// How the child's stdout is consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StdoutMode {
    /// Read as fast as possible.
    Normal,
    /// Read in small chunks with pauses, so the pipe fills up.
    Slow,
    /// The read end is closed right after spawn (writes raise SIGPIPE).
    Closed,
}

/// Bytes per read of a [`StdoutMode::Slow`] reader.
const SLOW_CHUNK: usize = 4096;
/// A slow reader pauses after this many reads.
const SLOW_READS_PER_PAUSE: usize = 16;
const SLOW_PAUSE: Duration = Duration::from_millis(1);

fn spawn_slow_reader<R: Read + Send + 'static>(mut r: R) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut chunk = vec![0u8; SLOW_CHUNK];
        for i in 1.. {
            let n = r.read(&mut chunk).expect("read child output");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
            if i % SLOW_READS_PER_PAUSE == 0 {
                std::thread::sleep(SLOW_PAUSE);
            }
        }
        out
    })
}

/// Runs `bin` with `argv[0] = arg0`, `args`, and `POSIXLY_CORRECT` set iff `posix`.
fn run(bin: &Path, arg0: &str, args: &[Vec<u8>], posix: bool) -> Outcome {
    run_io(bin, arg0, args, posix, Stdio::null(), StdoutMode::Normal)
}

/// [`run`] with the given stdin and stdout handling.
fn run_io(
    bin: &Path,
    arg0: &str,
    args: &[Vec<u8>],
    posix: bool,
    stdin: Stdio,
    stdout: StdoutMode,
) -> Outcome {
    let mut cmd = Command::new(bin);
    cmd.arg0(arg0)
        .args(args.iter().map(|a| OsString::from_vec(a.clone())))
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("POSIXLY_CORRECT");
    if posix {
        cmd.env("POSIXLY_CORRECT", "1");
    }
    let spawned = {
        let _spawn = spawn_lock();
        cmd.spawn()
    };
    let mut child = spawned.unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
    let child_out = child.stdout.take().expect("stdout pipe");
    let out = match stdout {
        StdoutMode::Normal => spawn_reader(child_out),
        StdoutMode::Slow => spawn_slow_reader(child_out),
        StdoutMode::Closed => {
            drop(child_out);
            std::thread::spawn(Vec::new)
        }
    };
    let err = spawn_reader(child.stderr.take().expect("stderr pipe"));
    let deadline = Instant::now() + RUN_TIMEOUT;
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{} {args:?} timed out", bin.display());
        }
        std::thread::sleep(POLL);
    };
    let status = match (status.code(), status.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        (None, None) => "unknown".to_string(),
    };
    Outcome {
        status,
        stdout: out.join().expect("stdout"),
        stderr: err.join().expect("stderr"),
    }
}

/// Replaces `from` with `to` where it starts a line.
fn replace_at_line_start(text: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut at_start = true;
    let mut i = 0;
    while i < text.len() {
        if at_start && text[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
            at_start = from.ends_with(b"\n");
            continue;
        }
        at_start = text[i] == b'\n';
        out.push(text[i]);
        i += 1;
    }
    out
}

/// Normalizes C output to what rovacom must print: the version line always;
/// the macOS executable-basename prefix of getopt diagnostics unless `alias`.
fn normalize_c(mut o: Outcome, alias: bool) -> Outcome {
    o.stdout = replace_at_line_start(&o.stdout, C_VERSION_LINE, &rovacom::cli::version_line());
    if cfg!(target_os = "macos") && !alias {
        o.stderr = replace_at_line_start(&o.stderr, b"novacom: ", b"rovacom: ");
    }
    o
}

/// Builds argv from string parts; `{P}` is replaced by the mock port.
fn argv(parts: &[&str], port: u16) -> Vec<Vec<u8>> {
    parts
        .iter()
        .map(|p| p.replace("{P}", &port.to_string()).into_bytes())
        .collect()
}

/// Runs both binaries (each against a fresh mock serving `responses`) and
/// asserts identical outcomes. Returns the outcome.
fn check_with(responses: &[&[u8]], posix: bool, args: &dyn Fn(u16) -> Vec<Vec<u8>>) -> Outcome {
    let c = {
        let mock = Mock::start(responses);
        normalize_c(run(&reference(), "rovacom", &args(mock.port), posix), false)
    };
    let r = {
        let mock = Mock::start(responses);
        run(&rovacom(), "rovacom", &args(mock.port), posix)
    };
    assert_eq!(
        r,
        c,
        "args {:?}",
        String::from_utf8_lossy(&args(0).join(&b' '))
    );
    r
}

fn check(responses: &[&[u8]], parts: &[&str]) -> Outcome {
    check_with(responses, false, &|port| argv(parts, port))
}

const BASIC: &[u8] = b"6970 0123456789abcdef0123456789abcdef01234567 usb castle-linux\n\
6972 FEDCBA9876543210FEDCBA9876543210FEDCBA98 tcp Topaz-Linux sess123456789\n";

fn usage_status() -> String {
    "exit 1".to_string()
}

#[test]
fn list_basic_and_empty() {
    let o = check(&[BASIC], &["-p", "{P}", "-l"]);
    assert_eq!((o.status.as_str(), o.stdout.as_slice()), ("exit 0", BASIC));
    let o = check(&[b""], &["-l", "-p", "{P}"]);
    assert_eq!((o.status.as_str(), o.stdout.len()), ("exit 0", 0));
    check(&[b""], &["-p", "{P}"]);
    check(&[BASIC], &["-a", "127.0.0.1", "-p", "{P}", "-l"]);
    check(&[BASIC], &["-a", "localhost", "-p", "{P}", "-l"]);
    check(&[BASIC], &["-a", "127.1", "-p", "{P}", "-l"]);
    // `-l` returns before any `-c` connection; port 6971 is held anyway.
    let ctrl = control::CtrlMock::start(None);
    check(
        &[BASIC],
        &[
            "-ts", "-w", "-r", "pw", "-P", "-f", "1:2,3:4", "-c", "list", "-l", "-p", "{P}",
        ],
    );
    assert!(ctrl.finish().is_empty());
}

#[test]
fn malformed_lines() {
    let data: &[u8] =
        b"garbage\n1\n1 a b\n-x a b c\n+ a b c\n 6970\ta\x0bb\x0cc\n12x34 n c d e f\n6972 n c d\n";
    check(&[data], &["-l", "-p", "{P}"]);
    let o = check(&[data], &["-p", "{P}"]);
    assert_eq!(o.status, usage_status());
}

#[test]
fn long_lines() {
    let mut over = b"6970 ".to_vec();
    over.extend(std::iter::repeat_n(b'a', 295));
    over.extend_from_slice(b" usb t\n6971 n c d\n");
    check(&[&over], &["-l", "-p", "{P}"]);

    // Exactly 255 bytes, then '\n': the newline is read as an empty line
    // and ends the list.
    let mut exact = b"6970 ".to_vec();
    exact.extend(std::iter::repeat_n(b'b', 250));
    assert_eq!(exact.len(), 255);
    exact.extend_from_slice(b"\n6971 n c d\n");
    check(&[&exact], &["-l", "-p", "{P}"]);

    // Remainder of a split line is parsed as its own line.
    let mut split = b"6970 n c d ".to_vec();
    split.extend(std::iter::repeat_n(b' ', 255 - split.len()));
    split.extend_from_slice(b"6973 tail x y\n");
    check(&[&split], &["-l", "-p", "{P}"]);
}

#[test]
fn nul_cr_empty_and_eof() {
    check(
        &[b"6970 n1\0 x y\n6971 n2 c d\0garbage\n\0\n6972 n3 c d\n"],
        &["-l", "-p", "{P}"],
    );
    check(&[b"6970 n1 usb t\r\n6971 n2 c d\r\n"], &["-l", "-p", "{P}"]);
    check(
        &[b"6970 n1 usb t\r\n6971 n2 c d\r\n"],
        &["-l", "-d", "T", "-p", "{P}"],
    );
    check(&[b"6970 a b c\n\n6971 d e f\n"], &["-l", "-p", "{P}"]);
    check(&[b"6970 a b c\n6971 d e f"], &["-l", "-p", "{P}"]);
    check(
        &[b"6970 a b c\n6971 d e f"],
        &["-l", "-d", "E", "-p", "{P}"],
    );
}

#[test]
fn long_nduid() {
    let n45 = "n".repeat(45);
    let line = format!("6970 {n45} usb t\n");
    for filter in ["", &"N".repeat(40), &n45, "nnnnn", "usb"] {
        check(&[line.as_bytes()], &["-l", "-d", filter, "-p", "{P}"]);
    }
}

#[test]
fn device_filter() {
    for filter in [
        "USB",
        "topaz-LINUX",
        "fedcba9876543210fedcba9876543210fedcba98",
        "0123456789ABCDEF0123456789ABCDEF01234567",
        "TCP",
        "nomatch",
        "sess123456",
        "",
    ] {
        check(&[BASIC], &["-l", "-d", filter, "-p", "{P}"]);
        check(&[BASIC], &["-d", filter, "-p", "{P}"]);
    }
}

#[test]
fn high_bytes() {
    let data: &[u8] = b"6970 \xc9t \xa0 \x85x\n6971 a\xa0b c d\n6972 \xff c\x85 d\xa0\n";
    for filter in [&b"\xe9t"[..], b"\xc9T", b"\xa0", b"c\x85", b"\xff"] {
        check_with(&[data], false, &|port| {
            vec![
                b"-l".to_vec(),
                b"-d".to_vec(),
                filter.to_vec(),
                b"-p".to_vec(),
                port.to_string().into_bytes(),
            ]
        });
    }
}

#[test]
fn negative_port_and_selection() {
    let data: &[u8] = b"-1 a b c\n6970 d e f\n";
    let o = check(&[data], &["-p", "{P}"]);
    assert_eq!(o.stderr, b"unable to find device\n");
    let o = check(&[data], &["-d", "D", "-p", "{P}"]);
    assert_eq!(o.status, usage_status());
    check(&[b"0 a b c\n"], &["-p", "{P}"]);
}

#[test]
fn wait_for_device() {
    let o = check(&[b"", BASIC], &["-w", "-p", "{P}"]);
    assert_eq!(o.status, usage_status());
    check(
        &[b"-5 a b c\n", b"junk\n", BASIC],
        &["-p", "{P}", "--wait", "-d", "castle-linux"],
    );
}

#[test]
fn connection_failures() {
    for extra in [&["-l"][..], &["-w"], &[]] {
        let port = refused_port().to_string();
        let mut parts = vec!["-p", &port];
        parts.extend_from_slice(extra);
        let o = check(&[], &parts);
        assert_eq!(o.stderr, b"failed to connect to server\n");
    }
    check(&[], &["-a", "nonexistent.invalid", "-l"]);
    check(&[], &["-a", "", "-l"]);
    check(&[], &["-a", "::1", "-p", "{P}", "-l"]);
    check(&[], &["-p", "junk", "-l"]);
}

#[test]
fn port_atoi_wrapping() {
    let variants: [&dyn Fn(u16) -> String; 6] = [
        &|p| (i64::from(p) + 65536).to_string(),
        &|p| (i64::from(p) - 65536).to_string(),
        &|p| (i64::from(p) + (1i64 << 32)).to_string(),
        &|p| format!(" \t+{p}"),
        &|p| format!("{p}junk"),
        &|p| (i64::from(p) - (1i64 << 32) * 3).to_string(),
    ];
    for v in variants {
        let o = check_with(&[BASIC], false, &|port| {
            vec![b"-l".to_vec(), b"-p".to_vec(), v(port).into_bytes()]
        });
        assert_eq!(o.status, "exit 0");
    }
}

#[test]
fn version_and_order() {
    check(&[], &["-V"]);
    check(&[], &["-Z", "-V"]);
    check(&[], &["-V", "-Z"]);
    check(&[], &["-l", "-p", "1", "run", "x", "-V"]);
    check(&[], &["-f", "3000000000", "-V"]);
    check(&[], &["-V", "-f", "3000000000"]);
}

#[test]
fn getopt_errors() {
    for arg in [
        "-Z",
        "-a",
        "-p",
        "--addr",
        "--address",
        "--bogus",
        "--bogus=1",
        "--wait=1",
        "--wai=1",
        "--list=",
        "--p",
        "--p=5",
        "--pa",
        "--po",
        "--=x",
        "-l-",
        "-l-x",
        "-l:",
        "---x",
        "---",
        "-lZ",
        "-\u{e9}",
    ] {
        check(&[], &[arg]);
        check(&[], &["-l", "x", arg]);
    }
}

#[test]
fn long_option_forms() {
    check(&[BASIC], &["--li", "--po={P}"]);
    check(&[BASIC], &["--port", "{P}", "--list", "--device", "usb"]);
    check(
        &[BASIC],
        &[
            "--port={P}",
            "--l",
            "--d=TCP",
            "--addr",
            "127.0.0.1",
            "--term",
            "--sig",
            "--wa",
        ],
    );
    check(&[BASIC], &["-l-list", "-p", "{P}"]);
    check(&[BASIC], &["-p{P}", "-ld", "USB"]);
}

#[test]
fn permutation_and_double_dash() {
    check(&[BASIC], &["run", "-l", "-p", "{P}", "x"]);
    check(&[BASIC], &["x", "-p", "{P}", "-l", "--", "-V"]);
    check(&[BASIC], &["-l", "-p", "{P}", "-", "--", "--bogus"]);
    check(&[BASIC], &["-p", "--", "-l"]); // "--" is the argument of -p
}

#[test]
fn posixly_correct() {
    check_with(&[BASIC], true, &|p| {
        argv(&["-l", "-p", "{P}", "x", "-V"], p)
    });
    check_with(&[BASIC], true, &|p| {
        argv(&["-l", "-p", "{P}", "--", "-V"], p)
    });
    check_with(&[], true, &|p| argv(&["-Z"], p));
    check_with(&[], true, &|p| argv(&["--p"], p));
    check_with(&[], true, &|p| argv(&["-a"], p));
    check_with(&[], true, &|p| argv(&["-V", "x"], p));
}

#[test]
fn port_forward_parsing() {
    check(&[BASIC], &["-f", "1:2,3:4", "-l", "-p", "{P}"]);
    check(&[BASIC], &["-f", ",,1::2,", "-f", " 5:x", "-p", "{P}"]);
    for bad in [
        "3000000000",
        "1:-1",
        "1:2:3000000000",
        "-1:2",
        "1:18446744073709551616",
    ] {
        let o = check(&[], &["-f", bad, "-l"]);
        assert!(
            o.stderr
                .starts_with(b"Port Forwarding params were not correct\n")
        );
    }
}

/// Odd or empty `-f` lists crash the C client (strtoul(NULL), bug B8); the
/// port reports a parse error instead, so only rovacom is checked here.
#[test]
fn port_forward_odd_count_rust_only() {
    for bad in ["1", "1:2:3", "", ",:,"] {
        let o = run(&rovacom(), "rovacom", &argv(&["-f", bad, "-l"], 0), false);
        assert_eq!(o.status, "exit 1");
        assert_eq!(o.stderr, b"Port Forwarding params were not correct\n");
        assert_eq!(o.stdout, rovacom::cli::usage_text(b"rovacom"));
    }
}

static ALIAS_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Temporary directory holding a `novacom -> rovacom` symlink.
struct AliasDir(PathBuf);

impl AliasDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rovacom-alias-{}-{}",
            std::process::id(),
            ALIAS_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create alias dir");
        std::os::unix::fs::symlink(rovacom(), dir.join("novacom")).expect("create symlink");
        Self(dir)
    }

    fn bin(&self) -> PathBuf {
        self.0.join("novacom")
    }
}

impl Drop for AliasDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Invoked as `novacom` through a symlink, rovacom must match the C client
/// invoked as `novacom` with no program-name normalization.
fn check_alias(responses: &[&[u8]], parts: &[&str]) -> Outcome {
    let alias = AliasDir::new();
    let c = {
        let mock = Mock::start(responses);
        normalize_c(
            run(&reference(), "novacom", &argv(parts, mock.port), false),
            true,
        )
    };
    let r = {
        let mock = Mock::start(responses);
        run(&alias.bin(), "novacom", &argv(parts, mock.port), false)
    };
    assert_eq!(r, c, "alias args {parts:?}");
    r
}

#[test]
fn alias_invocation() {
    let o = check_alias(&[BASIC], &["-p", "{P}"]);
    assert!(o.stdout.windows(14).any(|w| w == b"usage: novacom"));
    check_alias(&[], &["-V"]);
    let o = check_alias(&[], &["-Z"]);
    assert!(o.stderr.starts_with(b"novacom: "));
    let o = check_alias(&[], &["--bogus"]);
    assert!(o.stderr.starts_with(b"novacom: "));
    check_alias(&[], &["--addr"]);
    let o = check_alias(&[BASIC], &["-l", "-p", "{P}"]);
    assert_eq!(o.stdout, BASIC);
}

// ---------------------------------------------------------------------------
// Remote command sessions (`novacom [opts] <command...>`, no -t/-s).
// ---------------------------------------------------------------------------

const MAGIC: u32 = 0xDECA_FBAD;
const T_DATA: u32 = 0;
const T_ERR: u32 = 1;
const T_OOB: u32 = 2;
const OOB_EOF: u32 = 0;
const OOB_SIGNAL: u32 = 1;
const OOB_RETURN: u32 = 2;
const OOB_RESIZE: u32 = 3;
/// Meaningful OOB body bytes (message + first union word); RESIZE also
/// uses the second word.
const OOB_MEANINGFUL: usize = 8;
/// Read timeout of the mock device connection (guards against hangs).
const MOCK_IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest DATA payload the mock echoes per packet.
const ECHO_CHUNK: usize = 65536;

/// A raw packet header (independent of the code under test).
fn hdr(magic: u32, version: u32, size: u32, kind: u32) -> Vec<u8> {
    [magic, version, size, kind]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect()
}

fn pkt(kind: u32, body: &[u8]) -> Vec<u8> {
    let mut p = hdr(MAGIC, 1, u32::try_from(body.len()).expect("size"), kind);
    p.extend_from_slice(body);
    p
}

fn oob_body(message: u32, arg: u32) -> Vec<u8> {
    let mut b = vec![0u8; 20];
    b[..4].copy_from_slice(&message.to_le_bytes());
    b[4..8].copy_from_slice(&arg.to_le_bytes());
    b
}

fn oob(message: u32, arg: u32) -> Vec<u8> {
    pkt(T_OOB, &oob_body(message, arg))
}

fn ret(rc: i32) -> Vec<u8> {
    oob(OOB_RETURN, u32::from_le_bytes(rc.to_le_bytes()))
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// One step of the mock device's script, run after it has read the command
/// line. The mock always closes the connection after the last step.
#[derive(Clone, Debug)]
enum Step {
    Send(Vec<u8>),
    /// Sends each part as its own TCP segment, pausing between parts.
    SendParts(Vec<Vec<u8>>, Duration),
    /// Records client packets until an OOB EOF (or close) arrives.
    ReadUntilEof,
    /// Sends back all DATA payload received so far as DATA packets.
    EchoStdin,
    /// Records client bytes until the client closes the connection.
    WaitClose,
}

/// What the client sent to the mock device port.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Recorded {
    connections: usize,
    command: Vec<u8>,
    sent: Vec<u8>,
}

/// Splits recorded client packets into (meaningful bytes, OOB padding).
/// OOB padding is uninitialized memory in C (bug B5) and is compared only
/// for rovacom, which must send zeros.
fn split_padding(sent: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut meaningful, mut padding) = (Vec::new(), Vec::new());
    let mut rest = sent;
    while rest.len() >= 16 {
        let size = u32::from_le_bytes(rest[8..12].try_into().expect("4 bytes")) as usize;
        let kind = u32::from_le_bytes(rest[12..16].try_into().expect("4 bytes"));
        let end = (16 + size).min(rest.len());
        let used = if rest.get(16..20) == Some(&OOB_RESIZE.to_le_bytes()[..]) {
            OOB_MEANINGFUL + 4
        } else {
            OOB_MEANINGFUL
        };
        if kind == T_OOB && end >= 16 + used {
            meaningful.extend_from_slice(&rest[..16 + used]);
            padding.extend_from_slice(&rest[16 + used..end]);
        } else {
            meaningful.extend_from_slice(&rest[..end]);
        }
        rest = &rest[end..];
    }
    meaningful.extend_from_slice(rest);
    (meaningful, padding)
}

/// DATA payload of recorded client packets, concatenated.
fn data_payload(sent: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = sent;
    while rest.len() >= 16 {
        let size = u32::from_le_bytes(rest[8..12].try_into().expect("4 bytes")) as usize;
        let kind = u32::from_le_bytes(rest[12..16].try_into().expect("4 bytes"));
        let end = (16 + size).min(rest.len());
        if kind == T_DATA {
            out.extend_from_slice(&rest[16..end]);
        }
        rest = &rest[end..];
    }
    out
}

/// Reads one client packet and appends it to `rec.sent`. Returns false on
/// close/error or after an OOB EOF.
fn read_client_packet(s: &mut TcpStream, rec: &Mutex<Recorded>) -> bool {
    let mut h = [0u8; 16];
    if s.read_exact(&mut h).is_err() {
        return false;
    }
    let size = u32::from_le_bytes(h[8..12].try_into().expect("4 bytes")) as usize;
    let kind = u32::from_le_bytes(h[12..16].try_into().expect("4 bytes"));
    let mut body = vec![0u8; size];
    let ok = s.read_exact(&mut body).is_ok();
    let mut r = rec.lock().expect("record lock");
    r.sent.extend_from_slice(&h);
    r.sent.extend_from_slice(&body);
    let eof = kind == T_OOB && size >= 4 && body[..4] == OOB_EOF.to_le_bytes();
    ok && !eof
}

fn serve_device(mut s: TcpStream, script: &[Step], rec: &Mutex<Recorded>) {
    s.set_nonblocking(false).expect("blocking stream");
    s.set_nodelay(true).expect("nodelay");
    s.set_read_timeout(Some(MOCK_IO_TIMEOUT)).expect("timeout");
    // The command line, one byte at a time (the client sends nothing else
    // before it has read the response).
    let mut byte = [0u8; 1];
    while let Ok(1) = s.read(&mut byte) {
        rec.lock().expect("record lock").command.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    // Send errors are ignored: the client may already have exited.
    for step in script {
        match step {
            Step::Send(bytes) => {
                let _ = s.write_all(bytes);
            }
            Step::SendParts(parts, pause) => {
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        std::thread::sleep(*pause);
                    }
                    let _ = s.write_all(part);
                }
            }
            Step::ReadUntilEof => while read_client_packet(&mut s, rec) {},
            Step::EchoStdin => {
                let data = data_payload(&rec.lock().expect("record lock").sent);
                for chunk in data.chunks(ECHO_CHUNK) {
                    let _ = s.write_all(&pkt(T_DATA, chunk));
                }
            }
            Step::WaitClose => {
                let mut rest = Vec::new();
                let _ = s.read_to_end(&mut rest);
                rec.lock().expect("record lock").sent.extend(rest);
            }
        }
    }
}

/// Mock device port: serves every connection with `script`.
struct DeviceMock {
    port: u16,
    rec: Arc<Mutex<Recorded>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DeviceMock {
    fn start(script: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind device mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        let rec = Arc::new(Mutex::new(Recorded::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (rec2, stop2) = (Arc::clone(&rec), Arc::clone(&stop));
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((s, _)) => {
                        rec2.lock().expect("record lock").connections += 1;
                        serve_device(s, &script, &rec2);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(POLL)
                    }
                    Err(e) => panic!("device mock accept: {e}"),
                }
            }
        });
        Self {
            port,
            rec,
            stop,
            handle: Some(handle),
        }
    }

    /// Stops the mock and returns what it recorded.
    fn finish(mut self) -> Recorded {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().expect("device mock thread");
        }
        self.rec.lock().expect("record lock").clone()
    }
}

impl Drop for DeviceMock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

static STDIN_SEQ: AtomicUsize = AtomicUsize::new(0);

/// A temporary regular file used as stdin. A regular file makes `read`
/// return full 4096-byte chunks, so the client's DATA packet framing is
/// deterministic.
struct StdinFile(PathBuf);

impl StdinFile {
    fn new(data: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rovacom-stdin-{}-{}",
            std::process::id(),
            STDIN_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, data).expect("write stdin file");
        Self(path)
    }

    fn open(&self) -> Stdio {
        Stdio::from(File::open(&self.0).expect("open stdin file"))
    }
}

impl Drop for StdinFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One session test case.
struct Session<'a> {
    /// Device list; `{D}` is the device mock port, `{R}` a refused port.
    devlist: &'a str,
    /// Arguments; `{P}` is the device-list port.
    args: Vec<Vec<u8>>,
    stdin: &'a [u8],
    stdout: StdoutMode,
    script: Vec<Step>,
}

impl<'a> Session<'a> {
    fn new(cmd: &[&str], script: Vec<Step>) -> Self {
        let mut args = vec![b"-p".to_vec(), b"{P}".to_vec()];
        args.extend(cmd.iter().map(|c| c.as_bytes().to_vec()));
        Self {
            devlist: "{D} 0123456789abcdef0123456789abcdef01234567 usb castle-linux\n",
            args,
            stdin: b"",
            stdout: StdoutMode::Normal,
            script,
        }
    }

    fn stdin(mut self, data: &'a [u8]) -> Self {
        self.stdin = data;
        self
    }

    fn stdout(mut self, mode: StdoutMode) -> Self {
        self.stdout = mode;
        self
    }

    fn devlist(mut self, devlist: &'a str) -> Self {
        self.devlist = devlist;
        self
    }

    /// Runs `bin` against fresh mocks.
    fn run(&self, bin: &Path, stdin: &StdinFile) -> (Outcome, Recorded) {
        let device = DeviceMock::start(self.script.clone());
        let list = self
            .devlist
            .replace("{D}", &device.port.to_string())
            .replace("{R}", &refused_port().to_string());
        let mock = Mock::start(&[list.as_bytes()]);
        let args: Vec<Vec<u8>> = self
            .args
            .iter()
            .map(|a| {
                if a == b"{P}" {
                    mock.port.to_string().into_bytes()
                } else {
                    a.clone()
                }
            })
            .collect();
        let o = run_io(bin, "rovacom", &args, false, stdin.open(), self.stdout);
        drop(mock);
        (o, device.finish())
    }

    /// Runs rovacom only and checks its OOB padding is zero.
    fn run_rust(&self) -> (Outcome, Recorded) {
        let stdin = StdinFile::new(self.stdin);
        let (o, rec) = self.run(&rovacom(), &stdin);
        let (_, padding) = split_padding(&rec.sent);
        assert!(padding.iter().all(|&b| b == 0), "B5: OOB padding not zero");
        (o, rec)
    }

    /// Runs both binaries and asserts identical exit status, stdout, stderr,
    /// command line and client packets (OOB padding excluded, B5).
    fn check(&self) -> (Outcome, Recorded) {
        let stdin = StdinFile::new(self.stdin);
        let (c, c_rec) = self.run(&reference(), &stdin);
        let (r, r_rec) = self.run_rust();
        let what = String::from_utf8_lossy(&self.args.join(&b' ')).into_owned();
        assert_eq!(r, c, "outcome, args {what:?}");
        assert_eq!(
            r_rec.connections, c_rec.connections,
            "connections, {what:?}"
        );
        assert!(
            r_rec.command == c_rec.command,
            "command line differs, {what:?}"
        );
        let (r_sent, _) = split_padding(&r_rec.sent);
        let (c_sent, _) = split_padding(&c_rec.sent);
        assert!(r_sent == c_sent, "client packets differ, {what:?}");
        (r, r_rec)
    }
}

/// Usual script prefix: reply `ok`, then wait for the client's stdin EOF,
/// so the client sends nothing more and every later close is an orderly one.
fn after_eof(steps: &[Step]) -> Vec<Step> {
    let mut v = vec![Step::Send(b"ok\n".to_vec()), Step::ReadUntilEof];
    v.extend_from_slice(steps);
    v
}

fn send_after_eof(bytes: Vec<u8>) -> Vec<Step> {
    after_eof(&[Step::Send(bytes)])
}

const EOF_MSG: &[u8] = b"novacom: unexpected EOF from server\n";
const CLOSED_MSG: &[u8] = b"\nnovacomd socket was closed prematurely\n";

#[test]
fn session_echo_empty_stdin() {
    let (o, rec) = Session::new(
        &["run", "file:///bin/cat"],
        after_eof(&[Step::EchoStdin, Step::Send(ret(0))]),
    )
    .check();
    assert_eq!(o.status, "exit 0");
    assert!(o.stdout.is_empty() && o.stderr.is_empty());
    assert_eq!(rec.command, b"run file:///bin/cat\n");
    assert_eq!(rec.sent.len(), 36);
}

#[test]
fn session_binary_stdin_all_byte_values() {
    let data: Vec<u8> = (0..40).flat_map(|_| 0..=255u8).collect();
    let (o, rec) = Session::new(
        &["run", "file:///bin/cat"],
        after_eof(&[Step::EchoStdin, Step::Send(ret(0))]),
    )
    .stdin(&data)
    .check();
    assert_eq!((o.status.as_str(), &o.stdout), ("exit 0", &data));
    assert_eq!(data_payload(&rec.sent), data);
}

#[test]
fn session_upload_10mib() {
    let data: Vec<u8> = (0..10 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let (o, rec) = Session::new(&["run", "x"], send_after_eof(ret(0)))
        .stdin(&data)
        .check();
    assert_eq!(o.status, "exit 0");
    assert!(data_payload(&rec.sent) == data, "uploaded payload differs");
}

#[test]
fn session_download_10mib_slow_consumer() {
    let total = 10 * 1024 * 1024usize;
    let data: Vec<u8> = Vec::from_fn(total, |i| (i % 251) as u8);
    let mut stream = Vec::new();
    // Packet sizes vary, including some larger than the 64 KiB receive buffer.
    for (i, chunk) in data.chunks(100_000).enumerate() {
        if i % 2 == 0 {
            stream.extend(pkt(T_DATA, chunk));
        } else {
            for part in chunk.chunks(4096) {
                stream.extend(pkt(T_DATA, part));
            }
        }
    }
    stream.extend(ret(0));
    let (o, _) = Session::new(&["run", "x"], send_after_eof(stream))
        .stdout(StdoutMode::Slow)
        .check();
    assert_eq!(o.status, "exit 0");
    assert!(o.stdout == data, "downloaded payload differs");
}

#[test]
fn session_stdout_stderr_interleaved() {
    let stream = cat(&[
        &pkt(T_DATA, b"out1\n"),
        &pkt(T_ERR, b"err1\n"),
        &pkt(T_DATA, b"out2\0\xff\n"),
        &pkt(T_ERR, b"err2\0\n"),
        &pkt(T_DATA, b"out3"),
        &ret(0),
    ]);
    let (o, _) = Session::new(&["run", "x"], send_after_eof(stream)).check();
    assert_eq!(o.stdout, b"out1\nout2\0\xff\nout3");
    assert_eq!(o.stderr, b"err1\nerr2\0\n");
}

#[test]
fn session_return_codes() {
    for (rc, status, msg) in [
        (0, "exit 0", false),
        (1, "exit 1", true),
        (255, "exit 255", true),
        (256, "exit 0", true),
        (-1, "exit 255", true),
    ] {
        let (o, _) = Session::new(&["run", "x"], send_after_eof(ret(rc))).check();
        assert_eq!(o.status, status, "rc {rc}");
        assert_eq!(o.stderr, if msg { EOF_MSG } else { b"" }, "rc {rc}");
    }
}

#[test]
fn session_peer_closes_early() {
    let data = pkt(T_DATA, b"partial");
    for (bytes, stdout) in [
        // No RETURN.
        (data.clone(), &b"partial"[..]),
        // Mid-header.
        (data[..7].to_vec(), b""),
        // Mid-body (header and part of the body in one segment).
        (data[..19].to_vec(), b"par"),
    ] {
        let (o, _) = Session::new(&["run", "x"], send_after_eof(bytes)).check();
        assert_eq!(o.status, "exit 255");
        assert_eq!(o.stdout, stdout);
        assert_eq!(o.stderr, cat(&[CLOSED_MSG, EOF_MSG]));
    }
}

#[test]
fn session_bad_header() {
    for (bytes, msg) in [
        (hdr(0xDECA_FBAE, 1, 0, 0), &b"Bad packet header magic\n"[..]),
        (hdr(MAGIC, 2, 0, 0), b"Unsupported packet version\n"),
    ] {
        let (o, _) = Session::new(&["run", "x"], send_after_eof(bytes)).check();
        assert_eq!(o.status, "exit 255");
        assert_eq!(
            o.stderr,
            cat(&[msg, b"\nbad data from other end\n", EOF_MSG])
        );
    }
}

#[test]
fn session_unknown_type_and_zero_size() {
    let stream = cat(&[&pkt(7, b"abc"), &pkt(T_DATA, b"ok"), &ret(0)]);
    let (o, _) = Session::new(&["run", "x"], send_after_eof(stream)).check();
    assert_eq!(o.status, "exit 0");
    assert_eq!(o.stderr, b"unknown packet, discarding data\n");
    // Zero-size DATA and ERR are taken as a closed socket (quirk B3, kept).
    for kind in [T_DATA, T_ERR] {
        let stream = cat(&[&pkt(kind, b""), &ret(0)]);
        let (o, _) = Session::new(&["run", "x"], send_after_eof(stream)).check();
        assert_eq!(o.status, "exit 255");
        assert_eq!(o.stderr, cat(&[CLOSED_MSG, EOF_MSG]));
    }
}

#[test]
fn session_oob_sizes() {
    // Size 0: diagnostic, then a closed socket (B3).
    let (o, _) = Session::new(
        &["run", "x"],
        send_after_eof(cat(&[&pkt(T_OOB, b""), &ret(0)])),
    )
    .check();
    assert_eq!(o.status, "exit 255");
    // Size 8: silently consumed.
    let short = pkt(T_OOB, &oob_body(OOB_RETURN, 5)[..8]);
    let (o, _) = Session::new(&["run", "x"], send_after_eof(cat(&[&short, &ret(3)]))).check();
    assert_eq!(
        (o.status.as_str(), o.stderr.as_slice()),
        ("exit 3", EOF_MSG)
    );
    // Size 24: the message is acted on, then the 4 stray bytes are discarded.
    let mut body = oob_body(OOB_EOF, 2);
    body.extend_from_slice(b"WXYZ");
    let stream = cat(&[&pkt(T_OOB, &body), &pkt(T_DATA, b"after"), &ret(0)]);
    let (o, _) = Session::new(&["run", "x"], send_after_eof(stream)).check();
    assert_eq!(o.stdout, b"after");
    assert_eq!(
        o.stderr,
        b"oversized oob message. sizeof(m) = 20, h.size = 24, current pos = 20\n"
    );
}

#[test]
fn session_peer_oob_messages() {
    let stream = cat(&[
        &oob(9, 0),
        &oob(u32::MAX, 0),
        &oob(OOB_SIGNAL, 2),
        &oob(OOB_SIGNAL, 0x8000_0000),
        &oob(OOB_RESIZE, 24),
        &oob(OOB_EOF, 2),
        &oob(OOB_EOF, 0),
        &pkt(T_ERR, b"still here\n"),
        &ret(0),
    ]);
    let (o, _) = Session::new(&["run", "x"], send_after_eof(stream)).check();
    assert_eq!(o.status, "exit 0");
    assert!(
        o.stderr
            .starts_with(b"unknown OOB message: 9\nunknown OOB message: -1\n")
    );
}

#[test]
fn session_stdout_closed_by_peer() {
    let stream = cat(&[
        &pkt(T_DATA, b"before"),
        &oob(OOB_EOF, 1),
        &pkt(T_DATA, b"after"),
        &pkt(T_ERR, b"err\n"),
        &ret(0),
    ]);
    let (o, _) = Session::new(&["run", "x"], send_after_eof(stream)).check();
    assert_eq!(o.status, "exit 0");
    assert_eq!(o.stdout, b"before");
    assert_eq!(
        o.stderr,
        b"\nReceived data for stdout after closing stdout\nerr\n"
    );
}

#[test]
fn session_responses() {
    let long = "x".repeat(3000);
    for (resp, err) in [
        ("req:auth\n", "password required\n".to_string()),
        ("req:authorize me\n", "password required\n".to_string()),
        (
            "unrecognized command run\n",
            "unknown command\n".to_string(),
        ),
        (
            "o\n",
            "bad or error response from other side: 'o'\n".to_string(),
        ),
        (
            "\n",
            "bad or error response from other side: ''\n".to_string(),
        ),
        (
            "",
            "bad or error response from other side: ''\n".to_string(),
        ),
        (
            "bad\0rest\n",
            "bad or error response from other side: 'bad'\n".to_string(),
        ),
        (
            &format!("{long}\n"),
            format!(
                "bad or error response from other side: '{}'\n",
                &long[..2047]
            ),
        ),
    ] {
        // With no reply at all the mock just closes (EOF reads as "").
        let script = if resp.is_empty() {
            Vec::new()
        } else {
            vec![Step::Send(resp.as_bytes().to_vec()), Step::WaitClose]
        };
        let (o, rec) = Session::new(&["run", "x"], script)
            .stdin(b"never read")
            .check();
        assert_eq!(o.status, "exit 1", "{resp:?}");
        assert_eq!(o.stderr, err.as_bytes(), "{resp:?}");
        assert!(rec.sent.is_empty(), "{resp:?}");
    }
    // "ok" with trailing NUL garbage is accepted.
    let script = vec![
        Step::Send(b"ok\0junk\n".to_vec()),
        Step::ReadUntilEof,
        Step::Send(ret(0)),
    ];
    let (o, _) = Session::new(&["run", "x"], script).check();
    assert_eq!(o.status, "exit 0");
    // An over-long "ok" line: the rest of it is decoded as a packet header.
    let script = vec![
        Step::Send(format!("ok{}\n", "y".repeat(2100)).into_bytes()),
        Step::WaitClose,
    ];
    let (o, rec) = Session::new(&["run", "x"], script).check();
    assert_eq!(o.status, "exit 255");
    assert_eq!(
        o.stderr,
        cat(&[
            b"Bad packet header magic\n\nbad data from other end\n",
            EOF_MSG
        ])
    );
    assert!(rec.sent.is_empty());
}

#[test]
fn session_many_operands() {
    let ops: Vec<String> = Vec::from_fn(300, |i| format!("arg{i}"));
    let mut cmd: Vec<&str> = vec!["run", "file:///bin/echo"];
    cmd.extend(ops.iter().map(String::as_str));
    cmd.push("");
    cmd.push("a b");
    let (_, rec) = Session::new(&cmd, send_after_eof(ret(0))).check();
    let expected = format!("{}\n", cmd.join(" "));
    assert!(expected.len() < 4096);
    assert_eq!(rec.command, expected.as_bytes());
}

/// A command line longer than 4095 bytes overflows the C `command` buffer
/// (bug B1, undefined behavior), so only rovacom is checked: it sends the
/// whole line.
#[test]
fn session_long_command_rust_only() {
    let big = "z".repeat(5000);
    let (o, rec) = Session::new(&["run", &big], send_after_eof(ret(0))).run_rust();
    assert_eq!(o.status, "exit 0");
    assert_eq!(rec.command, format!("run {big}\n").as_bytes());
}

#[test]
fn session_device_selection_and_refused() {
    let two = "{R} n1 usb castle\n{D} n2 tcp topaz\n";
    let (o, rec) = Session::new(&["-d", "TOPAZ", "run", "x"], send_after_eof(ret(0)))
        .devlist(two)
        .check();
    assert_eq!((o.status.as_str(), rec.connections), ("exit 0", 1));
    let (o, rec) = Session::new(&["run", "x"], send_after_eof(ret(0)))
        .devlist(two)
        .check();
    assert_eq!(o.stderr, b"failed to connect to server\n");
    assert_eq!((o.status.as_str(), rec.connections), ("exit 1", 0));
}

#[test]
fn closed_stdout_pipe_raises_sigpipe() {
    let stream = cat(&[&pkt(T_DATA, b"hello\n"), &ret(0)]);
    let (o, _) = Session::new(
        &["run", "x"],
        after_eof(&[Step::Send(stream), Step::WaitClose]),
    )
    .stdout(StdoutMode::Closed)
    .check();
    assert_eq!(o.status, "signal 13");
    // Device listing to a closed pipe (a slice-1 path).
    let mock = Mock::start(&[BASIC]);
    let args = argv(&["-l", "-p", "{P}"], mock.port);
    let c = run_io(
        &reference(),
        "rovacom",
        &args,
        false,
        Stdio::null(),
        StdoutMode::Closed,
    );
    let r = run_io(
        &rovacom(),
        "rovacom",
        &args,
        false,
        Stdio::null(),
        StdoutMode::Closed,
    );
    assert_eq!(r, c);
    assert_eq!(r.status, "signal 13");
}

/// Header and body in separate TCP segments with a pause between them. C
/// reads the body in the same call as the header and takes EAGAIN as a
/// closed socket (bug B2), so its result depends on timing; only rovacom
/// is checked.
#[test]
fn session_split_segments_rust_only() {
    let data = pkt(T_DATA, b"body bytes");
    let o_ret = ret(4);
    let parts = vec![
        data[..16].to_vec(),
        data[16..].to_vec(),
        o_ret[..10].to_vec(),
        o_ret[10..16].to_vec(),
        o_ret[16..].to_vec(),
    ];
    let script = after_eof(&[Step::SendParts(parts, Duration::from_millis(50))]);
    let (o, _) = Session::new(&["run", "x"], script).run_rust();
    assert_eq!(o.status, "exit 4");
    assert_eq!(o.stdout, b"body bytes");
    assert_eq!(o.stderr, EOF_MSG);
}
