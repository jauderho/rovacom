# rovacom: Improvement Log

Ideas for stability, performance and security found while porting novacom (C, 2008-2013).
The port itself preserves behavior; items here are **not applied** unless marked otherwise.
Each item lists whether it changes wire behavior (needs `novacomd`/`rovacomd`/device agreement) or is host-local only.

Status: `open` · `applied-in-port` (fixed as part of the port, see ASSESSMENT §8) · `done` (applied later)

| ID | Area | Finding (C source) | Proposal | Wire impact | Status |
|---|---|---|---|---|---|
| I1 | Security | Password via `-r` is visible in `ps` and shell history (`main.c:259`) | Read from env var or prompt / stdin pipe; deprecate `-r` | None | open |
| I2 | Security | Auth is `SHA1(SHA1(nduid‖pw)‖session)`, unsalted SHA-1 over plaintext TCP (`main.c:prepare_cmd`) | Device-side protocol; document weakness. Only fixable together with device firmware | Yes | open |
| I3 | Security | Port forwarding listens on `INADDR_ANY` (`socket.c:create_listen_socket`) — exposes device services to the LAN | Bind `127.0.0.1` by default; opt-in flag for all interfaces | None | done (127.0.0.1 only; no opt-in flag) |
| I4 | Security | Server traffic is plaintext TCP; `-a` to a remote host sends password hashes and streams unencrypted | Recommend SSH tunnel; consider refusing non-loopback `-a` without a flag | None | open |
| I5 | Security | Uninitialized OOB padding sent on wire (B5) | Zero padding | Bytes only in reserved field | applied-in-port |
| I6 | Security | Global buffer overflow building command from argv (B1) | Build the command without a fixed buffer (no length limit) | None | applied-in-port |
| I7 | Stability | macOS `-t` sets VINTR/VQUIT/VSUSP to `'\0'`, which is not `_POSIX_VDISABLE` (255) on macOS: Ctrl-@ interrupts locally | Use `_POSIX_VDISABLE` | None | open |
| I8 | Stability | Death by signal (no `-s`) leaves tty raw: restore only via `atexit` | Restore termios in a guard + on fatal signals | None | open |
| I9 | Stability | False "socket closed" on `EAGAIN` between header and body (B2) | Treat `EAGAIN` as no data | None | applied-in-port |
| I10 | Stability | Signals silently dropped when `errno` is not stale `EINTR` (B4); the C handler also clobbers `errno` | Atomic mask + self-pipe; handler saves `errno` | None | applied-in-port |
| I11 | Stability | Zero-length DATA/OOB packets are treated as disconnect (B3) | Accept zero-length packets | None (receiver only) | open |
| I12 | Stability | Remote non-zero exit prints misleading `novacom: unexpected EOF from server` | Print only on real EOF | None (stderr text) | open |
| I13 | Stability | `-c list` never reads the host list it requested (B12) | Print response body | None | open |
| I14 | Stability | No connect / read timeouts anywhere; a stuck daemon hangs the client forever | Configurable timeouts | None | open |
| I15 | Stability | IPv4 only via deprecated `gethostbyname` (`socket.c:86`) | `getaddrinfo`, try all addresses, IPv6 | None | open |
| I16 | Stability | Relay crash on odd `-f` list (B8); relay leaks listening sockets and never exits; tunnel threads close sockets the other thread still uses (B16); 100% CPU spin when the device closes mid-packet (B15) | Parse errors; shared sockets with `shutdown`; end tunnel on EOF; clean shutdown on SIGINT | None | applied-in-port (B8, B15, B16) / open (clean shutdown) |
| I17 | Performance | Device list read 1 byte per `recv` syscall (`read_line`) | Buffered reader (must not over-read into the next protocol phase on the same socket) | None | open |
| I18 | Performance | `select` with a 5 ms timeout busy-polls while stdin is open, and a zero timeout spins at 100% CPU when stdin is not watched (`data_xfer`) | Block without timeout. rovacom already blocks when stdin is not watched; the 5 ms poll remains | None | partly applied-in-port |
| I19 | Performance | stdin read in 4 KiB chunks, one packet per `send`; relay sends header and body as 2 syscalls | Larger chunks (peer max permitting), `writev` | Packet sizes only | open |
| I20 | Performance | Relay uses 2 OS threads per tunnel + 1 per port | Fine at this scale; revisit only if needed | None | open |
| I21 | Maintenance | Host-endian wire format with no explicit byte order | rovacom encodes explicit little-endian (same bytes on all targets) | None | applied-in-port |
| I22 | Maintenance | Dead code: `base64.c`, `hexdump8`, `adler32/crc32` decls, `init_sockets` | Not ported | None | applied-in-port |
| I23 | Stability | Oversized OOB packet arriving in parts overflows a static buffer (B13) | Discard extra bytes | None | applied-in-port |
| I24 | Stability | rovacom resets SIGPIPE to default at startup; C inherits an ignored SIGPIPE from its parent | Inherit the parent disposition (needs a pre-main hook) if any caller depends on it | None | open |
| I25 | Security | C zeroes the `-r` password in argv after hashing, so it disappears from `ps` once used; rovacom zeroes only its own copies, so the password stays visible in `ps` for the life of the process | Scrub argv (platform-specific) or prefer I1 (no password on the command line) | None | open (regression vs C) |
| I26 | Security | Password echo is disabled only if both stdin and stdout are terminals (`setconsole` gives up if `tcgetattr(1)` fails): `rovacom -c login \| tee log` echoes the password | Disable echo on stdin (or `/dev/tty`) independently | None | open |
| I27 | Stability | B6 leaves the tty with only ECHO set (no ICANON/ISIG) between the prompt and exit; Ctrl-C does not work while waiting for the daemon | Restore the saved attributes instead of `&= ECHO` | None | open |
| I28 | Security | SHA-1 auth hash computed over an unbounded `snprintf` buffer (B14) | Hash the full input | None | applied-in-port |
| I29 | Maintenance | The port 6971 test guard intermittently refuses to run on macOS ("port 6971 accepts connections", about 3 in 20 runs during slice 5) with nothing found listening afterwards; cause unknown. Overlapping `cargo test` runs also trigger it (the lock is per process) | Find the cause; cross-process lock file (`flock`) | None (tests only; fails safe) | open |
| I30 | Stability | Relay accept errors (e.g. EMFILE) make the service thread retry at once in a tight loop | Back off on accept errors | None | done (100 ms wait) |
