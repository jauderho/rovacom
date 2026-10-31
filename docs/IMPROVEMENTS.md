# rovacom: Improvement Log

Ideas for stability, performance and security found while porting novacom (C, 2008-2013).
The port itself preserves behavior; items here are **not applied** unless marked otherwise.
Each item lists whether it changes wire behavior (needs `novacomd`/`rovacomd`/device agreement) or is host-local only.

Status: `open` · `applied-in-port` (fixed as part of the port, see ASSESSMENT §8) · `done` (applied later)

| ID | Area | Finding (C source) | Proposal | Wire impact | Status |
|---|---|---|---|---|---|
| I1 | Security | Password via `-r` is visible in `ps` and shell history (`main.c:259`) | Read from env var or prompt / stdin pipe; deprecate `-r` | None | open |
| I2 | Security | Auth is `SHA1(SHA1(nduid‖pw)‖session)`, unsalted SHA-1 over plaintext TCP (`main.c:prepare_cmd`) | Device-side protocol; document weakness. Only fixable together with device firmware | Yes | open |
| I3 | Security | Port forwarding listens on `INADDR_ANY` (`socket.c:create_listen_socket`) — exposes device services to the LAN | Bind `127.0.0.1` by default; opt-in flag for all interfaces | None | open |
| I4 | Security | Server traffic is plaintext TCP; `-a` to a remote host sends password hashes and streams unencrypted | Recommend SSH tunnel; consider refusing non-loopback `-a` without a flag | None | open |
| I5 | Security | Uninitialized OOB padding sent on wire (B5) | Zero padding | Bytes only in reserved field | applied-in-port |
| I6 | Security | Global buffer overflow building command from argv (B1) | Build the command without a fixed buffer (no length limit) | None | applied-in-port |
| I7 | Stability | macOS `-t` sets VINTR/VQUIT/VSUSP to `'\0'`, which is not `_POSIX_VDISABLE` (255) on macOS: Ctrl-@ interrupts locally | Use `_POSIX_VDISABLE` | None | open |
| I8 | Stability | Death by signal (no `-s`) leaves tty raw: restore only via `atexit` | Restore termios in a guard + on fatal signals | None | open |
| I9 | Stability | False "socket closed" on `EAGAIN` between header and body (B2) | Treat `EAGAIN` as no data | None | applied-in-port |
| I10 | Stability | Signals silently dropped when `errno` is not stale `EINTR` (B4) | Reliable self-pipe | None | applied-in-port |
| I11 | Stability | Zero-length DATA/OOB packets are treated as disconnect (B3) | Accept zero-length packets | None (receiver only) | open |
| I12 | Stability | Remote non-zero exit prints misleading `novacom: unexpected EOF from server` | Print only on real EOF | None (stderr text) | open |
| I13 | Stability | `-c list` never reads the host list it requested (B12) | Print response body | None | open |
| I14 | Stability | No connect / read timeouts anywhere; a stuck daemon hangs the client forever | Configurable timeouts | None | open |
| I15 | Stability | IPv4 only via deprecated `gethostbyname` (`socket.c:86`) | `getaddrinfo`, try all addresses, IPv6 | None | open |
| I16 | Stability | Relay crash on odd `-f` list (B8); relay leaks listening sockets and never exits | Parse errors; clean shutdown on SIGINT | None | applied-in-port (B8) / open |
| I17 | Performance | Device list read 1 byte per `recv` syscall (`read_line`) | Buffered reader (must not over-read into the next protocol phase on the same socket) | None | open |
| I18 | Performance | `select` with a 5 ms timeout busy-polls while stdin is open, and a zero timeout spins at 100% CPU when stdin is not watched (`data_xfer`) | Block without timeout. rovacom already blocks when stdin is not watched; the 5 ms poll remains | None | partly applied-in-port |
| I19 | Performance | stdin read in 4 KiB chunks, one packet per `send`; relay sends header and body as 2 syscalls | Larger chunks (peer max permitting), `writev` | Packet sizes only | open |
| I20 | Performance | Relay uses 2 OS threads per tunnel + 1 per port | Fine at this scale; revisit only if needed | None | open |
| I21 | Maintenance | Host-endian wire format with no explicit byte order | rovacom encodes explicit little-endian (same bytes on all targets) | None | applied-in-port |
| I22 | Maintenance | Dead code: `base64.c`, `hexdump8`, `adler32/crc32` decls, `init_sockets` | Not ported | None | applied-in-port |
| I23 | Stability | Oversized OOB packet arriving in parts overflows a static buffer (B13) | Discard extra bytes | None | applied-in-port |
| I24 | Stability | rovacom resets SIGPIPE to default at startup; C inherits an ignored SIGPIPE from its parent | Inherit the parent disposition (needs a pre-main hook) if any caller depends on it | None | open |
