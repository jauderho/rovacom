//! Program flow: emulation of C `main()`.

use std::io::{BufReader, Write};
use std::time::Duration;

use crate::cli::{self, Options, ParseContext};
use crate::devlist::{self, FoundDevice};
use crate::net;
use crate::session;
use crate::sys::{self, FdWriter, STDERR_FD, STDOUT_FD};

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
    // Raw, unbuffered fds 1 and 2: the session closes fd 1 and writes to
    // both fds directly, so no output may wait in a std buffer.
    run(
        &args,
        &ctx,
        &mut FdWriter(STDOUT_FD),
        &mut FdWriter(STDERR_FD),
    )
}

/// Emulates C `main()`. Output write errors are ignored, as in C.
pub fn run(args: &[Vec<u8>], ctx: &ParseContext, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let argv0: &[u8] = args.first().map_or(b"", Vec::as_slice);
    let opts = match cli::parse_opt(args, ctx) {
        Ok(opts) => opts,
        Err(exit) => {
            let _ = err.write_all(&exit.stderr);
            let _ = out.write_all(&exit.stdout);
            return exit.code;
        }
    };
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
        match found {
            Some(d) if d.port >= 0 => break d,
            _ if opts.wait => std::thread::sleep(WAIT_RETRY_DELAY),
            _ => {
                let _ = err.write_all(b"unable to find device\n");
                return 1;
            }
        }
    };
    after_device_found(&opts, &device, argv0, out, err)
}

/// Everything after a device is found. This is the single boundary for
/// paths not implemented yet: port forwarding, control commands, terminal
/// mode and signal forwarding.
fn after_device_found(
    opts: &Options,
    device: &FoundDevice,
    argv0: &[u8],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let what: &[u8] = if opts.port_forward {
        b"port forwarding"
    } else if opts.cmd.is_some() {
        b"control command"
    } else if opts.operands.is_empty() {
        let _ = out.write_all(&cli::usage_text(argv0));
        return 1;
    } else if opts.term_mode {
        b"terminal mode"
    } else if opts.signals {
        b"signal forwarding"
    } else {
        return remote_command(opts, device, err);
    };
    let _ = err.write_all(&[argv0, b": ", what, b" is not implemented in this build\n"].concat());
    NOT_IMPLEMENTED_EXIT
}

/// C `main()` from building the command to the end, without `-t`/`-s`.
/// The data stream uses the process fds 0, 1 and 2 directly.
fn remote_command(opts: &Options, device: &FoundDevice, err: &mut dyn Write) -> u8 {
    let command = session::build_command(&opts.operands);
    let Some(sock) = net::connect_socket(&opts.server_host, device.port) else {
        let _ = err.write_all(b"failed to connect to server\n");
        return 1;
    };
    sys::allow_sigpipe(&sock);
    // C makes one blocking send(); only an error (< 0) is reported.
    let mut rest = command.as_slice();
    while !rest.is_empty() {
        match sys::send(&sock, rest) {
            Ok(n) if n > 0 => rest = &rest[n..],
            Ok(_) => break,
            Err(_) => {
                let _ = err.write_all(b"novacom: unable to send command to server\n");
                break;
            }
        }
    }
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

    #[test]
    fn not_implemented_paths() {
        for (extra, what) in [
            (&["-t", "run", "file:///bin/ls"][..], "terminal mode"),
            (&["-s", "run"][..], "signal forwarding"),
            (&["-ts", "run"][..], "terminal mode"),
            (&["-c", "login"][..], "control command"),
            (&["-P"][..], "port forwarding"),
            (&["-P", "-c", "list", "x"][..], "port forwarding"),
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
