//! Program flow: emulation of C `main()` for the device-list slice.

use std::io::{BufReader, Write};
use std::time::Duration;

use crate::cli::{self, Options, ParseContext};
use crate::devlist::{self, FoundDevice};
use crate::net;

/// Delay between `-w` retries (C `usleep(500000)`).
const WAIT_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Exit code for paths this build does not implement yet (`EX_SOFTWARE`).
pub const NOT_IMPLEMENTED_EXIT: u8 = 70;

/// Entry point for the binary: reads the process environment and runs.
pub fn main_entry() -> u8 {
    use std::os::unix::ffi::OsStrExt;
    let args: Vec<Vec<u8>> = std::env::args_os().map(|a| a.as_bytes().to_vec()).collect();
    let argv0: &[u8] = args.first().map_or(b"", Vec::as_slice);
    let ctx = ParseContext {
        flavor: cli::FLAVOR,
        posixly_correct: std::env::var_os("POSIXLY_CORRECT").is_some(),
        diag_progname: cli::diag_progname(argv0),
    };
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let code = run(&args, &ctx, &mut out, &mut err);
    // C ignores stdio errors; a failed flush does not change the exit code.
    let _ = out.flush();
    code
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

/// Everything after a device is found. This is the single boundary of the
/// current slice: only the "no command" usage path is implemented.
fn after_device_found(
    opts: &Options,
    _device: &FoundDevice,
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
    } else {
        b"remote command execution"
    };
    let _ = err.write_all(&[argv0, b": ", what, b" is not implemented in this build\n"].concat());
    NOT_IMPLEMENTED_EXIT
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
            (&["run", "file:///bin/ls"][..], "remote command execution"),
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
