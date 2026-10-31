# rovacom: Build, Install and Run

rovacom is a Rust port of the `novacom` client. It talks TCP to a novacom daemon (`novacomd`, or its Rust port `rovacomd`); the daemon owns the USB link to the device. rovacom has no USB code and needs no drivers.

Everything below runs as your user. Nothing is installed system-wide and nothing needs `sudo`.

Verified on 2026-10-31 with rustc 1.99.1 on macOS 27.2 (arm64) and Ubuntu 26.10 (x86_64). Other OS versions and CPU types are untested; see [STATUS.md](STATUS.md).

## 1. What you need

| | Linux | macOS |
|---|---|---|
| Rust | stable, 1.85 or newer (edition 2024), via [rustup](https://rustup.rs) in your home directory | same |
| C compiler, `make`, `git` | only to build the C reference for the tests (`gcc`, `make`, `git` packages) | only for the tests: Xcode Command Line Tools (`xcode-select --install`) |
| Daemon | a running `novacomd` or `rovacomd` on the same host (or reachable over TCP) | same |
| Native libraries | none beyond the C library (glibc) | none beyond the system (`libSystem`) |

Crate dependencies (fetched by Cargo, all MIT OR Apache-2.0): `libc`, `sha1` and the crates `sha1` uses. No async runtime, no USB library.

If `cargo fmt` or `cargo clippy` say they are "not installed for the toolchain", add them:

```bash
rustup component add rustfmt clippy
```

## 2. Build

From the repository root:

```bash
cargo build --release --locked --manifest-path rust/Cargo.toml
```

The binary is `rust/target/release/rovacom` (about 0.4 MB on macOS, 0.6 MB on Linux). To set the version string it prints for `-V`, build with `BUILDVERSION=<text>` in the environment; the default is `rovacom-<crate version>`.

## 3. Test

The differential tests compare rovacom with the original C client, so build that once first. The script clones upstream novacom at the pinned commit into `reference/` (gitignored) and builds it:

```bash
scripts/build-reference.sh
```

Then run the same checks as CI:

```bash
cd rust && cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --all-targets --locked
```

The tests use only mock daemons on 127.0.0.1. The suite takes about 35 seconds.

- Do not run two test runs at the same time on one machine (see Troubleshooting).
- The control-command tests need port 6971 free. If a real novacomd is running (for example with a device attached), they **refuse to run** rather than risk talking to it. Stop the daemon before running the tests.

## 4. Run without installing

```bash
rust/target/release/rovacom -l
```

## 5. Install for your user, with the `novacom` name

Existing tools and scripts (the Palm SDK `palm-*` tools, `novaterm`) run `novacom` by name. rovacom behaves the same under either name, so a symlink is all that is needed.

**Linux** (`~/.local/bin` is on `PATH` by default on Ubuntu):

```bash
cargo install --path rust --locked --root ~/.local
```

```bash
ln -sf rovacom ~/.local/bin/novacom
```

**macOS** (`~/.local/bin` is not on `PATH` by default; add it to your shell profile, for example `~/.zshrc`):

```bash
cargo install --path rust --locked --root ~/.local
```

```bash
ln -sf rovacom ~/.local/bin/novacom
```

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
```

Check which `novacom` your shell finds. If an older `novacom` (for example from the Palm SDK) comes first on `PATH`, either put `~/.local/bin` before it or remove that copy yourself. Tools that call `novacom` by an absolute path keep using that path.

```bash
command -v novacom && novacom -V
```

`novaterm` from the original repository is a one-line wrapper. The rovacom equivalent:

```bash
printf '#!/bin/sh\nexec novacom "$@" -t open tty://0\n' > ~/.local/bin/novaterm && chmod +x ~/.local/bin/novaterm
```

**Uninstall:**

```bash
rm -f ~/.local/bin/rovacom ~/.local/bin/novacom ~/.local/bin/novaterm
```

macOS: a binary you build yourself is not quarantined, so it runs without signing or notarization. Distributing binaries to other Macs would need Developer ID signing and notarization; that is not set up.

## 6. Connecting to the daemon

There are no configuration files or environment variables. Everything is on the command line and matches the C client:

| Item | Default | Option |
|---|---|---|
| Daemon host | `localhost` (first IPv4 address only; no IPv6) | `-a host` |
| Device-list port | 6968 | `-p port` |
| Control port (`-c`) | 6971, fixed | none |
| Device port | read from the daemon's device list | `-d` picks the device |

Typical commands:

| Task | Command |
|---|---|
| List devices | `rovacom -l` |
| Run a command | `rovacom -- run file:///bin/uname -a` |
| Interactive shell | `rovacom -t open tty://0` (or `novaterm`) |
| Forward local port 10022 to device port 22 | `rovacom -P -f 10022:22` |
| Pick a device | `-d <nduid>`, `-d usb`, or `-d <device type>` |
| Wait for a device | `-w` |

Put `--` before a remote command that has its own options. rovacom (like the C client) reads options anywhere on the command line, so in `rovacom run file:///bin/uname -a` it takes `-a` as its own option.

The daemon carries all traffic in plain TCP. Using `-a` with a remote host sends commands, data and password hashes unencrypted; use an SSH tunnel instead. Port forwarding (`-P`) listens on **all network interfaces**, as the C client does.

## 7. Behavior that differs by OS (same as the C client)

| | Linux | macOS |
|---|---|---|
| Option error text | `rovacom: invalid option -- 'Z'` (program name as typed) | `rovacom: invalid option -- Z` (name of the executable) |
| `-t` and the NUL key (Ctrl-@ or Ctrl-Space) | sent to the device | **kills rovacom** (SIGINT), as the C client; avoid Ctrl-@ in `-t` sessions |
| Relay error text (`src/tcprelay.c:<line>: ...`) | glibc wording, e.g. `Success` | macOS wording, e.g. `Undefined error: 0` |

Intentional differences from the C client (bug fixes) are listed in [STATUS.md](STATUS.md).

## 8. Using it with a real device

Before connecting a device, read the safety notes: the first runs should be read-only, for example `rovacom -l` and `rovacom -- run file:///bin/uname -a`. Do not run `-c login|add|remove|logout`, file transfers or anything that writes to the device until those read-only runs give the expected results.

## 9. Troubleshooting

Each entry below was observed during development.

| Symptom | Cause | Fix |
|---|---|---|
| `failed to connect to server` | No daemon is listening on `-a`/`-p` (default `localhost:6968`), or the host has no IPv4 address | Start `novacomd`/`rovacomd`; check `-a` and `-p` |
| `unable to find device` | The daemon lists no device matching `-d` (or no device at all) | Check `rovacom -l`; use `-w` to wait for the device |
| `option requires an argument -- 'a'` (or similar) when running a remote command | rovacom took an option of the remote command as its own | Put `--` before the remote command |
| Ctrl-C does nothing while waiting after a password prompt | C behavior kept (bug B6) | Wait for the daemon's reply; it is restored at exit |
| Password echoed while typing | Echo is turned off only if stdout is also a terminal (C behavior) | Don't pipe stdout during `-c login`, or use `-r` |
| Password visible in `ps` | `-r` stays in the process arguments (the C client wiped it) | Prefer the prompt to `-r` |
| Tests panic: `C reference binary not found` | The reference is not built | `scripts/build-reference.sh` |
| Tests panic: `port 6971 accepts connections ... refusing to run` | A daemon is running, another test run is in progress, or (rarely, on macOS) a false refusal whose cause is not yet known | Stop the daemon or the other run and retry |
| `scripts/build-reference.sh` fails, or a manual `make BUILDVERSION=x` fails with `use of undeclared identifier` | `BUILDVERSION` given on the `make` command line bypasses the makefile's quoting | Use the script, or pass `BUILDVERSION` in the environment |

## 10. Not covered

- Packaging (`.deb`, Homebrew), background services, and code signing: not set up and not needed for local use.
- Linux arm64 and macOS x86_64: CI jobs are defined but have not run.
- Real daemons and devices: not yet tested (see [STATUS.md](STATUS.md)).
