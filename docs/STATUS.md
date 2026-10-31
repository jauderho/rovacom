# rovacom: Compatibility Matrix and Verification Report

Last updated: 2026-10-30. Slices 1 (device listing), 2 (remote command data stream), 3 (`-c` control commands, password prompt), 4 (`-t` terminal mode, `-s` signal forwarding) and 5 (`-P` port forwarding) complete. All C functionality is ported.

## Validation runs

| | macOS | Linux |
|---|---|---|
| Host | MacBook Air, macOS 27.2 (26B123), arm64 | a010, Ubuntu 26.10, kernel 7.3.0, x86_64, glibc 2.44 |
| Rust | rustc 1.99.1 (58cf8f9c3 2026-10-13) stable | rustc 1.99.1 (58cf8f9c3 2026-10-13) stable |
| Dependencies | libc 0.2.191; sha1 0.11.0 (+ block-buffer, cfg-if, cpufeatures, crypto-common, digest, hybrid-array, typenum); all MIT OR Apache-2.0 | same |
| C reference | `scripts/build-reference.sh` (Apple clang 21.0.0), commit d9d8b77 | same script (gcc 15.3.0) |

Build, install and run instructions: [DEPLOY.md](DEPLOY.md).

Commands (run in `rust/`): `cargo fmt --check`, `cargo check --all-targets --locked`,
`cargo clippy --all-targets --locked -- -D warnings`, `cargo test --all-targets --locked`.

## Verification report

| Item | macOS | Linux |
|---|---|---|
| fmt / check / clippy `-D warnings` | executed, passed | executed, passed |
| Full suite repeated | 4 full runs after slice 5, all passed | 6 full runs after slice 5; 1 failure (C reference flake, below), then 3 passed after the fix |
| Unit tests | executed, 78 passed | executed, 77 passed (1 test is macOS-only) |
| Differential tests vs C (mock novacomd, merged-stream, pty, signal and relay tests) | executed, 64 passed | executed, 64 passed |
| Known flaky reference | `signals_forwarded`: C drops signals under load (B4): 8 of 20 runs on a loaded 2-core host, rovacom 0 of 20. The test now retries the C side up to 5 times; rovacom runs once | same |
| Port 6971 guard false refusals (I29) | about 3 in 20 runs during slice 5; fails safe; cause unknown | not seen |
| Harness negative checks | slice 1: wrong reference fails 18 of 20; slices 2-5: 13 deliberate code mutations each caught; slice 3: tests refuse to run with a fake wildcard listener on 6971 | not executed |
| CI workflow (`.github/workflows/rust.yml`) | proposed, **never run** | proposed, **never run** |
| Linux arm64, macOS x86_64 | not executed (CI jobs proposed) | not executed |
| Real novacomd (upstream 530a103, built for macOS with `scripts/patches/novacomd-macos.patch`, run as the user, no device) | executed 2026-10-31: `-l` (empty list, exit 0) and device lookup (`unable to find device`, exit 1) match the C client | not executed |
| rovacomd | blocked: not written yet | blocked |
| Real device (Palm Pre3) | blocked: plugged in, but never seen on the Mac's USB bus (3 cable/port attempts) | n/a |

## Compatibility matrix

"Verified" means byte-identical exit status, stdout and stderr against the C build on the same OS, using a mock device-list server. Mock tests do not prove real-device compatibility.

| Behavior | macOS | Linux |
|---|---|---|
| Option parsing (permutation, clusters, long options, abbreviations, `--`, `POSIXLY_CORRECT`) | preserved, verified | preserved, verified |
| getopt diagnostics (per-libc text; macOS `-l-list` cluster quirk; macOS `illegal option` under `POSIXLY_CORRECT`) | preserved, verified | preserved, verified |
| Usage text, `-V` (exit 1) | preserved, verified (version string differs by design) | same |
| Invoked as `novacom` (symlink alias) | preserved, verified | preserved, verified |
| `-a` resolution (first IPv4, `127.1`, `""`, `::1` rejected) | preserved, verified | preserved, verified |
| `-a` with non-UTF-8 bytes | implemented, unverified (treated as lookup failure) | same |
| `-p` atoi semantics, 16-bit wrap | preserved, verified | preserved, verified |
| Device list parsing and `-l` output, `-d` filter, NUL/CR/long lines | preserved, verified | preserved, verified |
| `find_device`, `unable to find device`, negative port | preserved, verified | preserved, verified |
| `-w` wait-and-retry | preserved, verified | preserved, verified |
| `-f` parsing | preserved, verified | preserved, verified |
| `-f` with odd / empty token list | **intentionally changed** (B8): error + usage instead of segfault | same |
| Write to closed stdout pipe (incl. `-l`) | preserved, verified (killed by signal 13) | same |
| SIGPIPE inherited as ignored from parent | **differs**: rovacom always resets to default (I24) | same |
| Remote command: command line, `ok` / `req:auth` / `unrecognized command` / other responses | preserved, verified | same |
| Command longer than 4095 bytes | **intentionally changed** (B1): sent in full; C overflows | same |
| Packet framing: stdin upload, stdout/stderr download, binary and NUL bytes, 10 MiB each way | preserved, verified | same |
| RETURN codes (0, 1, 255, 256, -1) and `novacom: unexpected EOF from server` | preserved, verified | same |
| Malformed peer data: bad magic/version, unknown type, zero-size DATA (B3 kept), OOB size 0/8/24, unknown OOB, peer SIGNAL/RESIZE, disconnect mid-header/body | preserved, verified | same |
| Header and body in separate TCP segments | **intentionally changed** (B2): C fails with "closed prematurely" (observed 3/3 on macOS); rovacom waits | same (C failure not checked on Linux) |
| OOB padding bytes | **intentionally changed** (B5): zero; C sends heap garbage | same |
| Short writes to stdout/stderr | **intentionally changed** (B7): full writes | same |
| Oversized OOB arriving in parts | **intentionally changed** (B13): extra bytes discarded; C has UB | same |
| `unable to send command to server` path | implemented, unverified (no deterministic trigger) | same |
| Started with fd 0/1/2 closed | **differs**: Rust runtime reopens them as `/dev/null` | same |
| `-c` list / login / logout / add / remove, prefix matching, `unsupported command(...)` | preserved, verified | same |
| Auth hash (double SHA-1) with `-r` or prompted password | preserved, verified | same |
| Password prompt on a pty: no echo, terminal restored | preserved, verified (PENDIN left set, as C) | preserved, verified |
| stdout/stderr byte order when merged into one pipe | preserved, verified | same |
| `-w` session-id carry-over | preserved, verified | same |
| Password > 4055 bytes | **intentionally changed** (B14): hashed in full; C UB | same |
| `-r` password visible in `ps` after use | **differs** (I25): C scrubs argv; rovacom does not | same |
| `-t` terminal attributes while running and restored after exit | preserved, verified (PENDIN left set, as C) | preserved, verified |
| `-t` typed control bytes (0x03, 0x1c, 0x1a, 0x7f) passed through as data | preserved, verified | same |
| `-t` NUL key | preserved, verified: kills with SIGINT (P2) | preserved, verified: passed as data |
| Initial RESIZE (pty size, or 0x0 when stdout is not a tty); RESIZE on SIGWINCH | preserved, verified | same |
| `-s` forwards SIGINT, SIGHUP, SIGQUIT, SIGTERM; process survives | preserved, verified | same |
| Signals arriving while blocked on a socket write | **intentionally changed** (B4): all forwarded once, lowest first, duplicates merged; C drops them | same |
| `-c` with `-t` / `-s` | preserved, verified | same |
| `failed to establish pipe ` path | implemented, unverified (needs fd exhaustion) | same |
| `-P` relay output (`Forwarding Port`, `created service fd`, thread lines, `connect tcp-port://N` + blank line), stdout buffering | preserved, verified (thread line order is nondeterministic in both; compared as multiset + per-thread order) | same |
| Relay data both ways (DATA chunks ≤ 0x2000), ERR dropped, OOB ends tunnel, bad packet | preserved, verified | same |
| Relay error lines `src/tcprelay.c:<line>: <strerror>` (247, 333, ...) | preserved, verified (libc text per OS) | same |
| Relay daemon refusals (`req:auth`, garbage, no reply) | preserved, verified; **intentionally changed** (B9): empty text after `got response ` | same |
| Device closes mid-packet | **intentionally changed** (B15): tunnel ends; C spins at ~100% CPU | same |
| Closing one side of a tunnel | **intentionally changed** (B16): `shutdown` on shared sockets; output matches C (EBADF lines emulated) | **intentionally changed** (B16): peer sees FIN at once; C waits for the peer to act |
| Relay listens on all interfaces (INADDR_ANY) | preserved (security note I3) | same |

## Remaining work before full replacement

1. Run CI on real runners; Linux arm64 and macOS x86_64 coverage.
2. Interop with `rovacomd` and, with approval, read-only tests against the Pre3.
