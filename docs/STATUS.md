# rovacom: Compatibility Matrix and Verification Report

Last updated: 2026-10-30. Slice 1 (device listing) complete.

## Validation runs

| | macOS | Linux |
|---|---|---|
| Host | MacBook Air, macOS 27.2 (26B123), arm64 | a010, Ubuntu 26.10, kernel 7.3.0, x86_64, glibc 2.44 |
| Rust | rustc 1.99.1 (58cf8f9c3 2026-10-13) stable | rustc 1.99.1 (58cf8f9c3 2026-10-13) stable |
| C reference | `scripts/build-reference.sh` (Apple clang 21.0.0), commit d9d8b77 | same script (gcc 15.3.0) |

Commands (run in `rust/`): `cargo fmt --check`, `cargo check --all-targets --locked`,
`cargo clippy --all-targets --locked -- -D warnings`, `cargo test --all-targets --locked`.

## Verification report

| Item | macOS | Linux |
|---|---|---|
| fmt / check / clippy `-D warnings` | executed, passed | executed, passed |
| Unit tests | executed, 37 passed | executed, 36 passed (1 test is macOS-only) |
| Differential tests vs C (mock novacomd) | executed, 20 passed | executed, 20 passed |
| Harness negative check (`ROVACOM_REFERENCE=/bin/echo`) | executed, 18 of 20 fail as expected (2 are Rust-only cases) | not executed |
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
| Write to closed stdout pipe | **differs, open**: C dies by SIGPIPE; Rust ignores SIGPIPE and exits normally. Fix in the `sys` slice (needs `libc`) | same |
| Remote command execution (`run …`, data stream) | unsupported in this build (exit 70) | same |
| `-c` control commands and auth | unsupported in this build (exit 70) | same |
| `-t` / `-s` terminal and signals | unsupported in this build | same |
| `-P` port forwarding | unsupported in this build (exit 70) | same |

## Remaining work before full replacement

1. Slice 2: packet codec + non-tty data stream (`run`, binary stdin/stdout, EOF, RETURN code, malformed packets).
2. Slice 3: `-c` control commands and SHA-1 auth.
3. Slice 4: `sys` module (`libc`): termios `-t`, signal forwarding `-s`, SIGWINCH resize, SIGPIPE default.
4. Slice 5: `-P` / `-f` port forwarding relay.
5. Run CI on real runners; Linux arm64 and macOS x86_64 coverage.
6. Interop with `rovacomd` and, with approval, read-only tests against the Pre3.
7. Phase 5 build and deployment docs, including `novacom` alias setup without system-wide install.
