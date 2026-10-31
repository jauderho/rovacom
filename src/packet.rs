//! Stream framing (C `packet.c`, `packet_struct.h`): packet encoders and an
//! exact emulation of the incremental decoder `packet_recv_something`.
//!
//! C uses host byte order; every supported target is little-endian, so this
//! module encodes and decodes explicit little-endian (same bytes on the wire).
//! Pure code: the decoder reads through a caller-supplied receive function and
//! returns its diagnostics to the caller.

use std::io;

pub const MAGIC: u32 = 0xDECA_FBAD;
pub const VERSION: u32 = 1;
/// `sizeof(struct packet_header)`.
pub const HEADER_LEN: usize = 16;
/// `sizeof(struct packet_oob_msg)`: u32 message + 16-byte union.
pub const OOB_LEN: usize = 20;
/// Length of a header plus an OOB message.
pub const OOB_PACKET_LEN: usize = HEADER_LEN + OOB_LEN;

pub const TYPE_DATA: u32 = 0;
pub const TYPE_ERR: u32 = 1;
pub const TYPE_OOB: u32 = 2;

pub const OOB_EOF: u32 = 0;
pub const OOB_SIGNAL: u32 = 1;
pub const OOB_RETURN: u32 = 2;
pub const OOB_RESIZE: u32 = 3;

/// C `STDOUT_FILENO`, compared with the fileno of an OOB EOF.
const STDOUT_FILENO: i32 = 1;
/// Size of the C static `packet_buf[64*1024]`.
pub const RECV_BUF_LEN: usize = 64 * 1024;

fn header(size: u32, kind: u32) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    for (i, v) in [MAGIC, VERSION, size, kind].into_iter().enumerate() {
        h[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    h
}

/// C `prepare_packet_data`: the header of a DATA packet whose `size`-byte
/// payload follows it.
pub fn encode_data_header(size: u32) -> [u8; HEADER_LEN] {
    header(size, TYPE_DATA)
}

/// An OOB packet with the two union words `a` and `b`.
///
/// Intentional change B5: the unused union bytes are zero (C sends
/// uninitialized heap bytes there).
fn oob(message: u32, a: u32, b: u32) -> [u8; OOB_PACKET_LEN] {
    let mut p = [0u8; OOB_PACKET_LEN];
    p[..HEADER_LEN].copy_from_slice(&header(OOB_LEN as u32, TYPE_OOB));
    p[16..20].copy_from_slice(&message.to_le_bytes());
    p[20..24].copy_from_slice(&a.to_le_bytes());
    p[24..28].copy_from_slice(&b.to_le_bytes());
    p
}

/// C `prepare_packet_eof`.
pub fn encode_eof(fileno: i32) -> [u8; OOB_PACKET_LEN] {
    oob(OOB_EOF, u32::from_le_bytes(fileno.to_le_bytes()), 0)
}

/// C `prepare_packet_signal`.
pub fn encode_signal(signo: u32) -> [u8; OOB_PACKET_LEN] {
    oob(OOB_SIGNAL, signo, 0)
}

/// C `prepare_packet_term_resize`.
pub fn encode_resize(rows: u32, cols: u32) -> [u8; OOB_PACKET_LEN] {
    oob(OOB_RESIZE, rows, cols)
}

/// Result of one [`Decoder::poll`] (C `enum packet_recv_code`).
#[derive(Debug, PartialEq, Eq)]
pub enum Recv<'a> {
    NoData,
    Stdout(&'a [u8]),
    Stderr(&'a [u8]),
    CloseStdout,
    CloseStderr,
    Return(i32),
    BadData,
    ClosedSocket,
}

/// Outcome of one receive call.
enum Got {
    Bytes(usize),
    /// `EAGAIN` / `EINTR`.
    Again,
    /// Zero bytes (orderly close, or a zero-length request) or an error.
    Closed,
}

fn receive(recv: &mut dyn FnMut(&mut [u8]) -> io::Result<usize>, buf: &mut [u8]) -> Got {
    // C `recv(fd, buf, 0, 0)` returns 0 without looking at the socket; the
    // caller treats that as a closed socket (quirk B3).
    if buf.is_empty() {
        return Got::Closed;
    }
    match recv(buf) {
        Ok(0) => Got::Closed,
        Ok(n) => Got::Bytes(n),
        // Intentional change B2: C treats EAGAIN (and EINTR) as a closed
        // socket; here it is "no data yet" and all state is kept.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Got::Again
        }
        Err(_) => Got::Closed,
    }
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(w)
}

/// `u32` reinterpreted as C `int` (for `%d` and `int32_t` fields).
fn as_i32(v: u32) -> i32 {
    i32::from_le_bytes(v.to_le_bytes())
}

/// Emulation of the static state of C `packet_recv_something`.
pub struct Decoder {
    header: [u8; HEADER_LEN],
    headercounter: usize,
    /// C `size_t bodycounter`.
    bodycounter: u64,
    msg: [u8; OOB_LEN],
    buf: Vec<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            header: [0; HEADER_LEN],
            headercounter: 0,
            bodycounter: 0,
            msg: [0; OOB_LEN],
            buf: vec![0; RECV_BUF_LEN],
        }
    }

    fn size(&self) -> u32 {
        le_u32(&self.header, 8)
    }

    /// C `MIN(h.size - bodycounter, sizeof(packet_buf))` in `size_t`.
    fn bulk_len(&self) -> usize {
        let left = u64::from(self.size()).wrapping_sub(self.bodycounter);
        usize::try_from(left.min(RECV_BUF_LEN as u64)).unwrap_or(RECV_BUF_LEN)
    }

    /// Adds `n` body bytes; resets for the next packet when the body is done.
    fn advance(&mut self, n: usize) {
        self.bodycounter += n as u64;
        if self.bodycounter == u64::from(self.size()) {
            self.headercounter = 0;
        }
    }

    /// Does what one call of C `packet_recv_something` does.
    ///
    /// `recv` receives up to `buf.len()` bytes (C `recv(fd, buf, len, 0)`);
    /// each call asks for exactly the length C asks for. Diagnostics that C
    /// prints to stderr are appended to `diag`, in order, before the result
    /// is acted on.
    pub fn poll(
        &mut self,
        recv: &mut dyn FnMut(&mut [u8]) -> io::Result<usize>,
        diag: &mut Vec<u8>,
    ) -> Recv<'_> {
        if self.headercounter < HEADER_LEN {
            match receive(recv, &mut self.header[self.headercounter..]) {
                Got::Bytes(n) => self.headercounter += n,
                Got::Again => return Recv::NoData,
                Got::Closed => return Recv::ClosedSocket,
            }
            // Don't wait around for the header to fill in.
            if self.headercounter < HEADER_LEN {
                return Recv::NoData;
            }
            self.bodycounter = 0;
        }

        if le_u32(&self.header, 0) != MAGIC {
            diag.extend_from_slice(b"Bad packet header magic\n");
            return Recv::BadData;
        }
        if le_u32(&self.header, 4) != VERSION {
            diag.extend_from_slice(b"Unsupported packet version\n");
            return Recv::BadData;
        }

        match le_u32(&self.header, 12) {
            kind @ (TYPE_DATA | TYPE_ERR) => {
                let len = self.bulk_len();
                let n = match receive(recv, &mut self.buf[..len]) {
                    Got::Bytes(n) => n,
                    Got::Again => return Recv::NoData,
                    Got::Closed => return Recv::ClosedSocket,
                };
                self.advance(n);
                if kind == TYPE_DATA {
                    Recv::Stdout(&self.buf[..n])
                } else {
                    Recv::Stderr(&self.buf[..n])
                }
            }
            TYPE_OOB => self.poll_oob(recv, diag),
            _ => {
                diag.extend_from_slice(b"unknown packet, discarding data\n");
                self.discard(recv)
            }
        }
    }

    /// Receives and drops up to [`Self::bulk_len`] bytes.
    fn discard(&mut self, recv: &mut dyn FnMut(&mut [u8]) -> io::Result<usize>) -> Recv<'_> {
        let len = self.bulk_len();
        match receive(recv, &mut self.buf[..len]) {
            Got::Bytes(n) => {
                self.advance(n);
                Recv::NoData
            }
            Got::Again => Recv::NoData,
            Got::Closed => Recv::ClosedSocket,
        }
    }

    fn poll_oob(
        &mut self,
        recv: &mut dyn FnMut(&mut [u8]) -> io::Result<usize>,
        diag: &mut Vec<u8>,
    ) -> Recv<'_> {
        let size = self.size();
        let bc = self.bodycounter;
        // C: `int readsize = MIN(h.size - bodycounter, sizeof(m) - bodycounter);`
        // computed in `size_t` (wrapping) and truncated to `int`.
        let left = u64::from(size).wrapping_sub(bc);
        let room = (OOB_LEN as u64).wrapping_sub(bc);
        let readsize = left.min(room) as u32 as i32;

        if readsize <= 0 {
            // Stray data after the OOB message (or an empty OOB packet).
            diag.extend_from_slice(
                format!(
                    "oversized oob message. sizeof(m) = {OOB_LEN}, h.size = {}, current pos = {}\n",
                    as_i32(size),
                    bc as i64
                )
                .as_bytes(),
            );
            return self.discard(recv);
        }

        let readsize = readsize.unsigned_abs() as usize;
        let got = match usize::try_from(bc) {
            Ok(at) if at < OOB_LEN => receive(recv, &mut self.msg[at..at + readsize]),
            // `bodycounter` is past the message: this happens when the stray
            // data of an oversized OOB packet arrives in parts. C then writes
            // past the static `m` (undefined behavior); here the bytes are
            // received into the scratch buffer and dropped.
            _ => {
                let len = readsize.min(RECV_BUF_LEN);
                receive(recv, &mut self.buf[..len])
            }
        };
        let n = match got {
            Got::Bytes(n) => n,
            Got::Again => return Recv::NoData,
            Got::Closed => return Recv::ClosedSocket,
        };
        self.advance(n);
        if self.bodycounter != OOB_LEN as u64 {
            return Recv::NoData;
        }
        let message = le_u32(&self.msg, 0);
        let arg = le_u32(&self.msg, 4);
        match message {
            OOB_EOF if as_i32(arg) == STDOUT_FILENO => Recv::CloseStdout,
            OOB_EOF => Recv::CloseStderr,
            OOB_RETURN => Recv::Return(as_i32(arg)),
            OOB_SIGNAL => {
                diag.extend_from_slice(
                    format!(
                        "Got a signal {} from the other end. That ain't right!\n",
                        as_i32(arg)
                    )
                    .as_bytes(),
                );
                Recv::NoData
            }
            OOB_RESIZE => {
                diag.extend_from_slice(b"Got a resize from the other end\n");
                Recv::NoData
            }
            other => {
                diag.extend_from_slice(
                    format!("unknown OOB message: {}\n", as_i32(other)).as_bytes(),
                );
                Recv::NoData
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// One scripted receive outcome.
    enum Step {
        /// Bytes available; a receive takes up to the requested length and
        /// leaves the rest for the next receive.
        Data(Vec<u8>),
        Again,
        Intr,
        Error,
    }

    /// Scripted peer. Records the length of every receive request. When the
    /// script is empty, receives return 0 (closed).
    struct Peer {
        steps: VecDeque<Step>,
        asked: Vec<usize>,
    }

    impl Peer {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: steps.into(),
                asked: Vec::new(),
            }
        }

        fn bytes(chunks: &[&[u8]]) -> Self {
            Self::new(chunks.iter().map(|c| Step::Data(c.to_vec())).collect())
        }

        fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.asked.push(buf.len());
            match self.steps.pop_front() {
                None => Ok(0),
                Some(Step::Again) => Err(io::ErrorKind::WouldBlock.into()),
                Some(Step::Intr) => Err(io::ErrorKind::Interrupted.into()),
                Some(Step::Error) => Err(io::ErrorKind::ConnectionReset.into()),
                Some(Step::Data(mut d)) => {
                    let n = d.len().min(buf.len());
                    buf[..n].copy_from_slice(&d[..n]);
                    if n < d.len() {
                        self.steps.push_front(Step::Data(d.split_off(n)));
                    }
                    Ok(n)
                }
            }
        }
    }

    /// Owned copy of a [`Recv`].
    #[derive(Debug, PartialEq, Eq)]
    enum R {
        NoData,
        Out(Vec<u8>),
        Err(Vec<u8>),
        CloseOut,
        CloseErr,
        Ret(i32),
        Bad,
        Closed,
    }

    fn own(r: Recv<'_>) -> R {
        match r {
            Recv::NoData => R::NoData,
            Recv::Stdout(d) => R::Out(d.to_vec()),
            Recv::Stderr(d) => R::Err(d.to_vec()),
            Recv::CloseStdout => R::CloseOut,
            Recv::CloseStderr => R::CloseErr,
            Recv::Return(rc) => R::Ret(rc),
            Recv::BadData => R::Bad,
            Recv::ClosedSocket => R::Closed,
        }
    }

    /// Polls `n` times; returns results and all diagnostics.
    fn run(d: &mut Decoder, p: &mut Peer, n: usize) -> (Vec<R>, String) {
        let mut diag = Vec::new();
        let rs = (0..n)
            .map(|_| own(d.poll(&mut |b| p.recv(b), &mut diag)))
            .collect();
        (rs, String::from_utf8(diag).unwrap())
    }

    fn raw_header(magic: u32, version: u32, size: u32, kind: u32) -> Vec<u8> {
        [magic, version, size, kind]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    }

    fn pkt(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut p = raw_header(MAGIC, VERSION, body.len() as u32, kind);
        p.extend_from_slice(body);
        p
    }

    fn oob_body(message: u32, arg: u32) -> Vec<u8> {
        let mut b = vec![0u8; OOB_LEN];
        b[..4].copy_from_slice(&message.to_le_bytes());
        b[4..8].copy_from_slice(&arg.to_le_bytes());
        b
    }

    fn oob_pkt(message: u32, arg: u32) -> Vec<u8> {
        pkt(TYPE_OOB, &oob_body(message, arg))
    }

    #[test]
    fn encoder_layouts() {
        assert_eq!(
            encode_data_header(0x0102_0304).to_vec(),
            [0xad, 0xfb, 0xca, 0xde, 1, 0, 0, 0, 4, 3, 2, 1, 0, 0, 0, 0]
        );
        let eof = encode_eof(0);
        assert_eq!(&eof[..16], &raw_header(MAGIC, 1, 20, 2)[..]);
        assert_eq!(&eof[16..], &[0u8; 20]);
        let eof = encode_eof(-2);
        assert_eq!(&eof[16..24], &[0, 0, 0, 0, 0xfe, 0xff, 0xff, 0xff]);
        assert_eq!(&eof[24..], &[0u8; 12]);
        let sig = encode_signal(15);
        assert_eq!(&sig[..16], &raw_header(MAGIC, 1, 20, 2)[..]);
        assert_eq!(&sig[16..24], &[1, 0, 0, 0, 15, 0, 0, 0]);
        assert_eq!(&sig[24..], &[0u8; 12]);
        let rs = encode_resize(0x18, 0x0150);
        assert_eq!(&rs[16..28], &[3, 0, 0, 0, 0x18, 0, 0, 0, 0x50, 0x01, 0, 0]);
        assert_eq!(&rs[28..], &[0u8; 8]);
    }

    #[test]
    fn data_and_err_whole() {
        let mut stream = pkt(TYPE_DATA, b"hello");
        stream.extend(pkt(TYPE_ERR, b"oops"));
        stream.extend(oob_pkt(OOB_RETURN, 7));
        let mut p = Peer::bytes(&[&stream]);
        let mut d = Decoder::new();
        let (rs, diag) = run(&mut d, &mut p, 3);
        assert_eq!(
            rs,
            [
                R::Out(b"hello".to_vec()),
                R::Err(b"oops".to_vec()),
                R::Ret(7)
            ]
        );
        assert_eq!(diag, "");
        assert_eq!(p.asked, [16, 5, 16, 4, 16, 20]);
    }

    #[test]
    fn negative_and_extreme_return_codes() {
        for rc in [-1i32, 0, 1, 255, 256, i32::MIN, i32::MAX] {
            let mut p = Peer::bytes(&[&oob_pkt(OOB_RETURN, rc as u32)]);
            let (rs, _) = run(&mut Decoder::new(), &mut p, 1);
            assert_eq!(rs, [R::Ret(rc)]);
        }
    }

    /// Every split of a DATA packet followed by RETURN gives the same bytes
    /// and the same final result.
    #[test]
    fn fragmented_at_every_split_point() {
        let mut stream = pkt(TYPE_DATA, b"0123456789");
        stream.extend(oob_pkt(OOB_RETURN, 3));
        for a in 0..stream.len() {
            for b in a..stream.len() {
                let mut steps = Vec::new();
                for part in [&stream[..a], &stream[a..b], &stream[b..]] {
                    if !part.is_empty() {
                        steps.push(Step::Data(part.to_vec()));
                        steps.push(Step::Again);
                    }
                }
                let mut p = Peer::new(steps);
                let mut d = Decoder::new();
                let mut out = Vec::new();
                let mut diag = Vec::new();
                let mut done = None;
                for _ in 0..40 {
                    match own(d.poll(&mut |x| p.recv(x), &mut diag)) {
                        R::Out(o) => out.extend(o),
                        R::NoData => {}
                        r => {
                            done = Some(r);
                            break;
                        }
                    }
                }
                assert_eq!(done, Some(R::Ret(3)), "split {a} {b}");
                assert_eq!(out, b"0123456789", "split {a} {b}");
                assert!(diag.is_empty());
            }
        }
    }

    #[test]
    fn partial_header_is_no_data() {
        let h = pkt(TYPE_DATA, b"xy");
        let mut p = Peer::bytes(&[&h[..5], &h[5..15], &h[15..]]);
        let mut d = Decoder::new();
        let (rs, _) = run(&mut d, &mut p, 3);
        assert_eq!(rs, [R::NoData, R::NoData, R::Out(b"xy".to_vec())]);
        assert_eq!(p.asked, [16, 11, 1, 2]);
    }

    #[test]
    fn eagain_and_eintr_at_each_stage_keep_state() {
        let body = b"abcdef";
        let full = pkt(TYPE_DATA, body);
        let makers: [fn() -> Step; 2] = [|| Step::Again, || Step::Intr];
        for again in makers {
            // Header receive.
            let mut p = Peer::new(vec![again(), Step::Data(full.clone())]);
            let (rs, _) = run(&mut Decoder::new(), &mut p, 2);
            assert_eq!(rs, [R::NoData, R::Out(body.to_vec())]);
            // Body receive right after the header completes (B2: C reports
            // a closed socket here).
            let mut p = Peer::new(vec![
                Step::Data(full[..16].to_vec()),
                again(),
                Step::Data(full[16..].to_vec()),
            ]);
            let (rs, _) = run(&mut Decoder::new(), &mut p, 2);
            assert_eq!(rs, [R::NoData, R::Out(body.to_vec())]);
            assert_eq!(p.asked, [16, 6, 6]);
            // Mid-body.
            let mut p = Peer::new(vec![
                Step::Data(full[..18].to_vec()),
                again(),
                Step::Data(full[18..].to_vec()),
            ]);
            let (rs, _) = run(&mut Decoder::new(), &mut p, 3);
            assert_eq!(
                rs,
                [R::Out(b"ab".to_vec()), R::NoData, R::Out(b"cdef".to_vec())]
            );
            // OOB message.
            let o = oob_pkt(OOB_RETURN, 9);
            let mut p = Peer::new(vec![
                Step::Data(o[..16].to_vec()),
                again(),
                Step::Data(o[16..25].to_vec()),
                again(),
                Step::Data(o[25..].to_vec()),
            ]);
            let (rs, _) = run(&mut Decoder::new(), &mut p, 4);
            assert_eq!(rs, [R::NoData, R::NoData, R::NoData, R::Ret(9)]);
            assert_eq!(p.asked, [16, 20, 20, 11, 11]);
        }
    }

    #[test]
    fn closed_and_errors() {
        // EOF before anything.
        let (rs, _) = run(&mut Decoder::new(), &mut Peer::new(vec![]), 1);
        assert_eq!(rs, [R::Closed]);
        // Hard error on header and on body.
        let (rs, _) = run(&mut Decoder::new(), &mut Peer::new(vec![Step::Error]), 1);
        assert_eq!(rs, [R::Closed]);
        let h = pkt(TYPE_DATA, b"zz");
        let mut p = Peer::new(vec![Step::Data(h[..16].to_vec()), Step::Error]);
        let (rs, _) = run(&mut Decoder::new(), &mut p, 1);
        assert_eq!(rs, [R::Closed]);
        // Close mid-header and mid-body.
        let mut p = Peer::bytes(&[&h[..9]]);
        let (rs, _) = run(&mut Decoder::new(), &mut p, 2);
        assert_eq!(rs, [R::NoData, R::Closed]);
        let mut p = Peer::bytes(&[&h[..17]]);
        let (rs, _) = run(&mut Decoder::new(), &mut p, 2);
        assert_eq!(rs, [R::Out(b"z".to_vec()), R::Closed]);
        // Close mid-OOB.
        let o = oob_pkt(OOB_RETURN, 0);
        let mut p = Peer::bytes(&[&o[..30]]);
        let (rs, _) = run(&mut Decoder::new(), &mut p, 2);
        assert_eq!(rs, [R::NoData, R::Closed]);
    }

    #[test]
    fn zero_size_data_and_err_are_closed_socket_b3() {
        for kind in [TYPE_DATA, TYPE_ERR] {
            let mut p = Peer::bytes(&[&pkt(kind, b"")]);
            let (rs, diag) = run(&mut Decoder::new(), &mut p, 1);
            assert_eq!(rs, [R::Closed]);
            assert_eq!(diag, "");
            // No receive of length 0 reaches the socket.
            assert_eq!(p.asked, [16]);
        }
    }

    #[test]
    fn bad_magic_and_version() {
        let mut p = Peer::bytes(&[&raw_header(0xDECA_FBAE, 1, 0, 0)]);
        let mut d = Decoder::new();
        let (rs, diag) = run(&mut d, &mut p, 2);
        assert_eq!(rs, [R::Bad, R::Bad]);
        assert_eq!(diag, "Bad packet header magic\nBad packet header magic\n");
        assert_eq!(p.asked, [16]);
        // Magic is checked before version.
        let mut p = Peer::bytes(&[&raw_header(1, 2, 0, 0)]);
        let (_, diag) = run(&mut Decoder::new(), &mut p, 1);
        assert_eq!(diag, "Bad packet header magic\n");
        let mut p = Peer::bytes(&[&raw_header(MAGIC, 0, 0, 0)]);
        let (rs, diag) = run(&mut Decoder::new(), &mut p, 1);
        assert_eq!(
            (rs, diag.as_str()),
            (vec![R::Bad], "Unsupported packet version\n")
        );
    }

    #[test]
    fn large_data_is_read_in_64k_chunks() {
        let body = vec![7u8; RECV_BUF_LEN * 2 + 10];
        let mut p = Peer::bytes(&[&pkt(TYPE_DATA, &body)]);
        let (rs, _) = run(&mut Decoder::new(), &mut p, 3);
        let lens: Vec<usize> = rs
            .iter()
            .map(|r| match r {
                R::Out(o) => o.len(),
                _ => usize::MAX,
            })
            .collect();
        assert_eq!(lens, [RECV_BUF_LEN, RECV_BUF_LEN, 10]);
        assert_eq!(p.asked, [16, RECV_BUF_LEN, RECV_BUF_LEN, 10]);
    }

    #[test]
    fn unknown_type_is_discarded_per_call() {
        let mut s = pkt(7, b"abcdef");
        s.extend(oob_pkt(OOB_RETURN, 1));
        let mut p = Peer::bytes(&[&s[..19], &s[19..]]);
        let (rs, diag) = run(&mut Decoder::new(), &mut p, 3);
        assert_eq!(rs, [R::NoData, R::NoData, R::Ret(1)]);
        assert_eq!(
            diag,
            "unknown packet, discarding data\nunknown packet, discarding data\n"
        );
        assert_eq!(p.asked, [16, 6, 3, 16, 20]);
        // Zero-size unknown packet: message, then closed socket.
        let mut p = Peer::bytes(&[&pkt(9, b"")]);
        let (rs, diag) = run(&mut Decoder::new(), &mut p, 1);
        assert_eq!(
            (rs, diag.as_str()),
            (vec![R::Closed], "unknown packet, discarding data\n")
        );
    }

    #[test]
    fn oob_eof_and_messages() {
        let mut s = Vec::new();
        for (m, a) in [
            (OOB_EOF, 1),
            (OOB_EOF, 0),
            (OOB_EOF, 2),
            (OOB_EOF, u32::MAX),
            (OOB_SIGNAL, 2),
            (OOB_SIGNAL, 0x8000_0000),
            (OOB_RESIZE, 0),
            (4, 0),
            (u32::MAX, 0),
        ] {
            s.extend(oob_pkt(m, a));
        }
        let mut p = Peer::bytes(&[&s]);
        let (rs, diag) = run(&mut Decoder::new(), &mut p, 9);
        assert_eq!(
            rs,
            [
                R::CloseOut,
                R::CloseErr,
                R::CloseErr,
                R::CloseErr,
                R::NoData,
                R::NoData,
                R::NoData,
                R::NoData,
                R::NoData
            ]
        );
        assert_eq!(
            diag,
            "Got a signal 2 from the other end. That ain't right!\n\
             Got a signal -2147483648 from the other end. That ain't right!\n\
             Got a resize from the other end\n\
             unknown OOB message: 4\n\
             unknown OOB message: -1\n"
        );
    }

    #[test]
    fn oob_size_zero_b3() {
        let mut p = Peer::bytes(&[&pkt(TYPE_OOB, b"")]);
        let (rs, diag) = run(&mut Decoder::new(), &mut p, 1);
        assert_eq!(rs, [R::Closed]);
        assert_eq!(
            diag,
            "oversized oob message. sizeof(m) = 20, h.size = 0, current pos = 0\n"
        );
        assert_eq!(p.asked, [16]);
    }

    #[test]
    fn short_oob_is_silently_consumed() {
        let mut s = pkt(TYPE_OOB, &oob_body(OOB_RETURN, 5)[..8]);
        s.extend(oob_pkt(OOB_RETURN, 6));
        let mut p = Peer::bytes(&[&s[..20], &s[20..]]);
        let (rs, diag) = run(&mut Decoder::new(), &mut p, 3);
        assert_eq!(rs, [R::NoData, R::NoData, R::Ret(6)]);
        assert_eq!(diag, "");
        assert_eq!(p.asked, [16, 8, 4, 16, 20]);
    }

    #[test]
    fn oversized_oob_dispatches_then_discards() {
        let mut body = oob_body(OOB_RETURN, 4);
        body.extend_from_slice(b"WXYZ");
        let mut s = pkt(TYPE_OOB, &body);
        s.extend(pkt(TYPE_DATA, b"k"));
        let mut p = Peer::bytes(&[&s]);
        let mut d = Decoder::new();
        let (rs, diag) = run(&mut d, &mut p, 1);
        assert_eq!((rs, diag.as_str()), (vec![R::Ret(4)], ""));
        let (rs, diag) = run(&mut d, &mut p, 2);
        assert_eq!(rs, [R::NoData, R::Out(b"k".to_vec())]);
        assert_eq!(
            diag,
            "oversized oob message. sizeof(m) = 20, h.size = 24, current pos = 20\n"
        );
        assert_eq!(p.asked, [16, 20, 4, 16, 1]);
    }

    /// Stray OOB data that arrives in parts: after the first part,
    /// `sizeof(m) - bodycounter` wraps and C takes the "message" branch
    /// without a diagnostic.
    #[test]
    fn oversized_oob_stray_data_in_parts() {
        let mut body = oob_body(OOB_RESIZE, 0);
        body.extend_from_slice(&[9u8; 30]);
        let s = pkt(TYPE_OOB, &body);
        let mut p = Peer::bytes(&[&s[..36], &s[36..46], &s[46..]]);
        let (rs, diag) = run(&mut Decoder::new(), &mut p, 3);
        assert_eq!(rs, [R::NoData, R::NoData, R::NoData]);
        assert_eq!(
            diag,
            "Got a resize from the other end\n\
             oversized oob message. sizeof(m) = 20, h.size = 50, current pos = 20\n"
        );
        assert_eq!(p.asked, [16, 20, 30, 20]);
        // The next packet decodes normally.
        let mut p2 = Peer::bytes(&[&oob_pkt(OOB_RETURN, 2)]);
        let mut d = Decoder::new();
        let mut q = Peer::bytes(&[&s[..36], &s[36..46], &s[46..]]);
        run(&mut d, &mut q, 3);
        assert_eq!(run(&mut d, &mut p2, 1).0, [R::Ret(2)]);
    }

    #[test]
    fn oversized_oob_negative_int_readsize() {
        // h.size - bodycounter >= 2^31 truncates to a negative int: C takes
        // the "oversized" branch again on every call.
        let size = u32::MAX;
        let mut s = raw_header(MAGIC, VERSION, size, TYPE_OOB);
        s.extend(oob_body(OOB_RESIZE, 0));
        s.extend_from_slice(&[1u8; 5]);
        let mut p = Peer::bytes(&[&s[..36], &s[36..38], &s[38..]]);
        let (rs, diag) = run(&mut Decoder::new(), &mut p, 3);
        assert_eq!(rs, [R::NoData, R::NoData, R::NoData]);
        assert_eq!(
            diag,
            "Got a resize from the other end\n\
             oversized oob message. sizeof(m) = 20, h.size = -1, current pos = 20\n\
             oversized oob message. sizeof(m) = 20, h.size = -1, current pos = 22\n"
        );
        assert_eq!(p.asked, [16, 20, RECV_BUF_LEN, RECV_BUF_LEN]);
    }
}
