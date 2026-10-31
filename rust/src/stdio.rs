//! C stdio `stdout` buffering, for the output C writes with `printf`,
//! `puts` and `fprintf(stdout, ...)`: usage, version, `-l` lines and the
//! password prompt.
//!
//! C `stdout` is line-buffered when fd 1 is a terminal and fully buffered
//! otherwise, while `stderr` is unbuffered. Emulating this keeps the byte
//! order of stdout and stderr when both go to one pipe. The remote-command
//! data stream does not use this type: C writes it with raw `write(1)`.

use std::io::{self, Write};

/// Buffer size at which a fully buffered stream is written out, so memory
/// stays bounded. C flushes when its buffer (`st_blksize` bytes) fills; the
/// exact chunk boundaries are not emulated.
const FULL_BUFFER_LIMIT: usize = 4096;

/// C `stdout` over `inner`. Bytes are held until a newline (line-buffered)
/// or until [`Write::flush`] (fully buffered). Write errors are ignored, as
/// C ignores `printf` errors.
pub struct StdioOut<W: Write> {
    inner: W,
    buf: Vec<u8>,
    line_buffered: bool,
}

impl<W: Write> StdioOut<W> {
    pub fn new(inner: W, line_buffered: bool) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            line_buffered,
        }
    }

    /// Writes out the first `n` buffered bytes; the bytes are dropped even
    /// if the write fails.
    fn write_out(&mut self, n: usize) {
        let _ = self.inner.write_all(&self.buf[..n]);
        self.buf.drain(..n);
    }
}

impl<W: Write> Write for StdioOut<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.line_buffered {
            if let Some(last_nl) = self.buf.iter().rposition(|&b| b == b'\n') {
                self.write_out(last_nl + 1);
            }
        } else if self.buf.len() >= FULL_BUFFER_LIMIT {
            self.write_out(self.buf.len());
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.write_out(self.buf.len());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_buffered_writes_through_last_newline() {
        let mut s = StdioOut::new(Vec::new(), true);
        s.write_all(b"Please enter password: ").unwrap();
        assert!(s.inner.is_empty());
        s.write_all(b"a\nb\nc").unwrap();
        assert_eq!(s.inner, b"Please enter password: a\nb\n");
        s.flush().unwrap();
        assert_eq!(s.inner, b"Please enter password: a\nb\nc");
    }

    #[test]
    fn fully_buffered_waits_for_flush_or_limit() {
        let mut s = StdioOut::new(Vec::new(), false);
        s.write_all(b"line\n").unwrap();
        assert!(s.inner.is_empty());
        s.flush().unwrap();
        assert_eq!(s.inner, b"line\n");
        let big = vec![b'x'; FULL_BUFFER_LIMIT];
        s.write_all(&big).unwrap();
        assert_eq!(s.inner.len(), 5 + FULL_BUFFER_LIMIT);
        assert!(s.buf.is_empty());
    }

    struct Failing;
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("fail"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn errors_are_ignored_and_bytes_dropped() {
        let mut s = StdioOut::new(Failing, true);
        assert_eq!(s.write(b"x\n").unwrap(), 2);
        assert!(s.buf.is_empty());
        s.write_all(b"y").unwrap();
        s.flush().unwrap();
        assert!(s.buf.is_empty());
    }
}
