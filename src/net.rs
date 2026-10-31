//! TCP helpers: emulation of C `connect_socket` (`socket.c`).

use std::net::{TcpStream, ToSocketAddrs};

/// Emulates C `connect_socket(address, port)`: resolves `host`, takes the
/// first IPv4 address, and does a blocking connect without timeout.
/// `port` is truncated to 16 bits, as `htons(port)` does in C.
///
/// Returns `None` on any failure (non-UTF-8 host, resolution error, no
/// IPv4 address, connect error), where C returns a negative value.
pub fn connect_socket(host: &[u8], port: i32) -> Option<TcpStream> {
    let host = std::str::from_utf8(host).ok()?;
    let addr = (host, port as u16)
        .to_socket_addrs()
        .ok()?
        .find(std::net::SocketAddr::is_ipv4)?;
    TcpStream::connect(addr).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn connects_with_truncated_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = i32::from(listener.local_addr().unwrap().port());
        let t = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
        });
        assert!(connect_socket(b"127.0.0.1", port + 65536).is_some());
        t.join().unwrap();
    }

    #[test]
    fn rejects_bad_hosts() {
        assert!(connect_socket(b"\xff\xfe", 1).is_none());
        assert!(connect_socket(b"::1", 1).is_none());
    }
}
