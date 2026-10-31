//! Terminal attributes of fds 0 and 1: C `saveconsole`, `resetconsole` and
//! `setconsole`.

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

/// C `CONMODE_e`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `-t`: no local input or output processing except signal keys.
    TermSupport,
    NoEcho,
    Echo,
}

/// Changes `t` as C `setconsole(mode)` does.
pub fn apply_mode(mode: Mode, t: &mut Termios) {
    match mode {
        Mode::TermSupport => {
            t.c_lflag = libc::ISIG;
            // Platform quirk P2, kept (improvement I7): C writes '\0' to
            // mean "disabled". That is `_POSIX_VDISABLE` on Linux, but on
            // macOS `_POSIX_VDISABLE` is 255, so NUL (Ctrl-@) becomes the
            // interrupt, quit and suspend character there, with ISIG on.
            for i in [libc::VINTR, libc::VQUIT, libc::VSUSP] {
                t.c_cc[i] = 0;
            }
        }
        Mode::NoEcho => t.c_lflag &= !libc::ECHO,
        // Bug B6, kept: `&=` instead of `|=` clears every other local flag
        // (ICANON, ISIG, ...) until the console is restored at exit.
        Mode::Echo => t.c_lflag &= libc::ECHO,
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
    apply_mode(mode, &mut ti);
    apply_mode(mode, &mut to);
    sys::tcsetattr_now(STDIN_FD, &ti);
    sys::tcsetattr_now(STDOUT_FD, &to);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_lflag(lflag: libc::tcflag_t) -> Termios {
        let mut t = sys::zeroed_termios();
        t.c_lflag = lflag;
        t.c_iflag = libc::ICRNL;
        t.c_oflag = libc::OPOST;
        t.c_cc[libc::VINTR] = 3;
        t.c_cc[libc::VQUIT] = 0x1c;
        t.c_cc[libc::VSUSP] = 0x1a;
        t.c_cc[libc::VEOF] = 4;
        t
    }

    fn applied(mode: Mode, t: &Termios) -> Termios {
        let mut t = *t;
        apply_mode(mode, &mut t);
        t
    }

    #[test]
    fn mode_flags() {
        let all = with_lflag(libc::ECHO | libc::ICANON | libc::ISIG);
        let lflag = |m, t: &Termios| applied(m, t).c_lflag;
        assert_eq!(lflag(Mode::NoEcho, &all), libc::ICANON | libc::ISIG);
        assert_eq!(lflag(Mode::Echo, &all), libc::ECHO);
        let no_echo = with_lflag(libc::ICANON | libc::ISIG);
        assert_eq!(lflag(Mode::Echo, &no_echo), 0);
        for m in [Mode::NoEcho, Mode::Echo] {
            assert_eq!(applied(m, &all).c_cc, all.c_cc);
        }
    }

    #[test]
    fn terminal_support_mode() {
        let all = with_lflag(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        let t = applied(Mode::TermSupport, &all);
        assert_eq!(t.c_lflag, libc::ISIG);
        assert_eq!(
            [
                t.c_cc[libc::VINTR],
                t.c_cc[libc::VQUIT],
                t.c_cc[libc::VSUSP]
            ],
            [0, 0, 0]
        );
        // Everything else is unchanged.
        assert_eq!((t.c_iflag, t.c_oflag), (all.c_iflag, all.c_oflag));
        assert_eq!(t.c_cc[libc::VEOF], 4);
    }
}
