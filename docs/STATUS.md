# rovacom: Compatibility Matrix and Verification Report

Last updated: 2026-10-30. Slices 1 (device listing), 2 (remote command data stream, no tty) and 3 (`-c` control commands, password prompt) complete.

## Validation runs

| | macOS | Linux |
|---|---|---|
| Host | MacBook Air, macOS 27.2 (26B123), arm64 | a010, Ubuntu 26.10, kernel 7.3.0, x86_64, glibc 2.44 |
| Rust | rustc 1.99.1 (58cf8f9c3 2026-10-13) stable | rustc 1.99.1 (58cf8f9c3 2026-10-13) stable |
| Dependencies | libc 0.2.191; sha1 0.11.0 (+ block-buffer, cfg-if, cpufeatures, crypto-common, digest, hybrid-array, typenum); all MIT OR Apache-2.0 | same |
| C reference | `scripts/build-reference.sh` (Apple clang 21.0.0), commit d9d8b77 | same script (gcc 15.3.0) |

Commands (run in `rust/`): `cargo fmt --check`, `cargo check --all-targets --locked`,
`cargo clippy --all-targets --locked -- -D warnings`, `cargo test --all-targets --locked`.

## Verification report

| Item | macOS | Linux |
|---|---|---|
| fmt / check / clippy `-D warnings` | executed, passed | executed, passed |
| Unit tests | executed, 72 passed (2 full runs) | executed, 71 passed (2 full runs; 1 test is macOS-only) |
| Differential tests vs C (mock novacomd, merged-stream and pty tests) | executed, 50 passed (2 full runs) | executed, 50 passed (2 full runs) |
| Harness negative checks | slice 1: wrong reference fails 18 of 20; slices 2-3: 6 deliberate code mutations each caught; slice 3: tests refuse to run with a fake wildcard listener on 6971 | not executed |
| CI workflow (`.github/workflows/rust.yml`) | proposed, **never run** | proposed, **never run** |
| Linux arm64, macOS x86_64 | not executed (CI jobs proposed) | not executed |
| Real novacomd / rovacomd | blocked: none available | blocked: none available |
| Real device (Palm Pre3) | blocked: not yet connected | n/a |

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
| `-c` with `-t` or `-s` | unsupported in this build (exit 70) | same |
| `-t` / `-s` terminal and signals | unsupported in this build (exit 70) | same |
| `-P` port forwarding | unsupported in this build (exit 70) | same |

## Remaining work before full replacement

1. Slice 4: termios `-t`, signal forwarding `-s`, SIGWINCH resize (extends `sys`).
2. Slice 5: `-P` / `-f` port forwarding relay.
3. Run CI on real runners; Linux arm64 and macOS x86_64 coverage.
4. Interop with `rovacomd` and, with approval, read-only tests against the Pre3.
5. Phase 5 build and deployment docs, including `novacom` alias setup without system-wide install.
