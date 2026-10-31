//! Terminal attributes of fds 0 and 1: C `saveconsole`, `resetconsole` and
//! `setconsole` (echo modes only; terminal mode is not implemented yet).

use crate::sys::{self, STDIN_FD, STDOUT_FD, Termios};

/// Attributes saved by [`save`] (C `oldstdin`, `oldstdout`).
pub struct Saved {
    stdin: Termios,
    stdout: Termios,
}

/// C `saveconsole`: `tcgetattr` of fds 0 and 1. A failed call leaves the
/// zero-initialized value, as for the C statics.
pub fn save() -> Saved {
    Saved {
        stdin: sys::tcgetattr(STDIN_FD).unwrap_or_else(sys::zeroed_termios),
        stdout: sys::tcgetattr(STDOUT_FD).unwrap_or_else(sys::zeroed_termios),
    }
}

impl Saved {
    /// C `resetconsole`: `tcsetattr(TCSANOW)` on fds 0 and 1, results
    /// ignored (zeroed values included, as in C).
    pub fn restore(&self) {
        sys::tcsetattr_now(STDIN_FD, &self.stdin);
        sys::tcsetattr_now(STDOUT_FD, &self.stdout);
    }
}

/// C `CONMODE_e` values used by the password prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    NoEcho,
    Echo,
}

/// The local-mode flags C `setconsole` sets for `mode`.
pub fn apply_mode(mode: Mode, lflag: libc::tcflag_t) -> libc::tcflag_t {
    match mode {
        Mode::NoEcho => lflag & !libc::ECHO,
        // Bug B6, kept: `&=` instead of `|=` clears every other local flag
        // (ICANON, ISIG, ...) until the console is restored at exit.
        Mode::Echo => lflag & libc::ECHO,
    }
}

/// C `setconsole(mode)`: if `tcgetattr` fails on fd 0 or on fd 1, nothing
/// changes, so echo stays on when stdout is not a terminal even if stdin
/// is one.
pub fn set_mode(mode: Mode) {
    let Some(mut ti) = sys::tcgetattr(STDIN_FD) else {
        return;
    };
    let Some(mut to) = sys::tcgetattr(STDOUT_FD) else {
        return;
    };
    ti.c_lflag = apply_mode(mode, ti.c_lflag);
    to.c_lflag = apply_mode(mode, to.c_lflag);
    sys::tcsetattr_now(STDIN_FD, &ti);
    sys::tcsetattr_now(STDOUT_FD, &to);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_flags() {
        let all = libc::ECHO | libc::ICANON | libc::ISIG;
        assert_eq!(apply_mode(Mode::NoEcho, all), libc::ICANON | libc::ISIG);
        assert_eq!(apply_mode(Mode::Echo, all), libc::ECHO);
        assert_eq!(apply_mode(Mode::Echo, libc::ICANON | libc::ISIG), 0);
    }
}
