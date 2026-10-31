//! Differential tests: run the C reference `novacom` and `rovacom` with the
//! same arguments against the same mock device-list server and compare exit
//! status, stdout and stderr byte for byte.
//!
//! Only mock servers on 127.0.0.1 are used; no case lets either binary go
//! past device selection into a device connection.

use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Version line printed by the reference build (`scripts/build-reference.sh`).
const C_VERSION_LINE: &[u8] = b"version: novacom-ref-d9d8b77\n";
/// Upper bound for one binary run.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// Poll interval of the mock server's accept loop and of child waits.
const POLL: Duration = Duration::from_millis(5);

fn reference() -> PathBuf {
    let path = std::env::var_os("ROVACOM_REFERENCE").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../reference/novacom/build-novacom/novacom"),
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

/// Runs `bin` with `argv[0] = arg0`, `args`, and `POSIXLY_CORRECT` set iff `posix`.
fn run(bin: &Path, arg0: &str, args: &[Vec<u8>], posix: bool) -> Outcome {
    let mut cmd = Command::new(bin);
    cmd.arg0(arg0)
        .args(args.iter().map(|a| OsString::from_vec(a.clone())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("POSIXLY_CORRECT");
    if posix {
        cmd.env("POSIXLY_CORRECT", "1");
    }
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
    let out = spawn_reader(child.stdout.take().expect("stdout pipe"));
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
6971 FEDCBA9876543210FEDCBA9876543210FEDCBA98 tcp Topaz-Linux sess123456789\n";

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
    check(
        &[BASIC],
        &[
            "-ts", "-w", "-r", "pw", "-P", "-f", "1:2,3:4", "-c", "list", "-l", "-p", "{P}",
        ],
    );
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

/// Paths outside this slice (C would connect to the device port) are
/// checked against rovacom only.
#[test]
fn not_implemented_rust_only() {
    for (extra, what) in [
        (&["run", "file:///bin/true"][..], "remote command execution"),
        (&["-c", "list"][..], "control command"),
        (&["-P"][..], "port forwarding"),
    ] {
        let mock = Mock::start(&[BASIC]);
        let mut parts = vec!["-p", "{P}"];
        parts.extend_from_slice(extra);
        let o = run(&rovacom(), "rovacom", &argv(&parts, mock.port), false);
        assert_eq!(o.status, "exit 70");
        assert!(o.stdout.is_empty());
        assert_eq!(
            o.stderr,
            format!("rovacom: {what} is not implemented in this build\n").into_bytes()
        );
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
