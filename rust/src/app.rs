//! Program flow: emulation of C `main()`.

use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::cli::{self, Options, ParseContext};
use crate::console::{self, Mode};
use crate::control;
use crate::devlist::{self, FoundDevice};
use crate::net;
use crate::session;
use crate::stdio::StdioOut;
use crate::sys::{self, FdReader, FdWriter, STDERR_FD, STDIN_FD, STDOUT_FD};

/// Delay between `-w` retries (C `usleep(500000)`).
const WAIT_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Exit code for paths this build does not implement yet (`EX_SOFTWARE`).
pub const NOT_IMPLEMENTED_EXIT: u8 = 70;

/// Entry point for the binary: reads the process environment and runs.
pub fn main_entry() -> u8 {
    use std::os::unix::ffi::OsStrExt;
    sys::default_sigpipe();
    let args: Vec<Vec<u8>> = std::env::args_os().map(|a| a.as_bytes().to_vec()).collect();
    let argv0: &[u8] = args.first().map_or(b"", Vec::as_slice);
    let ctx = ParseContext {
        flavor: cli::FLAVOR,
        posixly_correct: std::env::var_os("POSIXLY_CORRECT").is_some(),
        diag_progname: cli::diag_progname(argv0),
    };
    // C stdio stdout for printf/puts output; stderr is unbuffered. The
    // session writes fds 1 and 2 directly, bypassing `out`.
    let mut out = StdioOut::new(FdWriter(STDOUT_FD), sys::isatty(STDOUT_FD));
    let mut err = FdWriter(STDERR_FD);
    let code = match cli::parse_opt(&args, &ctx) {
        Err(exit) => report_exit(&exit, &mut out, &mut err),
        Ok(opts) => {
            // C `saveconsole()` and its `atexit(resetconsole)`: every exit
            // after option parsing restores the console.
            let saved = console::save();
            let code = run_parsed(opts, argv0, &mut out, &mut err);
            saved.restore();
            code
        }
    };
    // C `exit()` flushes stdio after the atexit handlers.
    let _ = out.flush();
    code
}

/// Emulates C `main()` without console save/restore. Output write errors
/// are ignored, as in C.
pub fn run(args: &[Vec<u8>], ctx: &ParseContext, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let argv0: &[u8] = args.first().map_or(b"", Vec::as_slice);
    match cli::parse_opt(args, ctx) {
        Ok(opts) => run_parsed(opts, argv0, out, err),
        Err(exit) => report_exit(&exit, out, err),
    }
}

/// Option parsing ended the program.
fn report_exit(exit: &cli::Exit, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let _ = err.write_all(&exit.stderr);
    let _ = out.write_all(&exit.stdout);
    exit.code
}

/// C `main()` after option parsing.
fn run_parsed(opts: Options, argv0: &[u8], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    // C keeps the session id of a matched device in a global that only a
    // later match with a non-empty session id overwrites. With `-w`, a
    // match with a negative port can leave its session id behind.
    let mut session = Vec::new();
    let device = loop {
        let Some(stream) = net::connect_socket(&opts.server_host, opts.server_port) else {
            let _ = err.write_all(b"failed to connect to server\n");
            return 1;
        };
        let mut reader = BufReader::new(stream);
        if opts.list {
            let _ = devlist::list_devices(&mut reader, &opts.device, out);
            return 0;
        }
        let found = devlist::find_device(&mut reader, &opts.device);
        drop(reader); // C closes the device-list socket here.
        if let Some(d) = &found {
            if !d.session.is_empty() {
                session.clone_from(&d.session);
            }
        }
        match found {
            Some(d) if d.port >= 0 => break FoundDevice { session, ..d },
            _ if opts.wait => std::thread::sleep(WAIT_RETRY_DELAY),
            _ => {
                let _ = err.write_all(b"unable to find device\n");
                return 1;
            }
        }
    };
    after_device_found(opts, &device, argv0, out, err)
}

/// Everything after a device is found. This is the single boundary for
/// paths not implemented yet: port forwarding, terminal mode and signal
/// forwarding (also with `-c`).
fn after_device_found(
    mut opts: Options,
    device: &FoundDevice,
    argv0: &[u8],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let what: &[u8] = if opts.port_forward {
        b"port forwarding"
    } else if opts.cmd.is_none() && opts.operands.is_empty() {
        let _ = out.write_all(&cli::usage_text(argv0));
        return 1;
    } else if opts.term_mode {
        b"terminal mode"
    } else if opts.signals {
        b"signal forwarding"
    } else if let Some(cmd) = opts.cmd.take() {
        return control_command(&cmd, opts, device, out, err);
    } else {
        return remote_command(&opts, device, err);
    };
    let _ = err.write_all(&[argv0, b": ", what, b" is not implemented in this build\n"].concat());
    NOT_IMPLEMENTED_EXIT
}

/// C `main()` `-c` branch: builds the control command, sends it to
/// [`control::CTRL_PORT`] and checks the response. Operands are ignored.
/// For `list` the response body is never read (bug B12, kept).
fn control_command(
    cmd: &[u8],
    mut opts: Options,
    device: &FoundDevice,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let password = opts.password.take();
    let command = match control::build(cmd, device, password, || getpsw(out)) {
        Ok(c) => c,
        Err(msg) => {
            let _ = err.write_all(&msg);
            return 1;
        }
    };
    let Some(sock) = net::connect_socket(&opts.server_host, control::CTRL_PORT) else {
        let _ = err.write_all(b"failed to connect to server\n");
        return 1;
    };
    send_command(&sock, &command, err);
    match session::parse_response(&mut &sock) {
        Ok(()) => 0,
        Err(msg) => {
            let _ = err.write_all(&msg);
            1
        }
    }
}

/// Emulates C `getpsw`: prompt on stdout, read `%64s` from stdin with echo
/// off, then echo "on" (bug B6) and a newline.
fn getpsw(out: &mut dyn Write) -> Vec<u8> {
    let _ = out.write_all(b"Please enter password: ");
    console::set_mode(Mode::NoEcho);
    // C stdio flushes a line-buffered stdout (a terminal) before it reads
    // a line-buffered stdin (a terminal); otherwise the prompt waits in the
    // stdout buffer.
    if sys::isatty(STDIN_FD) && sys::isatty(STDOUT_FD) {
        let _ = out.flush();
    }
    let password = control::scan_password(&mut FdReader(STDIN_FD));
    console::set_mode(Mode::Echo);
    let _ = out.write_all(b"\n");
    password
}

/// Sends `command` on `sock`. C makes one blocking `send()` and reports
/// only an error (< 0).
fn send_command(sock: &TcpStream, command: &[u8], err: &mut dyn Write) {
    sys::allow_sigpipe(sock);
    let mut rest = command;
    while !rest.is_empty() {
        match sys::send(sock, rest) {
            Ok(n) if n > 0 => rest = &rest[n..],
            Ok(_) => break,
            Err(_) => {
                let _ = err.write_all(b"novacom: unable to send command to server\n");
                break;
            }
        }
    }
}

/// C `main()` from building the command to the end, without `-t`/`-s`.
/// The data stream uses the process fds 0, 1 and 2 directly.
fn remote_command(opts: &Options, device: &FoundDevice, err: &mut dyn Write) -> u8 {
    let command = session::build_command(&opts.operands);
    let Some(sock) = net::connect_socket(&opts.server_host, device.port) else {
        let _ = err.write_all(b"failed to connect to server\n");
        return 1;
    };
    send_command(&sock, &command, err);
    if let Err(msg) = session::parse_response(&mut &sock) {
        let _ = err.write_all(&msg);
        return 1;
    }
    let rc = session::data_xfer(&sock);
    if rc != 0 {
        let _ = err.write_all(b"novacom: unexpected EOF from server\n");
    }
    // Exit status is the low byte of C main's return value.
    rc.to_le_bytes()[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Flavor;
    use std::net::TcpListener;

    /// Serves `body` to each connection, `count` times.
    fn serve(body: &'static [u8], count: usize) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..count {
                let (mut s, _) = listener.accept().unwrap();
                let _ = s.write_all(body);
            }
        });
        port
    }

    fn run_args(list: &[&str]) -> (u8, Vec<u8>, Vec<u8>) {
        let args: Vec<Vec<u8>> = std::iter::once("prog")
            .chain(list.iter().copied())
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let ctx = ParseContext {
            flavor: Flavor::Gnu,
            posixly_correct: false,
            diag_progname: b"prog".to_vec(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args, &ctx, &mut out, &mut err);
        (code, out, err)
    }

    const ONE: &[u8] = b"6970 n1 usb castle\n";

    /// `-c` cases connect to port 6971 and are tested only in
    /// `tests/differential.rs`, which first binds that port itself.
    #[test]
    fn not_implemented_paths() {
        for (extra, what) in [
            (&["-t", "run", "file:///bin/ls"][..], "terminal mode"),
            (&["-s", "run"][..], "signal forwarding"),
            (&["-ts", "run"][..], "terminal mode"),
            (&["-P"][..], "port forwarding"),
        ] {
            let port = serve(ONE, 1).to_string();
            let mut args = vec!["-a", "127.0.0.1", "-p", &port];
            args.extend_from_slice(extra);
            let (code, out, err) = run_args(&args);
            assert_eq!(code, NOT_IMPLEMENTED_EXIT);
            assert!(out.is_empty());
            assert_eq!(
                err,
                format!("prog: {what} is not implemented in this build\n").into_bytes()
            );
        }
    }

    #[test]
    fn usage_when_no_command() {
        let port = serve(ONE, 1).to_string();
        let (code, out, err) = run_args(&["-a", "127.0.0.1", "-p", &port]);
        assert_eq!((code, out, err), (1, cli::usage_text(b"prog"), Vec::new()));
    }

    #[test]
    fn list_mode() {
        let port = serve(ONE, 1).to_string();
        let (code, out, err) = run_args(&["-l", "-p", &port]);
        assert_eq!((code, out, err), (0, ONE.to_vec(), Vec::new()));
    }
}
