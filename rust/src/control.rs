//! Control commands (`-c`): the C `main()` `-c` branch, `prepare_cmd`, the
//! `scanf("%64s")` password input of `getpsw`, and the SHA-1 auth hash.

use std::io::{ErrorKind, Read};

use sha1::{Digest, Sha1};

use crate::cli::c_isspace;
use crate::devlist::{FoundDevice, c_str};

/// C `NOVACOM_CTRLPORT`: control commands always go to this port on the
/// server host.
pub const CTRL_PORT: i32 = 6971;

/// Field width of the C `scanf("%64s")` password read.
const PASSWORD_WIDTH: usize = 64;

/// Command selected by the `-c` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    List,
    Logout,
    /// `login`, `add` or `remove`: the first `prefix_len` bytes of the `-c`
    /// value name the command and a password hash is needed.
    Auth {
        prefix_len: usize,
    },
}

/// C `strncmp` prefix matching, in C order: `-c listfoo` is `list` and
/// `-c loginxyz` is `login`.
pub fn select(cmd: &[u8]) -> Option<Action> {
    if cmd.starts_with(b"list") {
        Some(Action::List)
    } else if cmd.starts_with(b"login") {
        Some(Action::Auth { prefix_len: 5 })
    } else if cmd.starts_with(b"logout") {
        Some(Action::Logout)
    } else if cmd.starts_with(b"add") {
        Some(Action::Auth { prefix_len: 3 })
    } else if cmd.starts_with(b"remove") {
        Some(Action::Auth { prefix_len: 6 })
    } else {
        None
    }
}

/// Builds the command sent to [`CTRL_PORT`], or returns the stderr message.
///
/// `password` is the `-r` value; `getpsw` is called to prompt for one when
/// the device has a session id and no `-r` was given. The password bytes
/// are overwritten after use.
pub fn build(
    cmd: &[u8],
    device: &FoundDevice,
    password: Option<Vec<u8>>,
    getpsw: impl FnOnce() -> Vec<u8>,
) -> Result<Vec<u8>, Vec<u8>> {
    match select(cmd) {
        Some(Action::List) => Ok(b"list host://".to_vec()),
        Some(Action::Logout) => Ok([&b"logout dev://"[..], &device.nduid, b"\n"].concat()),
        Some(Action::Auth { prefix_len }) => {
            prepare_cmd(&cmd[..prefix_len], device, password, getpsw)
        }
        None => Err([&b"unsupported command("[..], cmd, b")\n"].concat()),
    }
}

/// Emulates C `prepare_cmd`: `<name> dev://<nduid> <hash>`, no newline.
fn prepare_cmd(
    name: &[u8],
    device: &FoundDevice,
    password: Option<Vec<u8>>,
    getpsw: impl FnOnce() -> Vec<u8>,
) -> Result<Vec<u8>, Vec<u8>> {
    let has_session = !device.session.is_empty();
    let mut password = match password {
        None if has_session => Some(getpsw()),
        p => p,
    };
    // C uses the password as a C string.
    let pw = password.as_deref().map_or(&b""[..], c_str);
    let result = if has_session && !pw.is_empty() {
        let hash = auth_hash(&device.nduid, pw, &device.session);
        Ok([name, b" dev://", &device.nduid, b" ", &hash].concat())
    } else if has_session {
        Err(b"Please specify password\n".to_vec())
    } else {
        Err(b"Device does not require authentication\n".to_vec())
    };
    // C zeroes the password after hashing. The copy in the process argv
    // (`-r`) is not scrubbed here.
    if let Some(p) = password.as_mut() {
        p.fill(0);
        std::hint::black_box(p);
    }
    result
}

/// `hex(SHA1(hex(SHA1(nduid ‖ password)) ‖ session))`, lowercase.
///
/// New bug B14: C formats `nduid ‖ password` with `snprintf` into its
/// 4096-byte `command` buffer and hashes `len` bytes, where `len` is the
/// untruncated length; for inputs over 4095 bytes it reads past the buffer
/// (undefined behavior). Here the full input is hashed.
pub fn auth_hash(nduid: &[u8], password: &[u8], session: &[u8]) -> [u8; 40] {
    let inner = sha1_hex(&[nduid, password].concat());
    sha1_hex(&[&inner[..], session].concat())
}

/// SHA-1 digest as lowercase hex (C `SHA1Result` `Message_Digest_Str`).
fn sha1_hex(data: &[u8]) -> [u8; 40] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha1::digest(data);
    let mut out = [0u8; 40];
    for (pair, b) in out.chunks_exact_mut(2).zip(digest.iter()) {
        pair[0] = HEX[usize::from(b >> 4)];
        pair[1] = HEX[usize::from(b & 0x0f)];
    }
    out
}

/// Emulates `scanf("%64s", pwd)` on a zero-filled buffer: skips C-locale
/// whitespace, then reads up to 64 non-whitespace bytes. EOF or a read
/// error ends the input (an empty result if nothing was read). Reads one
/// byte at a time, so nothing past the delimiter is consumed.
pub fn scan_password<R: Read>(r: &mut R) -> Vec<u8> {
    let mut pw = Vec::new();
    while pw.len() < PASSWORD_WIDTH {
        match read_byte(r) {
            Some(b) if c_isspace(b) && pw.is_empty() => {}
            Some(b) if c_isspace(b) => break,
            Some(b) => pw.push(b),
            None => break,
        }
    }
    pw
}

fn read_byte<R: Read>(r: &mut R) -> Option<u8> {
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(1) => return Some(byte[0]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NDUID: &[u8] = b"FEDCBA9876543210FEDCBA9876543210FEDCBA98";

    fn dev(session: &[u8]) -> FoundDevice {
        FoundDevice {
            port: 7000,
            nduid: NDUID.to_vec(),
            session: session.to_vec(),
        }
    }

    fn no_prompt() -> Vec<u8> {
        panic!("unexpected password prompt")
    }

    #[test]
    fn prefix_matching_order() {
        use Action::*;
        let cases: &[(&[u8], Option<Action>)] = &[
            (b"list", Some(List)),
            (b"listfoo", Some(List)),
            (b"lis", None),
            (b"login", Some(Auth { prefix_len: 5 })),
            (b"loginxyz", Some(Auth { prefix_len: 5 })),
            (b"logout", Some(Logout)),
            (b"logoutx", Some(Logout)),
            (b"logou", None),
            (b"log", None),
            (b"add", Some(Auth { prefix_len: 3 })),
            (b"address", Some(Auth { prefix_len: 3 })),
            (b"remove", Some(Auth { prefix_len: 6 })),
            (b"removeall", Some(Auth { prefix_len: 6 })),
            (b"remov", None),
            (b"", None),
            (b"LIST", None),
            (b" list", None),
        ];
        for (cmd, want) in cases {
            assert_eq!(select(cmd), *want, "{:?}", String::from_utf8_lossy(cmd));
        }
    }

    // Vectors computed independently with `shasum -a 1`:
    //   printf 'abcpw' | shasum -a 1
    //   printf '<inner>sess' | shasum -a 1
    #[test]
    fn hash_vectors() {
        assert_eq!(
            &sha1_hex(b"abcpw"),
            b"b751289c59a81e9b0746c34d652eb281309ab797"
        );
        assert_eq!(&sha1_hex(b""), b"da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            &auth_hash(b"abc", b"pw", b"sess"),
            b"e8b63857fced0c6af44aaff8b9d3b33c6f81c80e"
        );
        assert_eq!(
            &auth_hash(NDUID, b"secret", b"sess123456"),
            b"3d43c4aba217ee365d9bdd4af58f0a3a83d16797"
        );
        // B14: more than 4095 bytes of nduid ‖ password are hashed in full.
        let long = vec![b'p'; 5000];
        assert_eq!(
            &auth_hash(b"n1", &long, b"s1"),
            b"083ddf9f63c7e31fe4b80d9ea141aaf9b825cf76"
        );
    }

    #[test]
    fn fixed_commands() {
        let d = dev(b"sess123456");
        assert_eq!(
            build(b"list", &d, None, no_prompt).unwrap(),
            b"list host://"
        );
        assert_eq!(
            build(b"listx", &dev(b""), None, no_prompt).unwrap(),
            b"list host://"
        );
        let want = [&b"logout dev://"[..], NDUID, b"\n"].concat();
        assert_eq!(build(b"logout", &d, None, no_prompt).unwrap(), want);
        assert_eq!(
            build(b"logoutnow", &dev(b""), None, no_prompt).unwrap(),
            want
        );
    }

    #[test]
    fn auth_commands() {
        let d = dev(b"sess123456");
        let hash = auth_hash(NDUID, b"pw", b"sess123456");
        for (cmd, name) in [
            (&b"login"[..], &b"login"[..]),
            (b"loginxyz", b"login"),
            (b"add", b"add"),
            (b"addme", b"add"),
            (b"remove", b"remove"),
            (b"removeall", b"remove"),
        ] {
            let want = [name, b" dev://", NDUID, b" ", &hash].concat();
            assert_eq!(
                build(cmd, &d, Some(b"pw".to_vec()), no_prompt).unwrap(),
                want
            );
            assert_eq!(build(cmd, &d, None, || b"pw".to_vec()).unwrap(), want);
        }
    }

    #[test]
    fn auth_errors() {
        let err = |r: Result<Vec<u8>, Vec<u8>>| String::from_utf8(r.unwrap_err()).unwrap();
        let d = dev(b"sess123456");
        let no_session = dev(b"");
        assert_eq!(
            err(build(b"login", &d, Some(Vec::new()), no_prompt)),
            "Please specify password\n"
        );
        assert_eq!(
            err(build(b"add", &d, None, Vec::new)),
            "Please specify password\n"
        );
        // A password read from stdin is a C string: a leading NUL empties it.
        assert_eq!(
            err(build(b"add", &d, None, || b"\0abc".to_vec())),
            "Please specify password\n"
        );
        for pw in [None, Some(b"pw".to_vec()), Some(Vec::new())] {
            assert_eq!(
                err(build(b"remove", &no_session, pw, no_prompt)),
                "Device does not require authentication\n"
            );
        }
        assert_eq!(
            err(build(b"", &d, None, no_prompt)),
            "unsupported command()\n"
        );
        assert_eq!(
            err(build(b"bogus cmd", &d, None, no_prompt)),
            "unsupported command(bogus cmd)\n"
        );
    }

    #[test]
    fn nul_in_prompted_password_truncates() {
        let d = dev(b"s1");
        let want = auth_hash(NDUID, b"ab", b"s1");
        let got = build(b"login", &d, None, || b"ab\0cd".to_vec()).unwrap();
        assert!(got.ends_with(&want));
    }

    fn scan(input: &[u8]) -> (Vec<u8>, usize) {
        let mut r = input;
        let pw = scan_password(&mut r);
        (pw, r.len())
    }

    #[test]
    fn password_scanner() {
        assert_eq!(scan(b"secret\n"), (b"secret".to_vec(), 0));
        assert_eq!(scan(b"secret\nmore"), (b"secret".to_vec(), 4));
        assert_eq!(
            scan(b" \t\r\n\x0b\x0csecret more\n"),
            (b"secret".to_vec(), 5)
        );
        assert_eq!(scan(b""), (Vec::new(), 0));
        assert_eq!(scan(b"  \n\n"), (Vec::new(), 0));
        assert_eq!(scan(b"noeol"), (b"noeol".to_vec(), 0));
        assert_eq!(scan(b"a\0b c"), (b"a\0b".to_vec(), 1));
        assert_eq!(scan(b"\xff\x80x y"), (b"\xff\x80x".to_vec(), 1));
        let seventy = vec![b'q'; 70];
        assert_eq!(scan(&seventy), (vec![b'q'; 64], 6));
        let mut sixty_four = vec![b'z'; 64];
        sixty_four.push(b'\n');
        assert_eq!(scan(&sixty_four), (vec![b'z'; 64], 1));
    }

    /// A reader that fails with EINTR once, then yields its data.
    struct Interrupting<'a>(bool, &'a [u8]);
    impl Read for Interrupting<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.0 {
                self.0 = true;
                return Err(ErrorKind::Interrupted.into());
            }
            self.1.read(buf)
        }
    }

    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(ErrorKind::Other.into())
        }
    }

    #[test]
    fn password_scanner_read_errors() {
        assert_eq!(scan_password(&mut Interrupting(false, b"pw\n")), b"pw");
        assert!(scan_password(&mut Broken).is_empty());
    }
}
