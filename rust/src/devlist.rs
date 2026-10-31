//! Device list protocol (novacomd device-list port): C `read_line`,
//! the `sscanf("%d %40s %20s %30s %10s")` line parser, `list_devices`
//! and `find_device`.

use std::io::{Read, Write};

use crate::cli::c_isspace;

/// Size of the C `buf[256]` used for device-list lines.
pub const DEVLIST_BUFLEN: usize = 256;

/// Widths of the `%40s %20s %30s %10s` conversions.
const NDUID_WIDTH: usize = 40;
const CONNTYPE_WIDTH: usize = 20;
const DEVTYPE_WIDTH: usize = 30;
const SESSION_WIDTH: usize = 10;

/// Fewer assignments than this means a bad line (C `rc < 4`).
const MIN_FIELDS: usize = 4;

/// Emulates C `read_line`: reads one byte at a time, at most `buflen - 1`
/// bytes, and stops at `\n` (not stored), EOF or a read error.
pub fn read_line<R: Read>(r: &mut R, buflen: usize) -> Vec<u8> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < buflen.saturating_sub(1) {
        match r.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
        }
    }
    line
}

/// The bytes a C string sees: everything before the first NUL.
pub fn c_str(bytes: &[u8]) -> &[u8] {
    bytes
        .iter()
        .position(|&b| b == 0)
        .map_or(bytes, |n| &bytes[..n])
}

/// Result of [`scan_device_line`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceLine {
    /// Number of successful assignments (C `sscanf` return value; the EOF
    /// return of -1 is reported as 0, which callers treat the same).
    pub assigned: usize,
    pub port: i32,
    pub nduid: Vec<u8>,
    pub conntype: Vec<u8>,
    pub devtype: Vec<u8>,
    pub session: Vec<u8>,
}

/// Input cursor with C `scanf` conversion semantics.
struct Scanner<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Scanner<'_> {
    fn skip_space(&mut self) {
        while self.s.get(self.pos).is_some_and(|&b| c_isspace(b)) {
            self.pos += 1;
        }
    }

    /// `%d`: whitespace, optional sign, one or more digits. The value is
    /// read as a saturating `long` and truncated to `int`, as glibc and
    /// macOS libc both do.
    fn int(&mut self) -> Option<i32> {
        self.skip_space();
        let start = self.pos;
        if matches!(self.s.get(self.pos), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let digits = self.s[self.pos..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        self.pos += digits;
        Some(crate::cli::c_atoi(&self.s[start..self.pos]))
    }

    /// `%<width>s`: whitespace, then 1 to `width` non-whitespace bytes.
    fn word(&mut self, width: usize) -> Option<Vec<u8>> {
        self.skip_space();
        let len = self.s[self.pos..]
            .iter()
            .take(width)
            .take_while(|&&b| !c_isspace(b))
            .count();
        if len == 0 {
            return None;
        }
        let w = self.s[self.pos..self.pos + len].to_vec();
        self.pos += len;
        Some(w)
    }
}

/// Emulates `sscanf(line, "%d %40s %20s %30s %10s", ...)` on a C string
/// (the input is cut at its first NUL).
pub fn scan_device_line(line: &[u8]) -> DeviceLine {
    let mut sc = Scanner {
        s: c_str(line),
        pos: 0,
    };
    let mut out = DeviceLine::default();
    let Some(port) = sc.int() else { return out };
    out.port = port;
    out.assigned = 1;
    let fields: [(&mut Vec<u8>, usize); 4] = [
        (&mut out.nduid, NDUID_WIDTH),
        (&mut out.conntype, CONNTYPE_WIDTH),
        (&mut out.devtype, DEVTYPE_WIDTH),
        (&mut out.session, SESSION_WIDTH),
    ];
    let mut assigned = 1;
    for (field, width) in fields {
        // The ' ' directive before each %s skips whitespace; %s does too.
        let Some(w) = sc.word(width) else { break };
        *field = w;
        assigned += 1;
    }
    out.assigned = assigned;
    out
}

/// `-d` filter match: empty filter, or ASCII case-insensitive equality
/// (`strcasecmp` in the C locale) with nduid, connection type or device type.
fn matches_filter(filter: &[u8], d: &DeviceLine) -> bool {
    filter.is_empty()
        || filter.eq_ignore_ascii_case(&d.nduid)
        || filter.eq_ignore_ascii_case(&d.conntype)
        || filter.eq_ignore_ascii_case(&d.devtype)
}

/// Reads lines until an empty line, EOF or error; yields each valid
/// (>= 4 fields) line as its C string plus the parse result.
fn valid_lines<R: Read>(r: &mut R) -> impl Iterator<Item = (Vec<u8>, DeviceLine)> + '_ {
    std::iter::from_fn(move || {
        let line = read_line(r, DEVLIST_BUFLEN);
        (!line.is_empty()).then_some(line)
    })
    .filter_map(|line| {
        let line = c_str(&line).to_vec();
        let d = scan_device_line(&line);
        (d.assigned >= MIN_FIELDS).then_some((line, d))
    })
}

/// Emulates C `list_devices`: prints each valid line that matches the
/// filter, followed by `\n` (C `puts`).
pub fn list_devices<R: Read, W: Write + ?Sized>(
    r: &mut R,
    filter: &[u8],
    out: &mut W,
) -> std::io::Result<()> {
    for (line, d) in valid_lines(r) {
        if matches_filter(filter, &d) {
            out.write_all(&line)?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Device selected by [`find_device`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundDevice {
    /// Device port; may be negative (the caller then treats it as not found).
    pub port: i32,
    pub nduid: Vec<u8>,
    /// Session id; empty if the line had none.
    pub session: Vec<u8>,
}

/// Emulates C `find_device`: the first valid line matching the filter.
pub fn find_device<R: Read>(r: &mut R, filter: &[u8]) -> Option<FoundDevice> {
    valid_lines(r)
        .find(|(_, d)| matches_filter(filter, d))
        .map(|(_, d)| FoundDevice {
            port: d.port,
            nduid: d.nduid,
            session: d.session,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &[u8]) -> (usize, i32, Vec<Vec<u8>>) {
        let d = scan_device_line(s);
        let mut f = vec![d.nduid, d.conntype, d.devtype, d.session];
        f.truncate(d.assigned.saturating_sub(1));
        (d.assigned, d.port, f)
    }

    fn v(fields: &[&[u8]]) -> Vec<Vec<u8>> {
        fields.iter().map(|f| f.to_vec()).collect()
    }

    #[test]
    fn scan_full_line() {
        assert_eq!(
            scan(b"6969 abcdef usb castle-linux sess1"),
            (5, 6969, v(&[b"abcdef", b"usb", b"castle-linux", b"sess1"]))
        );
        assert_eq!(
            scan(b"  \t-1\x0bn\x0cc\rd\n"),
            (4, -1, v(&[b"n", b"c", b"d"]))
        );
        assert_eq!(scan(b"+7 n c d"), (4, 7, v(&[b"n", b"c", b"d"])));
    }

    #[test]
    fn scan_partial_and_bad() {
        assert_eq!(scan(b"").0, 0);
        assert_eq!(scan(b"   ").0, 0);
        assert_eq!(scan(b"x 1 2 3").0, 0);
        assert_eq!(scan(b"- 1 2 3").0, 0);
        assert_eq!(scan(b"+").0, 0);
        assert_eq!(scan(b"5").0, 1);
        assert_eq!(scan(b"5 a b").0, 3);
        assert_eq!(scan(b"5 a b c").0, 4);
    }

    #[test]
    fn scan_digits_run_into_string() {
        assert_eq!(scan(b"123abc u t"), (4, 123, v(&[b"abc", b"u", b"t"])));
    }

    #[test]
    fn scan_widths_split_long_tokens() {
        let nduid = [b'a'; 45];
        let mut line = b"1 ".to_vec();
        line.extend_from_slice(&nduid);
        line.extend_from_slice(b" usb dev");
        let (n, _, f) = scan(&line);
        assert_eq!(n, 5);
        assert_eq!(f[0], &[b'a'; 40][..]);
        assert_eq!(f[1], b"aaaaa");
        assert_eq!(f[2], b"usb");
        assert_eq!(f[3], b"dev");

        let (n, _, f) = scan(b"1 n 0123456789012345678901234 t 01234567890123");
        assert_eq!(n, 5);
        assert_eq!(f[1], b"01234567890123456789");
        assert_eq!(f[2], b"01234");
        assert_eq!(f[3], b"t");
        let (_, _, f) = scan(b"1 n c 0123456789012345678901234567890123 s");
        assert_eq!(f[2], b"012345678901234567890123456789");
        assert_eq!(f[3], b"0123");
    }

    #[test]
    fn scan_int_overflow_truncates() {
        assert_eq!(scan(b"4294967297 n c d").1, 1);
        assert_eq!(scan(b"99999999999999999999 n c d").1, -1);
    }

    #[test]
    fn scan_stops_at_nul_and_keeps_high_bytes() {
        assert_eq!(scan(b"1 n c\0d e").0, 3);
        assert_eq!(
            scan(b"1 \xa0 \x85 \xff"),
            (4, 1, v(&[b"\xa0", b"\x85", b"\xff"]))
        );
    }

    #[test]
    fn read_line_splits() {
        let mut data: &[u8] = b"ab\ncd\n\nlast";
        assert_eq!(read_line(&mut data, 256), b"ab");
        assert_eq!(read_line(&mut data, 256), b"cd");
        assert_eq!(read_line(&mut data, 256), b"");
        assert_eq!(read_line(&mut data, 256), b"last");
        assert_eq!(read_line(&mut data, 256), b"");
    }

    #[test]
    fn read_line_limits_length() {
        let mut long = vec![b'x'; 300];
        long.push(b'\n');
        let mut r = long.as_slice();
        assert_eq!(read_line(&mut r, 256).len(), 255);
        assert_eq!(read_line(&mut r, 256), vec![b'x'; 45]);
        assert_eq!(read_line(&mut r, 256), b"");

        let mut exact = vec![b'y'; 255];
        exact.extend_from_slice(b"\nnext\n");
        let mut r = exact.as_slice();
        assert_eq!(read_line(&mut r, 256).len(), 255);
        assert_eq!(read_line(&mut r, 256), b""); // the '\n' alone ends the list
        assert_eq!(read_line(&mut r, 256), b"next");
    }

    #[test]
    fn read_line_keeps_nul_and_cr() {
        let mut r: &[u8] = b"\0a\r\nb";
        let l = read_line(&mut r, 256);
        assert_eq!(l, b"\0a\r");
        assert_eq!(c_str(&l), b"");
    }

    #[test]
    fn read_line_stops_on_error() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("boom"))
            }
        }
        assert_eq!(read_line(&mut Failing, 256), b"");
    }

    const LIST: &[u8] = b"bad line\n\
        6970 N1 usb castle-linux\n\
        6971 n2 TCP Topaz-Linux s1\n\
        6972 n3\0 usb x y\n\
        \0junk\n\
        6973 n4 wifi pre3\r\n\
        \n\
        6974 n5 usb hidden\n";

    fn list(filter: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        list_devices(&mut &LIST[..], filter, &mut out).unwrap();
        out
    }

    #[test]
    fn list_all_and_filtered() {
        assert_eq!(
            list(b""),
            b"6970 N1 usb castle-linux\n6971 n2 TCP Topaz-Linux s1\n6973 n4 wifi pre3\r\n".to_vec()
        );
        assert_eq!(list(b"n1"), b"6970 N1 usb castle-linux\n".to_vec());
        assert_eq!(list(b"tcp"), b"6971 n2 TCP Topaz-Linux s1\n".to_vec());
        assert_eq!(
            list(b"TOPAZ-linux"),
            b"6971 n2 TCP Topaz-Linux s1\n".to_vec()
        );
        assert_eq!(list(b"USB"), b"6970 N1 usb castle-linux\n".to_vec());
        assert_eq!(list(b"hidden"), b"".to_vec());
        assert_eq!(list(b"nomatch"), b"".to_vec());
    }

    #[test]
    fn find_first_and_filtered() {
        let f = |filter: &[u8]| find_device(&mut &LIST[..], filter);
        assert_eq!(
            f(b""),
            Some(FoundDevice {
                port: 6970,
                nduid: b"N1".to_vec(),
                session: Vec::new()
            })
        );
        assert_eq!(
            f(b"topaz-LINUX"),
            Some(FoundDevice {
                port: 6971,
                nduid: b"n2".to_vec(),
                session: b"s1".to_vec()
            })
        );
        assert_eq!(f(b"WIFI").map(|d| d.port), Some(6973));
        assert_eq!(f(b"hidden"), None);
        let mut neg: &[u8] = b"-1 a b c\n6970 a b c\n";
        assert_eq!(find_device(&mut neg, b"").map(|d| d.port), Some(-1));
    }

    #[test]
    fn ascii_only_case_fold() {
        let mut r: &[u8] = b"1 \xc9 b c\n";
        assert_eq!(find_device(&mut r, b"\xe9"), None);
    }
}
