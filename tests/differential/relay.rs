//! Port forwarding (`-P -f local:remote,...`): differential tests.
//!
//! The relay runs with stdout on a pty (line-buffered, so the test can wait
//! for its lines) or on a pipe, stderr piped, and is ended with SIGTERM.
//! The device port is a mock on 127.0.0.1 that the test drives step by step
//! ([`LiveDevice`]); the local clients connect to 127.0.0.1. The relay
//! listens on free ports picked by the test (never 6971): C on INADDR_ANY,
//! rovacom on 127.0.0.1 only (intentional change).
//!
//! Thread order is nondeterministic: the copy threads of a tunnel print in
//! either order, and C and rovacom differ there too. Output is compared as
//! a [`View`]: the sorted multiset of lines plus the order of lines within
//! each thread (device to host, host to device, the rest; `started service
//! thread` only counted, since the service threads print it while the main
//! thread goes on).
//!
//! Tunnels are always ended so that C gives the same output on both OSes:
//! after one side ends, the test waits for the matching thread to exit and
//! then closes the other side too. On Linux the C thread whose socket the
//! other thread closed stays blocked until then; rovacom ends it at once
//! (intentional change B16, asserted for rovacom only).

use super::terminal::{Client, Conn, LiveDevice, Packet, Pty};
use super::*;

/// Upper bound for waiting on relay output and on socket data.
const WAIT: Duration = Duration::from_secs(10);
/// How long `-P` without `-f` must keep running.
const IDLE: Duration = Duration::from_secs(1);
/// C `buffer[0x2000]`: largest DATA payload sent by the relay.
const CHUNK: usize = 0x2000;
/// Port the tests must never use (novacomd control port).
const FORBIDDEN_PORT: u16 = 6971;

/// Relay ports are taken from this range, below the ephemeral port ranges
/// of Linux (32768-60999) and macOS (49152-65535): a port picked here and
/// released cannot be taken by another test's connection or port-0 bind
/// before the relay binds it.
const PORT_RANGE: std::ops::Range<u16> = 20_000..30_000;
static NEXT_PORT: AtomicUsize = AtomicUsize::new(0);

/// A free port for the relay to listen on: the next port of
/// [`PORT_RANGE`] (starting at an offset from the process id, so parallel
/// test processes rarely meet) that can be bound on all interfaces.
fn free_port() -> u16 {
    let len = usize::from(PORT_RANGE.end - PORT_RANGE.start);
    let base = std::process::id() as usize * 97;
    loop {
        let i = (base + NEXT_PORT.fetch_add(1, Ordering::Relaxed)) % len;
        let port = PORT_RANGE.start + u16::try_from(i).expect("in range");
        if port == FORBIDDEN_PORT {
            continue;
        }
        let _fds = fd_create_lock();
        if TcpListener::bind(("0.0.0.0", port)).is_ok() {
            return port;
        }
    }
}

/// Relay cases run one at a time: several relays and their threads at
/// once make the C client in `terminal::signals_forwarded` more likely to
/// drop a signal (its bug B4).
static RELAY_LOCK: Mutex<()> = Mutex::new(());

/// C `strerror(errno)` text as printed by `REPORT_ERROR`.
fn report(line: u32, errno: i32) -> String {
    format!(
        "src/tcprelay.c:{line}: {}",
        String::from_utf8_lossy(&rovacom::sys::strerror(errno))
    )
}

/// `errno` reported by a thread whose socket the other thread of the tunnel
/// closed (C) or shut down (rovacom): Darwin wakes it with EBADF; on Linux
/// it sees end of stream with `errno` still 0.
fn closed_errno() -> i32 {
    if cfg!(target_vendor = "apple") {
        libc::EBADF
    } else {
        0
    }
}

/// Whether the closing side of a tunnel reaches the other peer at once:
/// always for rovacom (B16); for C only on macOS.
fn fin_at_once(rust: bool) -> bool {
    rust || cfg!(target_vendor = "apple")
}

/// A running relay process.
struct RelayRun {
    client: Option<Client>,
    pty: Option<Pty>,
    rust: bool,
    /// Sockets kept open until the relay has been killed.
    keep: Vec<TcpStream>,
    _list: Mock,
}

impl RelayRun {
    /// Starts `bin -a 127.0.0.1 -p <list> -P <extra>`, with stdout on a pty
    /// if `pty`, else on a pipe.
    fn start(bin: &Path, list: Mock, extra: &[String], pty: bool) -> Self {
        let mut args = argv(&["-a", "127.0.0.1", "-p", "{P}", "-P"], list.port);
        args.extend(extra.iter().map(|a| a.clone().into_bytes()));
        assert!(
            !String::from_utf8_lossy(&args.concat()).contains(&FORBIDDEN_PORT.to_string()),
            "port 6971 must not be used"
        );
        let pty = pty.then(Pty::open);
        let client = Client::spawn(
            bin,
            &args,
            Stdio::null(),
            pty.as_ref().map(Pty::slave),
            false,
        );
        Self {
            client: Some(client),
            pty,
            rust: bin == rovacom(),
            keep: Vec::new(),
            _list: list,
        }
    }

    fn output(&self) -> Vec<u8> {
        self.pty.as_ref().map(Pty::output).unwrap_or_default()
    }

    /// Waits until the pty output holds `needle` at least `count` times.
    fn wait_for(&self, needle: &str, count: usize) {
        let deadline = Instant::now() + WAIT;
        loop {
            let out = String::from_utf8_lossy(&self.output()).into_owned();
            if out.matches(needle).count() >= count {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "waited for {count} x {needle:?}; output so far: {out:?}"
            );
            std::thread::sleep(POLL);
        }
    }

    fn client(&mut self) -> &mut Client {
        self.client.as_mut().expect("client running")
    }

    /// Sends SIGTERM and collects (status, stdout, stderr).
    fn finish(mut self) -> (String, Vec<u8>, Vec<u8>) {
        let client = self.client.take().expect("client running");
        client.kill(libc::SIGTERM);
        let (status, out, err) = client.finish();
        self.keep.clear();
        let out = match self.pty.take() {
            Some(p) => p.finish(),
            None => out,
        };
        (status, out, err)
    }
}

impl Drop for RelayRun {
    /// A failed test must not leave the relay running.
    fn drop(&mut self) {
        if let Some(mut c) = self.client.take() {
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
    }
}

/// Connects a local client to the relay's port.
fn local(port: u16) -> TcpStream {
    let s = {
        let _fds = fd_create_lock();
        TcpStream::connect(("127.0.0.1", port)).expect("connect to relay")
    };
    s.set_read_timeout(Some(WAIT)).expect("timeout");
    s.set_nodelay(true).expect("nodelay");
    s
}

fn read_n(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).expect("read from relay");
    buf
}

/// Asserts that `s` reaches end of stream (no more data) within [`WAIT`].
fn assert_eof(s: &mut TcpStream, what: &str) {
    s.set_read_timeout(Some(WAIT)).expect("timeout");
    let mut byte = [0u8; 1];
    match s.read(&mut byte) {
        Ok(0) => {}
        Ok(_) => panic!("{what}: data instead of end of stream"),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("{what}: no end of stream: {e}"),
    }
}

/// Reads DATA packets until `n` payload bytes arrived: (payload, sizes).
fn upload(conn: &mut Conn, n: usize) -> (Vec<u8>, Vec<usize>) {
    let (mut data, mut sizes) = (Vec::new(), Vec::new());
    while data.len() < n {
        match conn.packet() {
            Packet::Data(d) => {
                sizes.push(d.len());
                data.extend(d);
            }
            p => panic!("expected DATA, got {p:?}"),
        }
    }
    (data, sizes)
}

/// Accepts tunnel number `n` (1-based): local client, device connection
/// with the command read, `ok` sent, both copy threads started.
fn open_tunnel(run: &RelayRun, dev: &LiveDevice, port: u16, n: usize) -> (TcpStream, Conn) {
    let c = local(port);
    let mut conn = dev.accept();
    conn.send(b"ok\n");
    run.wait_for("device to host thread started", n);
    run.wait_for("host to device thread started", n);
    (c, conn)
}

/// Ends tunnel `n` from the local side: the client closes; once the host to
/// device thread has exited the device sees end of stream (C on Linux:
/// only when the device closes, so it is checked where it happens at once)
/// and closes too.
fn end_from_local(run: &RelayRun, c: TcpStream, mut conn: Conn, n: usize) {
    drop(c);
    run.wait_for("host to device thread exited", n);
    if fin_at_once(run.rust) {
        assert_eof(&mut conn.s, "device after local close");
    }
    drop(conn);
    run.wait_for("device to host thread exited", n);
}

/// Ends tunnel `n` after the device side ended it (OOB, bad packet, close):
/// once the device to host thread has exited the local client sees end of
/// stream (C on Linux: never, so it is checked where it happens at once)
/// and closes.
fn end_from_device(run: &RelayRun, mut c: TcpStream, conn: Option<Conn>, n: usize) {
    run.wait_for("device to host thread exited", n);
    if fin_at_once(run.rust) {
        assert_eof(&mut c, "local client after device end");
    }
    drop(c);
    run.wait_for("host to device thread exited", n);
    drop(conn);
}

/// Output lines grouped for comparison (see the module comment).
#[derive(Debug, PartialEq, Eq)]
struct View {
    sorted: Vec<String>,
    device_to_host: Vec<String>,
    host_to_device: Vec<String>,
    /// Count of `started service thread`: each service thread prints it
    /// while the main thread goes on with the next pair, so its position is
    /// nondeterministic.
    started: usize,
    rest: Vec<String>,
}

fn view(bytes: &[u8]) -> View {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let has = |l: &str, sites: &[u32]| {
        sites
            .iter()
            .any(|s| l.starts_with(&format!("src/tcprelay.c:{s}:")))
    };
    let mut v = View {
        sorted: lines.clone(),
        device_to_host: Vec::new(),
        host_to_device: Vec::new(),
        started: 0,
        rest: Vec::new(),
    };
    v.sorted.sort();
    for l in lines {
        if l.starts_with("device to host") || l == "bad packet!\n" || has(&l, &[174, 191, 197]) {
            v.device_to_host.push(l);
        } else if l.starts_with("host to device") || has(&l, &[224, 229, 247, 252]) {
            v.host_to_device.push(l);
        } else if l == "started service thread\n" {
            v.started += 1;
        } else {
            v.rest.push(l);
        }
    }
    v
}

/// Lines of `bytes` with pty line ends turned into `\n`.
fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .split_inclusive('\n')
        .map(str::to_string)
        .collect()
}

/// B9: C prints its uninitialized `response` buffer after this text;
/// rovacom prints nothing there. The C bytes up to the line end are removed.
const B9_TEXT: &[u8] = b"error attempting to tunnel socket: got response ";

fn normalize_b9(out: &[u8]) -> Vec<u8> {
    let mut res = Vec::with_capacity(out.len());
    let mut rest = out;
    while let Some(i) = rest.windows(B9_TEXT.len()).position(|w| w == B9_TEXT) {
        let end = i + B9_TEXT.len();
        res.extend_from_slice(&rest[..end]);
        let tail = &rest[end..];
        let skip = tail
            .iter()
            .position(|&b| b == b'\r' || b == b'\n')
            .unwrap_or(tail.len());
        rest = &tail[skip..];
    }
    res.extend_from_slice(rest);
    res
}

/// Outcome plus raw stdout and stderr of one run.
type CaseResult = (RelayOutcome, Vec<u8>, Vec<u8>);

#[derive(Debug, PartialEq, Eq)]
struct RelayOutcome {
    status: String,
    stdout: View,
    stderr: View,
    /// Bytes seen by the mock peers (case-specific).
    data: Vec<Vec<u8>>,
}

/// Replaces each local port number (they differ between the C and the
/// rovacom run) with `L<index>`.
fn normalize_ports(bytes: &[u8], ports: &[u16]) -> Vec<u8> {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    for (i, p) in ports.iter().enumerate() {
        text = text.replace(&p.to_string(), &format!("L{i}"));
    }
    text.into_bytes()
}

fn outcome(
    (status, out, err): (String, Vec<u8>, Vec<u8>),
    ports: &[u16],
    data: Vec<Vec<u8>>,
) -> RelayOutcome {
    RelayOutcome {
        status,
        stdout: view(&normalize_ports(&normalize_b9(&out), ports)),
        stderr: view(&normalize_ports(&err, ports)),
        data,
    }
}

/// Runs `case` with C, then with rovacom, and compares. Returns the
/// rovacom outcome and its raw stdout and stderr.
fn diff(case: fn(&Path) -> CaseResult) -> CaseResult {
    let (c, ..) = case(&reference());
    let r = case(&rovacom());
    assert_eq!(r.0, c);
    r
}

/// Runs a case: `body` drives the started relay and returns the data.
fn run_case(
    bin: &Path,
    list: Mock,
    ports: &[u16],
    extra: &[String],
    pty: bool,
    body: impl FnOnce(&mut RelayRun) -> Vec<Vec<u8>>,
) -> CaseResult {
    let _serial = RELAY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let mut run = RelayRun::start(bin, list, extra, pty);
    let data = body(&mut run);
    let raw = run.finish();
    let (out, err) = (raw.1.clone(), raw.2.clone());
    (outcome(raw, ports, data), out, err)
}

fn forward(pairs: &[(u16, u32)]) -> Vec<String> {
    let spec: Vec<String> = pairs.iter().map(|(l, r)| format!("{l}:{r}")).collect();
    vec!["-f".to_string(), spec.join(",")]
}

/// The lines printed before any tunnel, for one service per pair.
fn startup(pairs: &[(u16, u32)], first_fd: u32) -> Vec<String> {
    let mut v = Vec::new();
    for (i, (l, r)) in pairs.iter().enumerate() {
        v.push(format!("Forwarding Port {l}<->{r}\n"));
        v.push(format!(
            "created service fd {} local: {l} remote {r}\n",
            first_fd + i as u32
        ));
        v.push("started service thread\n".to_string());
    }
    v
}

// ---------------------------------------------------------------------------
// Cases.
// ---------------------------------------------------------------------------

/// One forward: data both ways (uploads in DATA packets of at most 0x2000
/// bytes), an ERR packet dropped, an empty OOB packet ignored (C reads no
/// body, so the tunnel continues), then OOB ends the tunnel.
fn round_trip_case(bin: &Path) -> CaseResult {
    let dev = LiveDevice::bind();
    let port = free_port();
    run_case(
        bin,
        dev.devlist(),
        &[port],
        &forward(&[(port, 23)]),
        true,
        |run| {
            run.wait_for("created service fd", 1);
            let (mut c, mut conn) = open_tunnel(run, &dev, port, 1);
            let command = conn.command.clone();
            c.write_all(b"hello").expect("write");
            let (small, _) = upload(&mut conn, 5);
            let big: Vec<u8> = Vec::from_fn(2 * CHUNK + 100, |i| (i % 251) as u8);
            c.write_all(&big).expect("write");
            let (up, sizes) = upload(&mut conn, big.len());
            assert!(sizes.iter().all(|&n| (1..=CHUNK).contains(&n)), "{sizes:?}");
            conn.send(&pkt(T_DATA, b"world"));
            let down1 = read_n(&mut c, 5);
            conn.send(&pkt(T_ERR, b"dropped"));
            conn.send(&hdr(MAGIC, 1, 0, T_OOB));
            conn.send(&pkt(T_DATA, b"after"));
            let down2 = read_n(&mut c, 5);
            let big_down: Vec<u8> = Vec::from_fn(3 * CHUNK + 7, |i| (i % 241) as u8);
            conn.send(&pkt(T_DATA, &big_down));
            let down3 = read_n(&mut c, big_down.len());
            conn.send(&oob(OOB_EOF, 0));
            end_from_device(run, c, Some(conn), 1);
            vec![command, small, up, down1, down2, down3]
        },
    )
}

#[test]
fn relay_round_trip() {
    let (r, out, err) = diff(round_trip_case);
    assert_eq!(r.status, "signal 15");
    assert_eq!(r.data[0], b"connect tcp-port://23\n");
    assert_eq!(r.data[1], b"hello");
    assert_eq!(r.data[3], b"world");
    assert_eq!(r.data[4], b"after");
    let lines = lines(&out);
    assert_eq!(lines[..3], startup(&[(free_port_of(&lines), 23)], 3)[..]);
    assert_eq!(lines[3..5], ["connect tcp-port://23\n", "\n"]);
    assert_eq!(r.stdout.device_to_host.len(), 2);
    assert_eq!(r.stdout.host_to_device.len(), 2);
    assert_eq!(
        String::from_utf8_lossy(&err),
        format!("{}\n", report(247, closed_errno()))
    );
}

/// The local port printed in the first output line.
fn free_port_of(lines: &[String]) -> u16 {
    lines[0]
        .trim_start_matches("Forwarding Port ")
        .split("<->")
        .next()
        .and_then(|p| p.parse().ok())
        .expect("port in first line")
}

/// Two forwards; a tunnel on each, ended by the local client (EOF:
/// REPORT_ERROR at line 247 with `errno` 0).
fn two_forwards_case(bin: &Path) -> CaseResult {
    let dev = LiveDevice::bind();
    let ports = [free_port(), free_port()];
    let pairs = [(ports[0], 2201), (ports[1], 2202)];
    run_case(bin, dev.devlist(), &ports, &forward(&pairs), true, |run| {
        run.wait_for("started service thread", 2);
        let mut data = Vec::new();
        for (i, port) in ports.into_iter().enumerate() {
            let (mut c, mut conn) = open_tunnel(run, &dev, port, i + 1);
            data.push(conn.command.clone());
            c.write_all(format!("up{i}").as_bytes()).expect("write");
            data.push(upload(&mut conn, 3).0);
            conn.send(&pkt(T_DATA, format!("down{i}").as_bytes()));
            data.push(read_n(&mut c, 5));
            end_from_local(run, c, conn, i + 1);
        }
        data
    })
}

#[test]
fn relay_two_forwards_local_close() {
    let (r, _, err) = diff(two_forwards_case);
    assert_eq!(r.status, "signal 15");
    assert_eq!(r.data[0], b"connect tcp-port://2201\n");
    assert_eq!(r.data[3], b"connect tcp-port://2202\n");
    assert_eq!((r.stdout.started, r.stdout.rest.len()), (2, 4 + 4));
    // Darwin: the device to host thread is woken by the close with EBADF.
    let mut expected = String::new();
    for _ in 0..2 {
        expected.push_str(&format!("{}\n", report(247, 0)));
        if cfg!(target_vendor = "apple") {
            expected.push_str(&format!("{}\n", report(174, libc::EBADF)));
        }
    }
    assert_eq!(String::from_utf8_lossy(&err), expected);
}

/// The device closes between packets.
fn device_close_case(bin: &Path) -> CaseResult {
    let dev = LiveDevice::bind();
    let port = free_port();
    run_case(
        bin,
        dev.devlist(),
        &[port],
        &forward(&[(port, 23)]),
        true,
        |run| {
            run.wait_for("created service fd", 1);
            let (mut c, mut conn) = open_tunnel(run, &dev, port, 1);
            conn.send(&pkt(T_DATA, b"bye"));
            let down = read_n(&mut c, 3);
            drop(conn);
            end_from_device(run, c, None, 1);
            vec![down]
        },
    )
}

#[test]
fn relay_device_close() {
    let (r, _, err) = diff(device_close_case);
    assert_eq!(r.data, [b"bye".to_vec()]);
    assert_eq!(
        String::from_utf8_lossy(&err),
        format!("{}\n", report(247, closed_errno()))
    );
}

/// Bad magic, then (second tunnel) bad version: `bad packet!`.
fn bad_packet_case(bin: &Path) -> CaseResult {
    let dev = LiveDevice::bind();
    let port = free_port();
    run_case(
        bin,
        dev.devlist(),
        &[port],
        &forward(&[(port, 23)]),
        true,
        |run| {
            run.wait_for("created service fd", 1);
            for (i, header) in [hdr(0xDEAD_BEEF, 1, 0, T_DATA), hdr(MAGIC, 2, 0, T_DATA)]
                .into_iter()
                .enumerate()
            {
                let (c, mut conn) = open_tunnel(run, &dev, port, i + 1);
                conn.send(&header);
                end_from_device(run, c, Some(conn), i + 1);
            }
            Vec::new()
        },
    )
}

#[test]
fn relay_bad_packet() {
    let (r, ..) = diff(bad_packet_case);
    let bad: Vec<&String> = r
        .stdout
        .device_to_host
        .iter()
        .filter(|l| *l == "bad packet!\n")
        .collect();
    assert_eq!(bad.len(), 2);
}

/// Daemon replies other than `ok`: parse_response messages on stderr and
/// the B9 line on stdout; the local client is closed.
fn reject_case(bin: &Path) -> CaseResult {
    let dev = LiveDevice::bind();
    let port = free_port();
    run_case(
        bin,
        dev.devlist(),
        &[port],
        &forward(&[(port, 23)]),
        true,
        |run| {
            run.wait_for("created service fd", 1);
            let replies: [Option<&[u8]>; 4] = [
                Some(b"req:auth\n"),
                Some(b"nope\n"),
                Some(b"unrecognized command\n"),
                None,
            ];
            let mut data = Vec::new();
            for (i, reply) in replies.into_iter().enumerate() {
                let mut c = local(port);
                let mut conn = dev.accept();
                data.push(conn.command.clone());
                match reply {
                    Some(r) => conn.send(r),
                    None => drop(conn),
                }
                assert_eof(&mut c, "rejected local client");
                run.wait_for("error attempting to tunnel socket", i + 1);
            }
            data
        },
    )
}

#[test]
fn relay_daemon_rejects() {
    let (r, out, err) = diff(reject_case);
    assert_eq!(
        String::from_utf8_lossy(&err),
        "password required\n\
         bad or error response from other side: 'nope'\n\
         unknown command\n\
         bad or error response from other side: ''\n"
    );
    let tunnel = [
        "connect tcp-port://23\n",
        "\n",
        "error attempting to tunnel socket: got response \n",
    ];
    assert_eq!(lines(&out)[3..], tunnel.repeat(4)[..]);
    assert_eq!(r.data.len(), 4);
}

/// The device port refuses connections: the local client is closed
/// silently.
fn refused_case(bin: &Path) -> CaseResult {
    let line = format!("{} n usb castle\n", refused_port());
    let port = free_port();
    run_case(
        bin,
        Mock::start(&[line.as_bytes()]),
        &[port],
        &forward(&[(port, 23)]),
        true,
        |run| {
            run.wait_for("started service thread", 1);
            for _ in 0..2 {
                assert_eof(&mut local(port), "local client, device refused");
            }
            Vec::new()
        },
    )
}

#[test]
fn relay_device_refused() {
    let (_, out, err) = diff(refused_case);
    let lines = lines(&out);
    assert_eq!(lines, startup(&[(free_port_of(&lines), 23)], 3));
    assert!(err.is_empty());
}

/// The first local port is in use: REPORT_ERROR at line 333, the pair is
/// skipped and the next one gets the same fd number.
fn busy_case(bin: &Path) -> CaseResult {
    // Held on the address the relay binds: on macOS a 127.0.0.1 listener
    // would not block C's wildcard bind, and a wildcard listener would not
    // block rovacom's 127.0.0.1 bind.
    let busy_port = free_port();
    let host = if bin == rovacom() {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    let busy = {
        let _fds = fd_create_lock();
        TcpListener::bind((host, busy_port)).expect("bind busy port")
    };
    let port = free_port();
    let res = run_case(
        bin,
        Mock::start(&[b"7000 n usb castle\n"]),
        &[busy_port, port],
        &forward(&[(busy_port, 1), (port, 2)]),
        true,
        |run| {
            run.wait_for("started service thread", 1);
            Vec::new()
        },
    );
    drop(busy);
    res
}

#[test]
fn relay_listen_port_busy() {
    let (_, out, err) = diff(busy_case);
    let lines = lines(&out);
    let busy = free_port_of(&lines);
    let port: u16 = lines[1]
        .trim_start_matches("Forwarding Port ")
        .split("<->")
        .next()
        .and_then(|p| p.parse().ok())
        .expect("second port");
    let mut expected = vec![format!("Forwarding Port {busy}<->1\n")];
    expected.extend(startup(&[(port, 2)], 3));
    assert_eq!(lines, expected);
    assert_eq!(
        String::from_utf8_lossy(&err),
        format!("{}\n", report(333, libc::EADDRINUSE))
    );
}

/// `-P` without `-f`: no output, runs until killed.
fn idle_case(bin: &Path) -> CaseResult {
    run_case(
        bin,
        Mock::start(&[b"7000 n usb castle\n"]),
        &[],
        &[],
        true,
        |run| {
            std::thread::sleep(IDLE);
            run.client().assert_running();
            Vec::new()
        },
    )
}

#[test]
fn relay_without_forwards() {
    let (r, out, err) = diff(idle_case);
    assert_eq!(r.status, "signal 15");
    assert!(out.is_empty() && err.is_empty());
}

/// stdout on a pipe: fully buffered, so nothing is printed before the kill
/// and the buffered lines are lost.
fn pipe_case(bin: &Path) -> CaseResult {
    let dev = LiveDevice::bind();
    let port = free_port();
    run_case(
        bin,
        dev.devlist(),
        &[port],
        &forward(&[(port, 23)]),
        false,
        |run| {
            // No output to wait for: retry until the listener exists.
            let deadline = Instant::now() + WAIT;
            let mut c = loop {
                let s = {
                    let _fds = fd_create_lock();
                    TcpStream::connect(("127.0.0.1", port))
                };
                match s {
                    Ok(s) => break s,
                    Err(_) if Instant::now() < deadline => std::thread::sleep(POLL),
                    Err(e) => panic!("relay never listened: {e}"),
                }
            };
            c.set_read_timeout(Some(WAIT)).expect("timeout");
            let mut conn = dev.accept();
            conn.send(b"ok\n");
            c.write_all(b"ping").expect("write");
            let up = upload(&mut conn, 4).0;
            conn.send(&pkt(T_DATA, b"pong"));
            let down = read_n(&mut c, 4);
            // Closing now would race with the kill.
            run.keep.extend([c, conn.s]);
            vec![conn.command, up, down]
        },
    )
}

#[test]
fn relay_stdout_pipe() {
    let (r, out, err) = diff(pipe_case);
    assert_eq!(r.status, "signal 15");
    assert_eq!(r.data[1..], [b"ping".to_vec(), b"pong".to_vec()]);
    assert!(out.is_empty() && err.is_empty());
}

/// Intentional change B15, rovacom only: the device closes in the middle of
/// a packet body. C (`readComplete` returns 0, `send(..., 0)` "succeeds")
/// loops forever at full CPU and never closes the local client; rovacom
/// ends the tunnel. The partial body is dropped, as C `readComplete` does.
#[test]
fn relay_close_mid_body_rust_only() {
    let dev = LiveDevice::bind();
    let port = free_port();
    let (_, out, err) = run_case(
        &rovacom(),
        dev.devlist(),
        &[port],
        &forward(&[(port, 23)]),
        true,
        |run| {
            run.wait_for("created service fd", 1);
            let (c, mut conn) = open_tunnel(run, &dev, port, 1);
            conn.send(&[hdr(MAGIC, 1, 100, T_DATA), b"0123456789".to_vec()].concat());
            drop(conn);
            // `end_from_device` asserts end of stream with no data.
            end_from_device(run, c, None, 1);
            Vec::new()
        },
    );
    let lines = lines(&out);
    assert_eq!(lines.len(), 9, "{lines:?}");
    assert!(lines.contains(&"device to host thread exited\n".to_string()));
    assert_eq!(
        String::from_utf8_lossy(&err),
        format!("{}\n", report(247, closed_errno()))
    );
}

/// Intentional change: when `accept` fails (here `EMFILE`: with the fd
/// limit at 4, the listener takes the last free fd), the service thread
/// waits before it tries again. On Linux the connection stays queued, so C
/// fails again at once, uses a full CPU and prints the error about 10^5
/// times per second (macOS drops the connection: one error). rovacom only.
#[test]
fn relay_accept_error_backs_off() {
    let _serial = RELAY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let list = Mock::start(&[b"7000 n usb castle\n"]);
    let port = free_port();
    let mut args = vec![
        b"-c".to_vec(),
        b"ulimit -n 4 && exec \"$0\" \"$@\"".to_vec(),
        rovacom().into_os_string().into_vec(),
    ];
    args.extend(argv(&["-a", "127.0.0.1", "-p", "{P}", "-P"], list.port));
    args.extend(forward(&[(port, 22)]).into_iter().map(String::into_bytes));
    let client = Client::spawn(
        Path::new("/bin/sh"),
        &args,
        Stdio::null(),
        Some(Stdio::null()),
        false,
    );
    let deadline = Instant::now() + WAIT;
    let _conn = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(c) => break c,
            Err(e) => assert!(Instant::now() < deadline, "connect to relay: {e}"),
        }
        std::thread::sleep(POLL);
    };
    std::thread::sleep(IDLE);
    client.kill(libc::SIGTERM);
    let (_, _, err) = client.finish();
    let err = String::from_utf8_lossy(&err);
    let reports = err.matches(&report(273, libc::EMFILE)).count();
    assert!(
        (1..=20).contains(&reports),
        "{reports} accept errors in {IDLE:?}; stderr starts: {:?}",
        err.chars().take(200).collect::<String>()
    );
}
