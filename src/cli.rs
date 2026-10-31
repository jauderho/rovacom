//! Command-line parsing: a byte-exact emulation of the C client's
//! `parse_opt` (`getopt_long` with optstring `"a:d:f:lPp:tswVr:c:"`),
//! `usage`, `atoi` and `parsePortsToBeForwarded`.
//!
//! The only OS difference is the libc `getopt_long` implementation:
//! glibc ([`Flavor::Gnu`]) on Linux, FreeBSD-derived ([`Flavor::Bsd`]) on macOS.

/// Short option string passed to `getopt_long` by the C client.
const OPTSTRING: &[u8] = b"a:d:f:lPp:tswVr:c:";

/// One entry of the C `long_options` table.
struct LongOpt {
    name: &'static [u8],
    has_arg: bool,
    val: u8,
}

/// The C `long_options` table, in table order (order matters for
/// abbreviation matching and the glibc ambiguity list).
const LONG_OPTIONS: [LongOpt; 9] = [
    LongOpt {
        name: b"address",
        has_arg: true,
        val: b'a',
    },
    LongOpt {
        name: b"device",
        has_arg: true,
        val: b'd',
    },
    LongOpt {
        name: b"list",
        has_arg: false,
        val: b'l',
    },
    LongOpt {
        name: b"port",
        has_arg: true,
        val: b'p',
    },
    LongOpt {
        name: b"terminal",
        has_arg: false,
        val: b't',
    },
    LongOpt {
        name: b"signals",
        has_arg: false,
        val: b's',
    },
    LongOpt {
        name: b"wait",
        has_arg: false,
        val: b'w',
    },
    LongOpt {
        name: b"password",
        has_arg: true,
        val: b'r',
    },
    LongOpt {
        name: b"cmd",
        has_arg: true,
        val: b'c',
    },
];

/// Default device-list server host (C `serverip`).
const DEFAULT_SERVER_HOST: &[u8] = b"localhost";
/// Default device-list server port (C `serverport`).
const DEFAULT_SERVER_PORT: i32 = 6968;

/// Version string: `BUILDVERSION` at compile time, else `rovacom-<pkg version>`.
pub const VERSION: &str = match option_env!("BUILDVERSION") {
    Some(v) => v,
    None => concat!("rovacom-", env!("CARGO_PKG_VERSION")),
};

/// Which libc `getopt_long` implementation to emulate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// glibc (Linux).
    Gnu,
    /// FreeBSD-derived libc with `GNU_COMPATIBLE` (macOS).
    Bsd,
}

/// The flavor of the libc this binary would have been linked against.
#[cfg(target_os = "macos")]
pub const FLAVOR: Flavor = Flavor::Bsd;
/// The flavor of the libc this binary would have been linked against.
#[cfg(not(target_os = "macos"))]
pub const FLAVOR: Flavor = Flavor::Gnu;

/// Program name that libc puts in front of getopt diagnostics.
///
/// macOS `getprogname()` is the basename of the executable path given to
/// `execve` (symlinks are not resolved), not `argv[0]`. `current_exe()` on
/// macOS returns that same path (`_NSGetExecutablePath`).
#[cfg(target_os = "macos")]
pub fn diag_progname(argv0: &[u8]) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.as_bytes().to_vec()))
        .unwrap_or_else(|| basename(argv0).to_vec())
}

/// Program name that libc puts in front of getopt diagnostics.
///
/// glibc prints `argv[0]` verbatim.
#[cfg(not(target_os = "macos"))]
pub fn diag_progname(argv0: &[u8]) -> Vec<u8> {
    argv0.to_vec()
}

/// Bytes after the last `/` (fallback for `getprogname()`).
#[cfg(target_os = "macos")]
fn basename(path: &[u8]) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    std::path::Path::new(std::ffi::OsStr::from_bytes(path))
        .file_name()
        .map_or(path, |n| n.as_bytes())
}

/// Environment of one option-parsing run.
#[derive(Clone, Debug)]
pub struct ParseContext {
    pub flavor: Flavor,
    /// `POSIXLY_CORRECT` is set: stop at the first operand.
    pub posixly_correct: bool,
    /// Prefix of libc diagnostics (see [`diag_progname`]).
    pub diag_progname: Vec<u8>,
}

/// A `-f` local/remote port pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortPair {
    pub local: u32,
    pub remote: u32,
}

/// Parsed options (the C globals set by `parse_opt`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub server_host: Vec<u8>,
    pub server_port: i32,
    pub device: Vec<u8>,
    pub password: Option<Vec<u8>>,
    pub cmd: Option<Vec<u8>>,
    pub list: bool,
    pub port_forward: bool,
    pub term_mode: bool,
    pub signals: bool,
    pub sigwinch: bool,
    pub wait: bool,
    pub forwards: Vec<PortPair>,
    /// Non-option arguments, in order (C `argv[optind..]`).
    pub operands: Vec<Vec<u8>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            server_host: DEFAULT_SERVER_HOST.to_vec(),
            server_port: DEFAULT_SERVER_PORT,
            device: Vec::new(),
            password: None,
            cmd: None,
            list: false,
            port_forward: false,
            term_mode: false,
            signals: false,
            sigwinch: false,
            wait: false,
            forwards: Vec::new(),
            operands: Vec::new(),
        }
    }
}

/// Parsing ended the program: write `stderr`, then `stdout`, exit `code`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: u8,
}

/// `version: <V>\n`.
pub fn version_line() -> Vec<u8> {
    format!("version: {VERSION}\n").into_bytes()
}

/// Option list of C `usage()`, copied verbatim.
const USAGE_OPTIONS: &[u8] = b"options:\n\
\t-a address: ip address of the novacomd server, default is 'localhost'\n\
\t-p port: port of the novacomd server's device list port, default is 6968\n\
\t-t: go into terminal mode, for interactive use\n\
\t-s: pass signals to remote process\n\
\t-l: list devices and then exit\n\
\t-r: device password\n\
\t-c: service command [login, add, remove, logout]\n\
\t\t  login:  opens new session\n\
\t\t  add:    adds device token to host\n\
\t\t  remove: remove device token from host\n\
\t\t  logout: closes active session\n\
\t-d device: connect to specific device instead of first.\n\
\t\t might be <nduid>, <connection type>, <device type>\n\
\t-w: wait for device to show up before running command\n\
\t-V: version information\n\
\t-P: Port Forwarding Enabled\n\
\t-f: ports to forward\n";

/// Full C `usage()` stdout text, including the version line.
pub fn usage_text(argv0: &[u8]) -> Vec<u8> {
    let mut v = version_line();
    v.extend_from_slice(b"usage: ");
    v.extend_from_slice(argv0);
    v.extend_from_slice(
        b" [-a address] [-p port] [-t] [-l] [-d device] [-c cmd] [-r password] [-w] <command>\n",
    );
    v.extend_from_slice(argv0);
    v.extend_from_slice(b" [-V]\n");
    v.extend_from_slice(argv0);
    v.extend_from_slice(b" [-a address] [-p port] -P[ [-f <localport:remoteport,...>] ]\n");
    v.extend_from_slice(USAGE_OPTIONS);
    v
}

/// C `isspace` in the C locale.
pub fn c_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Leading whitespace, sign and decimal magnitude, as `strtol`/`strtoul`
/// read them. Returns `(negative, magnitude)`; `None` magnitude on u64 overflow.
fn c_scan_decimal(s: &[u8]) -> (bool, Option<u64>) {
    let mut i = s.iter().take_while(|&&b| c_isspace(b)).count();
    let mut negative = false;
    if let Some(&sign @ (b'+' | b'-')) = s.get(i) {
        negative = sign == b'-';
        i += 1;
    }
    let mut mag: Option<u64> = Some(0);
    for &b in s[i..].iter().take_while(|b| b.is_ascii_digit()) {
        mag = mag
            .and_then(|m| m.checked_mul(10))
            .and_then(|m| m.checked_add(u64::from(b - b'0')));
    }
    (negative, mag)
}

/// C `strtol(s, NULL, 10)` with 64-bit `long` (saturates on overflow).
pub fn c_strtol(s: &[u8]) -> i64 {
    let (negative, mag) = c_scan_decimal(s);
    let mag = mag.map_or(i128::MAX, i128::from);
    let v = if negative { -mag } else { mag };
    i64::try_from(v).unwrap_or(if negative { i64::MIN } else { i64::MAX })
}

/// C `atoi` (= `(int)strtol(s, NULL, 10)` on glibc and macOS): the `long`
/// result is truncated to 32 bits.
pub fn c_atoi(s: &[u8]) -> i32 {
    c_strtol(s) as i32
}

/// C `strtoul(s, NULL, 10)` with 64-bit `unsigned long`: overflow gives
/// `ULONG_MAX`; a leading `-` negates in unsigned arithmetic.
pub fn c_strtoul(s: &[u8]) -> u64 {
    match c_scan_decimal(s) {
        (_, None) => u64::MAX,
        (true, Some(m)) => m.wrapping_neg(),
        (false, Some(m)) => m,
    }
}

/// Emulates `parsePortsToBeForwarded` (`tcprelay.c`): `strtok` on `,:`
/// (empty tokens collapse), tokens taken as local,remote pairs, each value
/// parsed with `strtoul` and rejected if above `INT_MAX`.
///
/// Returns `None` on error. Intentional change B8: an odd token count or
/// no tokens is an error
/// (C dereferences NULL and crashes).
pub fn parse_ports(s: &[u8]) -> Option<Vec<PortPair>> {
    let mut tokens = s
        .split(|&b| b == b',' || b == b':')
        .filter(|t| !t.is_empty());
    let parse = |t: &[u8]| {
        u32::try_from(c_strtoul(t))
            .ok()
            .filter(|&v| v <= i32::MAX as u32)
    };
    let mut pairs = Vec::new();
    while let Some(local) = tokens.next() {
        let local = parse(local)?;
        let remote = parse(tokens.next()?)?;
        pairs.push(PortPair { local, remote });
    }
    (!pairs.is_empty()).then_some(pairs)
}

/// One result of [`Getopt::next_event`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Option character and its argument.
    Opt(u8, Option<Vec<u8>>),
    /// getopt returned `'?'`; carries the libc diagnostic (may be empty).
    Error(Vec<u8>),
}

/// Emulation of `getopt_long(argc, argv, OPTSTRING, LONG_OPTIONS, ...)`.
///
/// Operands are collected instead of permuting argv; the observable result
/// (option order, option arguments, operand order) is the same.
pub struct Getopt<'a> {
    args: &'a [Vec<u8>],
    ctx: &'a ParseContext,
    /// Next argv element to examine.
    idx: usize,
    /// Position of the next short option inside a cluster: (element, offset).
    cluster: Option<(usize, usize)>,
    operands: Vec<Vec<u8>>,
}

impl<'a> Getopt<'a> {
    /// `args` is the full argv, including `argv[0]`.
    pub fn new(args: &'a [Vec<u8>], ctx: &'a ParseContext) -> Self {
        Self {
            args,
            ctx,
            idx: 1,
            cluster: None,
            operands: Vec::new(),
        }
    }

    /// Operands collected so far; complete once [`Getopt::next_event`] returned `None`.
    pub fn into_operands(self) -> Vec<Vec<u8>> {
        self.operands
    }

    /// Next option, or `None` at the end of the options (getopt returned -1).
    pub fn next_event(&mut self) -> Option<Event> {
        loop {
            if let Some((elem, off)) = self.cluster {
                return Some(self.short_option(elem, off));
            }
            let arg = self.args.get(self.idx)?;
            if arg.first() != Some(&b'-') || arg.len() == 1 {
                // Non-option (including a lone "-").
                if self.ctx.posixly_correct {
                    self.operands.extend_from_slice(&self.args[self.idx..]);
                    self.idx = self.args.len();
                    return None;
                }
                self.operands.push(arg.clone());
                self.idx += 1;
                continue;
            }
            if arg.as_slice() == b"--" {
                self.operands.extend_from_slice(&self.args[self.idx + 1..]);
                self.idx = self.args.len();
                return None;
            }
            self.idx += 1;
            if arg[1] == b'-' {
                return Some(self.long_option(&arg[2..]));
            }
            self.cluster = Some((self.idx - 1, 1));
        }
    }

    fn short_option(&mut self, elem: usize, off: usize) -> Event {
        let arg = &self.args[elem];
        let c = arg[off];
        let rest = &arg[off + 1..];
        if self.ctx.flavor == Flavor::Bsd && c == b'-' {
            // FreeBSD libc: a '-' inside a cluster starts a long option
            // ("-l-list" is "--list"); a trailing '-' is a silent error.
            self.cluster = None;
            if rest.is_empty() {
                return Event::Error(Vec::new());
            }
            return self.long_option(rest);
        }
        self.cluster = if rest.is_empty() {
            None
        } else {
            Some((elem, off + 1))
        };
        let pos = if c == b':' {
            None
        } else {
            OPTSTRING.iter().position(|&o| o == c)
        };
        let Some(pos) = pos else {
            return Event::Error(self.diag_invalid_short(c));
        };
        if OPTSTRING.get(pos + 1) != Some(&b':') {
            return Event::Opt(c, None);
        }
        self.cluster = None;
        if !rest.is_empty() {
            return Event::Opt(c, Some(rest.to_vec()));
        }
        match self.args.get(self.idx) {
            Some(next) => {
                self.idx += 1;
                Event::Opt(c, Some(next.clone()))
            }
            None => Event::Error(self.diag_short_missing(c)),
        }
    }

    /// `body` is the text after the `--` prefix.
    fn long_option(&mut self, body: &[u8]) -> Event {
        let (name, value) = match body.iter().position(|&b| b == b'=') {
            Some(eq) => (&body[..eq], Some(&body[eq + 1..])),
            None => (body, None),
        };
        let found = match LONG_OPTIONS.iter().position(|o| o.name == name) {
            Some(i) => Some(i),
            None => {
                let partial: Vec<usize> = (0..LONG_OPTIONS.len())
                    .filter(|&i| LONG_OPTIONS[i].name.starts_with(name))
                    .collect();
                let ambiguous: Vec<usize> = match partial.first() {
                    Some(&first) => {
                        let f = &LONG_OPTIONS[first];
                        partial
                            .iter()
                            .copied()
                            .filter(|&i| {
                                let o = &LONG_OPTIONS[i];
                                i == first || o.has_arg != f.has_arg || o.val != f.val
                            })
                            .collect()
                    }
                    None => Vec::new(),
                };
                if ambiguous.len() > 1 {
                    return Event::Error(self.diag_ambiguous(body, name, &ambiguous));
                }
                partial.first().copied()
            }
        };
        let Some(i) = found else {
            return Event::Error(self.diag(&[b"unrecognized option ", &self.quote(body)]));
        };
        let opt = &LONG_OPTIONS[i];
        match (value, opt.has_arg) {
            (Some(v), true) => Event::Opt(opt.val, Some(v.to_vec())),
            (Some(_), false) => {
                let shown = match self.ctx.flavor {
                    Flavor::Gnu => opt.name,
                    Flavor::Bsd => name,
                };
                Event::Error(self.diag(&[
                    b"option ",
                    &self.quote(shown),
                    b" doesn't allow an argument",
                ]))
            }
            (None, false) => Event::Opt(opt.val, None),
            (None, true) => match self.args.get(self.idx) {
                Some(next) => {
                    self.idx += 1;
                    Event::Opt(opt.val, Some(next.clone()))
                }
                None => {
                    let shown = match self.ctx.flavor {
                        Flavor::Gnu => opt.name,
                        Flavor::Bsd => body,
                    };
                    Event::Error(self.diag(&[
                        b"option ",
                        &self.quote(shown),
                        b" requires an argument",
                    ]))
                }
            },
        }
    }

    /// `'--text'` (glibc) or `` `--text' `` (macOS).
    fn quote(&self, text: &[u8]) -> Vec<u8> {
        let open: &[u8] = match self.ctx.flavor {
            Flavor::Gnu => b"'",
            Flavor::Bsd => b"`",
        };
        [open, b"--", text, b"'"].concat()
    }

    /// `<progname>: <parts...>\n`, as printed by libc.
    fn diag(&self, parts: &[&[u8]]) -> Vec<u8> {
        [
            &self.ctx.diag_progname,
            b": ".as_slice(),
            &parts.concat(),
            b"\n",
        ]
        .concat()
    }

    fn diag_ambiguous(&self, body: &[u8], name: &[u8], set: &[usize]) -> Vec<u8> {
        match self.ctx.flavor {
            Flavor::Gnu => {
                let mut msg = [
                    b"option ".as_slice(),
                    &self.quote(body),
                    b" is ambiguous; possibilities:",
                ]
                .concat();
                for &i in set {
                    msg.push(b' ');
                    msg.extend_from_slice(&self.quote(LONG_OPTIONS[i].name));
                }
                self.diag(&[&msg])
            }
            Flavor::Bsd => self.diag(&[b"option ", &self.quote(name), b" is ambiguous"]),
        }
    }

    fn diag_invalid_short(&self, c: u8) -> Vec<u8> {
        let text: &[u8] = match (self.ctx.flavor, self.ctx.posixly_correct) {
            (Flavor::Bsd, true) => b"illegal option -- ",
            _ => b"invalid option -- ",
        };
        self.diag_short(text, c)
    }

    fn diag_short_missing(&self, c: u8) -> Vec<u8> {
        self.diag_short(b"option requires an argument -- ", c)
    }

    fn diag_short(&self, text: &[u8], c: u8) -> Vec<u8> {
        let quoted: Vec<u8> = match self.ctx.flavor {
            Flavor::Gnu => vec![b'\'', c, b'\''],
            Flavor::Bsd => vec![c],
        };
        self.diag(&[text, &quoted])
    }
}

/// Exit for a getopt return value the C `switch` sends to `default:`.
fn getopt_error_exit(argv0: &[u8], diag: Vec<u8>, code: u8) -> Exit {
    let mut stderr = diag;
    stderr
        .extend_from_slice(format!("?? getopt returned character code 0x{code:x} ??\n").as_bytes());
    Exit {
        stdout: usage_text(argv0),
        stderr,
        code: 1,
    }
}

/// Emulates C `parse_opt`: applies options in argv order; `-V`, `-f`
/// errors and getopt errors end the program immediately.
pub fn parse_opt(args: &[Vec<u8>], ctx: &ParseContext) -> Result<Options, Exit> {
    let argv0: &[u8] = args.first().map_or(b"", Vec::as_slice);
    let mut opts = Options::default();
    let mut getopt = Getopt::new(args, ctx);
    while let Some(event) = getopt.next_event() {
        let (c, arg) = match event {
            Event::Error(diag) => return Err(getopt_error_exit(argv0, diag, b'?')),
            Event::Opt(c, arg) => (c, arg.unwrap_or_default()),
        };
        match c {
            b'a' => opts.server_host = arg,
            b'd' => opts.device = arg,
            b'r' => opts.password = Some(arg),
            b'c' => opts.cmd = Some(arg),
            b'l' => opts.list = true,
            b'f' => match parse_ports(&arg) {
                Some(pairs) => opts.forwards.extend(pairs),
                None => {
                    return Err(Exit {
                        stdout: usage_text(argv0),
                        stderr: b"Port Forwarding params were not correct\n".to_vec(),
                        code: 1,
                    });
                }
            },
            b'p' => opts.server_port = c_atoi(&arg),
            b'P' => opts.port_forward = true,
            b't' => {
                opts.term_mode = true;
                opts.sigwinch = true;
            }
            b's' => {
                opts.signals = true;
                opts.sigwinch = true;
            }
            b'w' => opts.wait = true,
            b'V' => {
                return Err(Exit {
                    stdout: version_line(),
                    stderr: Vec::new(),
                    code: 1,
                });
            }
            other => return Err(getopt_error_exit(argv0, Vec::new(), other)),
        }
    }
    opts.operands = getopt.into_operands();
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(list: &[&str]) -> Vec<Vec<u8>> {
        std::iter::once("prog")
            .chain(list.iter().copied())
            .map(|s| s.as_bytes().to_vec())
            .collect()
    }

    fn ctx(flavor: Flavor, posix: bool) -> ParseContext {
        ParseContext {
            flavor,
            posixly_correct: posix,
            diag_progname: b"P".to_vec(),
        }
    }

    fn events(flavor: Flavor, posix: bool, list: &[&str]) -> (Vec<Event>, Vec<Vec<u8>>) {
        let args = argv(list);
        let c = ctx(flavor, posix);
        let mut g = Getopt::new(&args, &c);
        let mut ev = Vec::new();
        while let Some(e) = g.next_event() {
            let stop = matches!(e, Event::Error(_));
            ev.push(e);
            if stop {
                break;
            }
        }
        (ev, g.into_operands())
    }

    fn opt(c: u8, a: Option<&str>) -> Event {
        Event::Opt(c, a.map(|s| s.as_bytes().to_vec()))
    }

    fn err(s: &str) -> Event {
        Event::Error(s.as_bytes().to_vec())
    }

    fn ops(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    const BOTH: [Flavor; 2] = [Flavor::Gnu, Flavor::Bsd];

    #[test]
    fn permutation_collects_operands_in_order() {
        for f in BOTH {
            let (ev, operands) = events(f, false, &["run", "-l", "x", "-p", "1", "-", "y"]);
            assert_eq!(ev, vec![opt(b'l', None), opt(b'p', Some("1"))]);
            assert_eq!(operands, ops(&["run", "x", "-", "y"]));
        }
    }

    #[test]
    fn posixly_correct_stops_at_first_operand() {
        for f in BOTH {
            let (ev, operands) = events(f, true, &["-l", "run", "-V", "--", "z"]);
            assert_eq!(ev, vec![opt(b'l', None)]);
            assert_eq!(operands, ops(&["run", "-V", "--", "z"]));
        }
    }

    #[test]
    fn double_dash_ends_options() {
        for f in BOTH {
            let (ev, operands) = events(f, false, &["a", "-l", "--", "-V", "--"]);
            assert_eq!(ev, vec![opt(b'l', None)]);
            assert_eq!(operands, ops(&["a", "-V", "--"]));
        }
    }

    #[test]
    fn clusters_and_attached_arguments() {
        for f in BOTH {
            let (ev, _) = events(f, false, &["-ltwp6968", "-a", "-x", "-sPd", "dev", "-Vr"]);
            assert_eq!(
                ev,
                vec![
                    opt(b'l', None),
                    opt(b't', None),
                    opt(b'w', None),
                    opt(b'p', Some("6968")),
                    opt(b'a', Some("-x")),
                    opt(b's', None),
                    opt(b'P', None),
                    opt(b'd', Some("dev")),
                    opt(b'V', None),
                    err(match f {
                        Flavor::Gnu => "P: option requires an argument -- 'r'\n",
                        Flavor::Bsd => "P: option requires an argument -- r\n",
                    }),
                ]
            );
        }
    }

    #[test]
    fn long_options_exact_abbrev_and_values() {
        for f in BOTH {
            let (ev, operands) = events(
                f,
                false,
                &[
                    "--port=1", "--port", "2", "--po", "-3", "--add=", "--l", "--cmd", "--", "x",
                ],
            );
            assert_eq!(
                ev,
                vec![
                    opt(b'p', Some("1")),
                    opt(b'p', Some("2")),
                    opt(b'p', Some("-3")),
                    opt(b'a', Some("")),
                    opt(b'l', None),
                    opt(b'c', Some("--")),
                ]
            );
            assert_eq!(operands, ops(&["x"]));
        }
    }

    #[test]
    fn invalid_short_option() {
        assert_eq!(
            events(Flavor::Gnu, false, &["-Z"]).0,
            vec![err("P: invalid option -- 'Z'\n")]
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["-Z"]).0,
            vec![err("P: invalid option -- Z\n")]
        );
        assert_eq!(
            events(Flavor::Gnu, true, &["-Z"]).0,
            vec![err("P: invalid option -- 'Z'\n")]
        );
        assert_eq!(
            events(Flavor::Bsd, true, &["-Z"]).0,
            vec![err("P: illegal option -- Z\n")]
        );
        assert_eq!(
            events(Flavor::Gnu, false, &["-l:"]).0[1],
            err("P: invalid option -- ':'\n")
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["-l:"]).0[1],
            err("P: invalid option -- :\n")
        );
    }

    #[test]
    fn missing_short_argument() {
        assert_eq!(
            events(Flavor::Gnu, false, &["-a"]).0,
            vec![err("P: option requires an argument -- 'a'\n")]
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["-a"]).0,
            vec![err("P: option requires an argument -- a\n")]
        );
    }

    #[test]
    fn unrecognized_long_option() {
        for (body, gnu, bsd) in [
            (
                "--bogus",
                "P: unrecognized option '--bogus'\n",
                "P: unrecognized option `--bogus'\n",
            ),
            (
                "--bogus=1",
                "P: unrecognized option '--bogus=1'\n",
                "P: unrecognized option `--bogus=1'\n",
            ),
            (
                "---x",
                "P: unrecognized option '---x'\n",
                "P: unrecognized option `---x'\n",
            ),
        ] {
            assert_eq!(events(Flavor::Gnu, false, &[body]).0, vec![err(gnu)]);
            assert_eq!(events(Flavor::Bsd, false, &[body]).0, vec![err(bsd)]);
        }
    }

    #[test]
    fn long_missing_argument() {
        for (body, gnu, bsd) in [
            (
                "--addr",
                "P: option '--address' requires an argument\n",
                "P: option `--addr' requires an argument\n",
            ),
            (
                "--pa",
                "P: option '--password' requires an argument\n",
                "P: option `--pa' requires an argument\n",
            ),
        ] {
            assert_eq!(events(Flavor::Gnu, false, &[body]).0, vec![err(gnu)]);
            assert_eq!(events(Flavor::Bsd, false, &[body]).0, vec![err(bsd)]);
        }
    }

    #[test]
    fn long_given_argument() {
        for (body, gnu, bsd) in [
            (
                "--wait=1",
                "P: option '--wait' doesn't allow an argument\n",
                "P: option `--wait' doesn't allow an argument\n",
            ),
            (
                "--wai=1",
                "P: option '--wait' doesn't allow an argument\n",
                "P: option `--wai' doesn't allow an argument\n",
            ),
            (
                "--list=",
                "P: option '--list' doesn't allow an argument\n",
                "P: option `--list' doesn't allow an argument\n",
            ),
        ] {
            assert_eq!(events(Flavor::Gnu, false, &[body]).0, vec![err(gnu)]);
            assert_eq!(events(Flavor::Bsd, false, &[body]).0, vec![err(bsd)]);
        }
    }

    #[test]
    fn ambiguous_long_option() {
        assert_eq!(
            events(Flavor::Gnu, false, &["--p=5"]).0,
            vec![err(
                "P: option '--p=5' is ambiguous; possibilities: '--port' '--password'\n"
            )]
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["--p=5"]).0,
            vec![err("P: option `--p' is ambiguous\n")]
        );
        assert_eq!(
            events(Flavor::Gnu, false, &["--=x"]).0,
            vec![err(
                "P: option '--=x' is ambiguous; possibilities: '--address' '--device' '--list' '--port' \
                 '--terminal' '--signals' '--wait' '--password' '--cmd'\n"
            )]
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["--=x"]).0,
            vec![err("P: option `--' is ambiguous\n")]
        );
    }

    #[test]
    fn dash_inside_cluster() {
        assert_eq!(
            events(Flavor::Gnu, false, &["-l-list"]).0[1],
            err("P: invalid option -- '-'\n")
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["-l-list"]).0,
            vec![opt(b'l', None), opt(b'l', None)]
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["-l-"]).0,
            vec![opt(b'l', None), err("")]
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["-l-x"]).0[1],
            err("P: unrecognized option `--x'\n")
        );
        assert_eq!(
            events(Flavor::Bsd, false, &["-l-port", "9"]).0[1],
            opt(b'p', Some("9"))
        );
    }

    #[test]
    fn atoi_semantics() {
        assert_eq!(c_atoi(b"6968"), 6968);
        assert_eq!(c_atoi(b" \t\n\x0b\x0c\r+12x"), 12);
        assert_eq!(c_atoi(b"-5"), -5);
        assert_eq!(c_atoi(b"junk"), 0);
        assert_eq!(c_atoi(b""), 0);
        assert_eq!(c_atoi(b"+-1"), 0);
        assert_eq!(c_atoi(b"0x10"), 0);
        assert_eq!(c_atoi(b"4294967297"), 1);
        assert_eq!(c_atoi(b"2147483648"), i32::MIN);
        assert_eq!(c_atoi(b"99999999999999999999"), -1); // LONG_MAX truncated
        assert_eq!(c_atoi(b"-99999999999999999999"), 0); // LONG_MIN truncated
        assert_eq!(c_strtol(b"-9223372036854775808"), i64::MIN);
        assert_eq!(c_strtol(b"9223372036854775807"), i64::MAX);
        assert_eq!(c_strtol(b"9223372036854775808"), i64::MAX);
    }

    #[test]
    fn strtoul_semantics() {
        assert_eq!(c_strtoul(b" 5x"), 5);
        assert_eq!(c_strtoul(b"x"), 0);
        assert_eq!(c_strtoul(b"-0"), 0);
        assert_eq!(c_strtoul(b"-1"), u64::MAX);
        assert_eq!(c_strtoul(b"18446744073709551615"), u64::MAX);
        assert_eq!(c_strtoul(b"18446744073709551616"), u64::MAX);
        assert_eq!(c_strtoul(b"-18446744071562067969"), 2_147_483_647);
    }

    #[test]
    fn parse_ports_semantics() {
        let p = |l, r| PortPair {
            local: l,
            remote: r,
        };
        assert_eq!(parse_ports(b"1:2"), Some(vec![p(1, 2)]));
        assert_eq!(parse_ports(b",,1::2,,3,4:"), Some(vec![p(1, 2), p(3, 4)]));
        assert_eq!(parse_ports(b"x: 7"), Some(vec![p(0, 7)]));
        assert_eq!(
            parse_ports(b"2147483647:-0"),
            Some(vec![p(2_147_483_647, 0)])
        );
        assert_eq!(
            parse_ports(b"-18446744071562067969:1"),
            Some(vec![p(2_147_483_647, 1)])
        );
        assert_eq!(parse_ports(b"2147483648:1"), None);
        assert_eq!(parse_ports(b"1:2147483648"), None);
        assert_eq!(parse_ports(b"-1:2"), None);
        assert_eq!(parse_ports(b"1"), None);
        assert_eq!(parse_ports(b"1:2:3"), None);
        assert_eq!(parse_ports(b""), None);
        assert_eq!(parse_ports(b",:"), None);
    }

    fn parse(list: &[&str]) -> Result<Options, Exit> {
        parse_opt(&argv(list), &ctx(Flavor::Gnu, false))
    }

    #[test]
    fn parse_opt_effects() {
        let o = parse(&[
            "-a", "h", "-p", "70000", "-d", "D", "-r", "pw", "-c", "login", "-ts", "-w", "-P",
            "-f", "1:2", "-f", "3,4", "x",
        ])
        .unwrap();
        assert_eq!(o.server_host, b"h");
        assert_eq!(o.server_port, 70000);
        assert_eq!(o.device, b"D");
        assert_eq!(o.password.as_deref(), Some(b"pw".as_slice()));
        assert_eq!(o.cmd.as_deref(), Some(b"login".as_slice()));
        assert!(o.term_mode && o.signals && o.sigwinch && o.wait && o.port_forward && !o.list);
        assert_eq!(
            o.forwards,
            vec![
                PortPair {
                    local: 1,
                    remote: 2
                },
                PortPair {
                    local: 3,
                    remote: 4
                }
            ]
        );
        assert_eq!(o.operands, ops(&["x"]));
        let d = parse(&[]).unwrap();
        assert_eq!(d, Options::default());
        assert_eq!(d.server_host, b"localhost");
        assert_eq!(d.server_port, 6968);
    }

    #[test]
    fn parse_opt_exits_in_argv_order() {
        let v = parse(&["-l", "x", "-V", "-Z"]).unwrap_err();
        assert_eq!(
            v,
            Exit {
                stdout: version_line(),
                stderr: Vec::new(),
                code: 1
            }
        );
        let e = parse(&["-Z", "-V"]).unwrap_err();
        assert_eq!(
            e.stderr,
            b"P: invalid option -- 'Z'\n?? getopt returned character code 0x3f ??\n"
        );
        assert_eq!(e.stdout, usage_text(b"prog"));
        assert_eq!(e.code, 1);
        let f = parse(&["-f", "1", "-V"]).unwrap_err();
        assert_eq!(f.stderr, b"Port Forwarding params were not correct\n");
        assert_eq!(f.stdout, usage_text(b"prog"));
        assert_eq!(f.code, 1);
    }

    #[test]
    fn usage_text_layout() {
        let u = usage_text(b"/x/novacom");
        let expected_head = format!(
            "version: {VERSION}\nusage: /x/novacom [-a address] [-p port] [-t] [-l] [-d device] [-c cmd] \
             [-r password] [-w] <command>\n/x/novacom [-V]\n/x/novacom [-a address] [-p port] -P[ [-f \
             <localport:remoteport,...>] ]\noptions:\n"
        );
        assert!(u.starts_with(expected_head.as_bytes()));
        assert!(u.ends_with(b"\t-P: Port Forwarding Enabled\n\t-f: ports to forward\n"));
        assert_eq!(u.iter().filter(|&&b| b == b'\n').count(), 22);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn basename_fallback() {
        assert_eq!(basename(b"/a/b/novacom"), b"novacom");
        assert_eq!(basename(b"novacom"), b"novacom");
    }
}
