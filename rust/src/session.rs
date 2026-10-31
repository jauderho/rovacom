//! Remote command session: C `parse_response` and `data_xfer` (non-tty path).
//!
//! stdin, stdout and stderr are used as raw fds 0, 1 and 2 (no std
//! buffering): buffered stdin would hide data from `select`, and bytes held in
//! a std buffer would be lost by `close(1)`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::devlist::{c_str, read_line};
use crate::packet::{self, Decoder, HEADER_LEN, Recv};
use crate::sys::{self, FdWriter, STDERR_FD, STDIN_FD, STDOUT_FD};

/// C `response_buf[2048]`.
const RESPONSE_BUFLEN: usize = 2048;
/// C `NOVACOM_MAX_PACKETSIZE`: largest stdin chunk per DATA packet.
const MAX_PACKET_PAYLOAD: usize = 4096;
/// C `select` timeout while stdin is watched.
const STDIN_POLL_TIMEOUT: Duration = Duration::from_millis(5);

/// Builds the command line: operands joined by one space, then `\n`.
///
/// Intentional change B1: C `strcat`s into a 4096-byte buffer, which
/// overflows (undefined behavior) beyond 4095 bytes. Here there is no length
/// limit and no error; this is what C sends when the overflow is harmless.
pub fn build_command(operands: &[Vec<u8>]) -> Vec<u8> {
    let mut cmd = operands.join(&b' ');
    cmd.push(b'\n');
    cmd
}

/// Emulates C `parse_response`: reads one response line (at most 2047
/// bytes, one byte per `recv`, so no packet data is consumed) and checks it.
/// Returns the stderr message on failure.
pub fn parse_response<R: Read>(r: &mut R) -> Result<(), Vec<u8>> {
    let line = read_line(r, RESPONSE_BUFLEN);
    let line = c_str(&line);
    if line.starts_with(b"req:auth") {
        return Err(b"password required\n".to_vec());
    }
    if line.starts_with(b"ok") {
        return Ok(());
    }
    if line.starts_with(b"unrecognized command") {
        return Err(b"unknown command\n".to_vec());
    }
    Err([
        &b"bad or error response from other side: '"[..],
        line,
        b"'\n",
    ]
    .concat())
}

/// Packet in `wbuf` waiting to be sent (C `wsize`, `written`).
#[derive(Clone, Copy)]
struct Pending {
    len: usize,
    written: usize,
}

/// Emulates C `data_xfer` without terminal or signal forwarding.
///
/// Returns the remote return code, or -1 on abnormal termination.
pub fn data_xfer(sock: &TcpStream) -> i32 {
    // C ignores a failing fcntl here.
    let _ = sock.set_nonblocking(true);
    let mut out = FdWriter(STDOUT_FD);
    let mut err = FdWriter(STDERR_FD);
    let mut decoder = Decoder::new();
    let mut diag = Vec::new();
    let mut listen_stdio = true;
    let mut stdout_closed = false;
    let mut wbuf = vec![0u8; HEADER_LEN + MAX_PACKET_PAYLOAD];
    let mut pending: Option<Pending> = None;

    loop {
        let want_write = pending.is_some();
        let want_stdin = !want_write && listen_stdio;
        // C uses a zero timeout (a busy loop) when stdin is not watched;
        // blocking instead has the same observable behavior.
        let timeout = want_stdin.then_some(STDIN_POLL_TIMEOUT);
        let ready = match sys::select(sock, want_stdin, want_write, timeout) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            // C goes on with undefined fd sets; stop instead.
            Err(_) => return -1,
        };

        if ready.sock_read {
            diag.clear();
            let result = decoder.poll(&mut |b| (&mut &*sock).read(b), &mut diag);
            let _ = err.write_all(&diag);
            match result {
                Recv::BadData => {
                    let _ = err.write_all(b"\nbad data from other end\n");
                    return -1;
                }
                Recv::ClosedSocket => {
                    let _ = err.write_all(b"\nnovacomd socket was closed prematurely\n");
                    return -1;
                }
                Recv::NoData | Recv::CloseStderr => {}
                Recv::Stdout(data) => {
                    if stdout_closed {
                        let _ = err.write_all(b"\nReceived data for stdout after closing stdout\n");
                    } else {
                        // Intentional change B7: full writes (C ignores short writes).
                        let _ = out.write_all(data);
                    }
                }
                Recv::Stderr(data) => {
                    let _ = err.write_all(data);
                }
                Recv::CloseStdout => {
                    sys::close(STDOUT_FD);
                    stdout_closed = true;
                }
                Recv::Return(rc) => return rc,
            }
        }

        if let (true, Some(mut p)) = (ready.sock_write, pending) {
            pending = match sys::send(sock, &wbuf[p.written..p.len]) {
                Ok(n) if n > 0 => {
                    p.written += n;
                    (p.written < p.len).then_some(p)
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Some(p),
                // 0 or another error: drop the packet and stop reading stdin.
                // C: "Don't bail, drop through in case we can read".
                _ => {
                    listen_stdio = false;
                    None
                }
            };
        }

        if pending.is_some() {
            continue;
        }

        if ready.stdin_read {
            let n = sys::read(STDIN_FD, &mut wbuf[HEADER_LEN..]).unwrap_or(0);
            let len = if n == 0 {
                // stdin went empty (or failed): stop listening on it.
                listen_stdio = false;
                wbuf[..packet::OOB_PACKET_LEN].copy_from_slice(&packet::encode_eof(STDIN_FD));
                sys::close(STDIN_FD);
                packet::OOB_PACKET_LEN
            } else {
                // n <= MAX_PACKET_PAYLOAD, so it fits in u32.
                let size = u32::try_from(n).unwrap_or(u32::MAX);
                wbuf[..HEADER_LEN].copy_from_slice(&packet::encode_data_header(size));
                HEADER_LEN + n
            };
            pending = Some(Pending { len, written: 0 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line() {
        assert_eq!(build_command(&[b"run".to_vec()]), b"run\n");
        assert_eq!(
            build_command(&[b"run".to_vec(), b"file:///bin/ls".to_vec(), b"".to_vec()]),
            b"run file:///bin/ls \n"
        );
        let long = vec![b'x'; 10_000];
        assert_eq!(build_command(&[long]).len(), 10_001);
    }

    fn resp(bytes: &[u8]) -> Result<(), String> {
        parse_response(&mut &bytes[..]).map_err(|e| String::from_utf8_lossy(&e).into_owned())
    }

    #[test]
    fn responses() {
        assert_eq!(resp(b"ok\n"), Ok(()));
        assert_eq!(resp(b"okay whatever\nmore"), Ok(()));
        assert_eq!(resp(b"ok\0junk\n"), Ok(()));
        assert_eq!(resp(b"req:auth\n"), Err("password required\n".into()));
        assert_eq!(resp(b"req:authx"), Err("password required\n".into()));
        assert_eq!(
            resp(b"unrecognized command foo\n"),
            Err("unknown command\n".into())
        );
        let bad = |s: &str| Err(format!("bad or error response from other side: '{s}'\n"));
        assert_eq!(resp(b"o\n"), bad("o"));
        assert_eq!(resp(b""), bad(""));
        assert_eq!(resp(b"\n"), bad(""));
        assert_eq!(resp(b"o\0k\n"), bad("o"));
        assert_eq!(resp(b"req:aut\n"), bad("req:aut"));
        let long = vec![b'y'; 3000];
        assert_eq!(resp(&long), bad(&"y".repeat(2047)));
    }

    #[test]
    fn response_reads_only_one_line() {
        let data = b"ok\nPACKET";
        let mut r = &data[..];
        assert_eq!(parse_response(&mut r), Ok(()));
        assert_eq!(r, b"PACKET");
        let long = vec![b'z'; 2100];
        let mut r = &long[..];
        assert!(parse_response(&mut r).is_err());
        assert_eq!(r.len(), 2100 - 2047);
    }
}
