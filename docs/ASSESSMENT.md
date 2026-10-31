# rovacom: Phase 1 Assessment (Rust port of novacom)

The port is named **rovacom** and builds a binary named `rovacom`. Only the program name changes: wire bytes, ports and command strings stay identical, so `novacomd`, the Rust daemon port `rovacomd` (`~/projects/rovacomd`) and the device see no difference.

`rovacom` must also work unchanged when invoked as `novacom` (for example via a `novacom -> rovacom` symlink), because Palm SDK tools and `novaterm` call `novacom` by name. The device never sees host program names; only the wire protocol matters to it. Program-name-dependent output follows C: usage prints `argv[0]`; getopt diagnostics use `argv[0]` on Linux and the executable basename on macOS.

Status: inspection complete; no Rust code written yet.
Legend: **[V]** verified by reading source or running it; **[H]** hypothesis, not yet verified.

## 1. Source under port

| Item | Value |
|---|---|
| Upstream | https://github.com/openwebos/novacom |
| Commit | `d9d8b772b86231c6490ea8b1ff0e44ddaaf8df05` (2013-06-13, tag `submissions/18`, HEAD of `master`) |
| Size | 6 `.c` + 6 `.h`, ~1.5k code lines (tokei) |
| License | Apache-2.0 (LG Electronics, 2008-2013). `NOTICE`: `sha1.c` (Paul E. Jones, "freeware"), `base64.c` (LibTomCrypt, public domain) |
| Tests | None in the repository [V] |

## 2. Architecture and build-target inventory

### Targets [V]

- **One executable, `novacom`.** `CMakeLists.txt`: `add_executable(${CMAKE_PROJECT_NAME} ${SOURCE_FILES})` over `src/*.c`; `makefile` builds the same 6 objects. No library targets.
- `scripts/novaterm`: 1-line wrapper, `exec novacom $* -t open tty://0`. Not referenced by either build file.
- **`novacomd` is not in this repository.** `novacom` is a pure TCP client of `novacomd`. **USB is owned entirely by `novacomd`** — the port needs no USB library.
- **GLib is not used** despite the README. No `glib` include or `g_*` call in `src/`. Only external link dep: `pthread`.

### Module map (C)

| File | Role | Key symbols |
|---|---|---|
| `src/main.c` | CLI, device list, auth, control commands, interactive data loop, termios, signals | `parse_opt`, `usage`, `list_devices`, `find_device`, `read_line`, `parse_response`, `prepare_cmd`, `getpsw`, `setconsole`, `signal_to_pipe`, `data_xfer`, `main` |
| `src/packet.c`, `packet.h`, `packet_struct.h` | Wire framing encode + incremental decode | `prepare_packet_{data,eof,signal,term_resize}`, `packet_recv_something`, `struct packet_header`, `struct packet_oob_msg` |
| `src/socket.c` | IPv4 TCP helpers | `connect_socket` (`gethostbyname`), `create_listen_socket` (INADDR_ANY, `SO_REUSEADDR`), `accept_socket` |
| `src/tcprelay.c` | `-P/-f` port forwarding, thread-per-direction | `parsePortsToBeForwarded`, `createService`, `serviceThread`, `createTunnel`, `hostToDeviceThread`, `deviceToHostThread`, `sendNovacomPacket` |
| `src/sha1.c`, `cksum.h` | SHA-1 (hex digest string in `Message_Digest_Str`) | `SHA1Reset/Input/Result` |
| `src/base64.c` | **Dead code** — compiled, never called | `base64_encode/decode` |

Also dead: `hexdump8` (`packet.c`), `adler32`/`crc32` (declared in `cksum.h`, never defined), `init_sockets` (no-op).

### Control flow [V] (`main.c:main`)

1. `parse_opt` (getopt_long) → globals.
2. `saveconsole()` → `atexit(resetconsole)`.
3. Connect `serverip:serverport` (default `localhost:6968`, the device-list port). Read lines.
   - `-l`: `list_devices` prints matching lines, exit 0.
   - else `find_device` → device port; with `-w` reconnect and retry every 500 ms.
4. `-P`: `startTcpRelay` — never returns.
5. `-c <cmd>`: build control command, target port **6971** (`NOVACOM_CTRLPORT`).
   Otherwise the command is the remaining argv joined by `" "` + `"\n"`.
6. Connect to device port, optional `-t` raw tty, optional signal self-pipe.
7. `send(command)`, `parse_response` (expects `ok…`).
8. `-c`: exit 0. Else `data_xfer` loop until OOB RETURN or error.

### Concurrency, signals, lifecycle [V]

- Main path: **single thread**, `select()` loop on socket, stdin (5 ms timeout poll), signal pipe. Socket `O_NONBLOCK`.
- Signals (`-s`: INT/HUP/QUIT/TERM; `-t` or `-s`: WINCH) use `SA_RESTART` handler `signal_to_pipe`, which reads a `sigset_t` out of a pipe, adds the signal, and writes it back (self-pipe carrying a bitmask). Handler also calls `ioctl(TIOCGWINSZ)`.
- Relay: detached pthreads (1 accept loop per forwarded port, 2 per tunnel). Main thread sleeps forever. `SIGPIPE` ignored.
- Shutdown: `return` from `main` → `atexit` restores termios. Death by signal skips the restore.
- No async runtime is justified: the C model maps directly onto `poll` + `std::thread`.

### Existing platform code [V]

No `#ifdef` for OS in sources. `makefile` adds `-lpthread -Wl,-rpath,.` on Linux only. All OS differences are implicit (libc behavior); see §4.

## 3. Compatibility contract

### CLI [V]

`getopt_long` optstring `"a:d:f:lPp:tswVr:c:"`. Long options: `--address --device --list --port --terminal --signals --wait --password --cmd` (no long form for `-f -P -V`).

| Opt | Effect | Notes |
|---|---|---|
| `-a host` | server host, default `localhost` | resolved with `gethostbyname`, IPv4 only, first address |
| `-p n` | device-list port, default 6968 | `atoi` — junk → 0; no range check, truncated by `htons` |
| `-d s` | match nduid / connection type / device type, case-insensitive | |
| `-l` | list devices, exit 0 | |
| `-t` | raw tty + SIGWINCH forwarding | |
| `-s` | forward INT/HUP/QUIT/TERM + WINCH | |
| `-w` | wait for device | 500 ms poll, reconnects each time |
| `-r pw` | password | overwritten with zeros in argv after hashing |
| `-c cmd` | prefix match: `list`, `login`, `logout`, `add`, `remove` | `strncmp` prefix: `-c listfoo` == `list` |
| `-P` / `-f l:r,…` | port forwarding | `-f` parsed during option parsing |
| `-V` | print `version: …`, **exit 1** | |

- Argument permutation is **on** on both glibc and macOS libc (verified: `novacom -p 1 run x -V` prints the version on both). Remote commands containing `-x` tokens need `--`.
- Usage (stdout) → exit 1. With no command, usage is shown **only after** a device is found (verified: no-arg run with no server prints `failed to connect to server`, exit 1).
- Unknown option / missing argument: libc diagnostic on stderr, then `?? getopt returned character code 0x3f ??`, then version + usage, exit 1.

### Exit codes and messages [V]

| Situation | stderr | exit |
|---|---|---|
| connect/resolve failure | `failed to connect to server` | 1 |
| no matching device | `unable to find device` | 1 |
| response starts `req:auth` | `password required` | 1 |
| response starts `unrecognized command` | `unknown command` | 1 |
| other non-`ok` response (incl. EOF → empty) | `bad or error response from other side: '<line>'` | 1 |
| remote OOB RETURN `rc` | `novacom: unexpected EOF from server` **if rc ≠ 0** | `rc & 0xff` |
| socket closed mid-stream | `\nnovacomd socket was closed prematurely\n` + the EOF line | 255 |
| bad magic / version | `Bad packet header magic` or `Unsupported packet version`, `\nbad data from other end\n`, EOF line | 255 |

### Device list protocol (port 6968) [V]

Text lines, read 1 byte at a time, max 255 bytes (`read_line`, newline stripped; an empty line or EOF ends the list). Parsed with `sscanf("%d %40s %20s %30s %10s")` → port, nduid, conntype, devtype, optional session id; < 4 fields → line skipped. `-l` reprints the raw line via `puts`.

### Control/command protocol [V]

- Command line sent as-is: `"<argv joined by space>\n"`, e.g. `run file:///bin/ls\n`.
- Response: one line (≤ 2047 bytes), prefix-matched: `ok`, `req:auth`, `unrecognized command`.
- `-c` commands, all to port **6971**:
  - `list` → `"list host://"` (**no newline**). Exit 0 on `ok`, **response body never read**.
  - `logout` → `"logout dev://<nduid>\n"`.
  - `login|add|remove` → `"<cmd> dev://<nduid> <hash>"` (**no newline**), `hash = hex(SHA1(hex(SHA1(nduid ‖ password)) ‖ sessionid))`, lowercase hex. Requires a session id in the device list; else `Device does not require authentication`, exit 1. If no `-r`, prompts `Please enter password: ` on **stdout**, reads `%64s` with echo off.
  - other → `unsupported command(<cmd>)`, exit 1.

### Stream framing [V] (`packet_struct.h`)

```
header  : u32 magic=0xDECAFBAD, u32 version=1, u32 size, u32 type   (16 bytes)
type    : 0=DATA (stdout / stdin), 1=ERR (stderr), 2=OOB
OOB body: u32 message + 16-byte union  (20 bytes)
message : 0=EOF{i32 fileno}, 1=SIGNAL{u32 signo}, 2=RETURN{i32 rc}, 3=RESIZE{u32 rows, u32 cols}
```

- **Byte order is host-native** (no `htonl`). Every proposed target (x86_64, aarch64) is little-endian, so the port will encode **explicit little-endian**, identical on the wire.
- Client → peer: DATA chunks ≤ 4096 from stdin; on stdin EOF: OOB EOF fileno 0 and stop reading stdin; RESIZE at start (if `-t`/`-s`) and per SIGWINCH; SIGNAL per forwarded signal (numbers 1/2/3/15 are identical on Linux and macOS).
- Peer → client: DATA → stdout, ERR → stderr, OOB EOF fileno 1 → `close(stdout)`, other EOF ignored, RETURN → exit. Incoming SIGNAL/RESIZE/unknown OOB → diagnostic, ignored.
- Relay (`tcprelay.c`) uses the same header; sends `"connect tcp-port://<port>\n"` on the device port, then DATA packets each way; any OOB from the device ends the tunnel.

### Relay output [V]

Chatty stdout: `Forwarding Port L<->R`, `created service fd <fd> local: L remote R`, `started service thread`, `connect tcp-port://R` + blank line, thread start/exit lines. Errors via `REPORT_ERROR` → `src/tcprelay.c:<line>: <strerror>`. Listens on **INADDR_ANY**.

### Paths and configuration [V]

No config files, no env vars, no runtime file paths. Only install layout comes from webOS CMake modules (`/usr/local/webos` default per README; not verified — modules not present).

### Depends on external components or hardware

`novacomd` (USB ↔ TCP bridge) and a device for any real interop: device list contents, `ok`/`req:auth` replies, session ids, real RETURN codes, `tty://` semantics, `tcp-port://` forwarding. **None available**; integration is blocked on both OSes. Mock peers will cover framing and CLI.

## 4. Platform differences and portability risks

| # | Area | Linux | macOS | Effect |
|---|---|---|---|---|
| P1 | getopt diagnostic | `./path/novacom: invalid option -- 'Z'` | `novacom: invalid option -- Z` | [V] stderr differs by OS; emulated per OS. Also macOS-only: progname is the executed path's basename (symlinks unresolved), `illegal option` under `POSIXLY_CORRECT`, and `-` inside a cluster starts a long option (`-l-list`) |
| P2 | `_POSIX_VDISABLE` | 0 | 255 | [V] `-t` sets VINTR/VQUIT/VSUSP to `'\0'` with ISIG on: disabled on Linux, but on macOS **NUL (Ctrl-@) becomes the interrupt key** |
| P3 | `sigset_t` size | 128 B | 4 B | [V] internal to signal pipe only; not on the wire |
| P4 | `select` + `SA_RESTART` | not restarted, `EINTR` | [H] likely same | signal path depends on stale `errno == EINTR` (B4) |
| P5 | `gethostbyname` vs Rust `getaddrinfo` | | | [H] address order may differ for multi-A hosts |
| P6 | `SO_REUSEADDR` | | | [H] semantics differ slightly; relay only |
| P7 | strerror text | glibc | Darwin libc | relay error lines differ by OS (already true in C) |

### Suspected source bugs (flagged, not fixed)

| # | Location | Defect |
|---|---|---|
| B1 | `main.c:792-797` | Command built with `strcat` into 4096-byte buffer: argv > 4095 bytes overflows (UB) |
| B2 | `packet.c:121-152` | After the header completes, the body `recv` on the non-blocking socket runs in the same call; `EAGAIN` → treated as **closed socket**. Timing-dependent false disconnect |
| B3 | `packet.c:152,172-217` | DATA/ERR with `size==0` or OOB with `size==0` → `recv(…,0)` returns 0 → **closed socket** |
| B4 | `main.c:664` | Signal pipe processed only if `errno == EINTR` left over from an earlier `select`; otherwise logs an error and **drops the signal** |
| B5 | `packet.c` / `main.c:552` | OOB union padding sent from uninitialized `malloc` memory (info leak, nondeterministic bytes) |
| B6 | `main.c:191` | `CONMODE_ECHO` does `c_lflag &= ECHO` (clears all other flags) after password entry |
| B7 | `main.c:615,619` | `write()` to stdout/stderr ignores short writes |
| B8 | `tcprelay.c:102` | Checks `ports` instead of `port`: odd count in `-f` → `strtoul(NULL)` crash |
| B9 | `tcprelay.c:291` | Prints uninitialized `response` buffer |
| B10 | `main.c:504,517` | `%.*s` given `size_t` (works on LP64 by ABI accident) |
| B11 | `main.c:169` | `memset(&ti, …)` twice; `to` never cleared (overwritten by `tcgetattr`, harmless) |
| B12 | `-c list` | Never reads the host list it requested |
| B13 | `packet.c:172-176` | Oversized OOB packet whose extra bytes arrive in parts: `sizeof(m) - bodycounter` wraps, and `recv` writes past the static `m` (UB). Found in slice 2; rovacom discards those bytes |
| B14 | `main.c:493-501` | `snprintf` of nduid ‖ password into the 4096-byte `command` buffer, then SHA-1 over the untruncated length: inputs over 4095 bytes read past the buffer (UB). Found in slice 3; rovacom hashes the full input |
| B15 | `tcprelay.c:188-206` | Device closes in the middle of a packet body: `readComplete` returns 0, `send(..., 0)` succeeds, `totalRead` never advances: 100% CPU forever, local client never closed. Found in slice 5; rovacom ends the tunnel |
| B16 | `tcprelay.c:209,258` | Each tunnel thread `close`s a socket the other thread may be blocked in `recv` on; the fd number can then be reused by a new `accept`. macOS: `close` wakes the other `recv` with EBADF. Linux: the other thread stays blocked until its peer acts. rovacom uses shared sockets and `shutdown`, emulating the macOS EBADF messages on macOS |

## 5. Proposed Rust module map

Single Cargo package at the repository root, crate `rovacom` (binary `rovacom` + internal lib for tests). Edition 2024, std threads, no async.

| Rust module | Ports | Shared / platform |
|---|---|---|
| `cli.rs` | `parse_opt`, `usage`, getopt_long permutation + diagnostics | shared; diagnostic format chosen by `cfg(target_os)` (P1) |
| `devlist.rs` | `read_line`, sscanf-equivalent parse, `list_devices`, `find_device` | shared, pure over `Read` |
| `control.rs` | `parse_response`, `prepare_cmd`, `-c` command strings, auth hash | shared |
| `packet.rs` | encode + incremental decoder state machine (no `static` globals) | shared, pure |
| `net.rs` | `connect_socket` (first IPv4), listen socket | shared (std) |
| `session.rs` | `data_xfer` loop | shared logic over `sys::poll` |
| `relay.rs` | `tcprelay.c` with `std::thread` | shared |
| `sys/unix.rs` | termios save/restore/raw, `TIOCGWINSZ`, `sigaction` self-pipe, `poll`, `O_NONBLOCK` | unix; **only** P2 constant and `TIOCGWINSZ` value differ via `libc` per target — no separate linux.rs/macos.rs expected |

### Proposed dependencies

| Crate | Purpose | License | Notes |
|---|---|---|---|
| `libc` | termios, `ioctl(TIOCGWINSZ)`, `sigaction`, `poll`, `fcntl`, `pipe` | MIT/Apache-2.0 | Thin FFI; all `unsafe` confined to `sys/unix.rs`. Links only system libc (glibc / libSystem). No extra native deps |
| `sha1` (RustCrypto) | SHA-1 for auth hash | MIT/Apache-2.0 | Chosen: 0.11.0, `default-features = false`. Verified against `shasum` vectors and C output |

Not used: `clap` (cannot reproduce getopt output byte-for-byte), `tokio`, `nix`/`rustix` (extra surface for ~8 syscalls), any USB crate.
Exact versions recorded when added.

## 6. First slice: device listing

`novacom [-a host] [-p port] [-d filter] -l`, plus `-V`, usage, option errors, and connect-failure paths.

- **Inputs:** argv; device-list lines from a TCP peer.
- **Outputs:** stdout bytes, stderr bytes, exit code; one TCP connect.
- **Exercises:** `cli.rs` (full option table, permutation, P1), `net.rs`, `devlist.rs`, exit-code mapping. No termios/signals/unsafe.
- **Compatibility assertions:** byte-identical stdout/stderr and equal exit code vs the C build on the **same OS**, for: empty list, malformed lines, > 255-byte lines, `-d` match by nduid/conntype/devtype in mixed case, nduid > 40 chars, EOF without newline, refused connection, unresolvable host, `-p junk`, `-V`, unknown option, missing argument, permuted options.
- **Test strategy:** Rust mock `novacomd` (TCP listener serving a fixture) + differential harness that runs `build-novacom/novacom` and the Rust binary against it; unit tests for the line parser. Runs natively on both OSes.
- **Next slices:** (2) `packet.rs` codec + non-tty `data_xfer` (`run` with stdin/stdout, binary, NULs, fragmentation); (3) `-c` control + auth; (4) `-t`/`-s` termios and signals; (5) relay.

## 7. Validation environments

| | macOS | Linux |
|---|---|---|
| Host | `deepthought`, macOS 27.2 (26B123), arm64 | `a010`, Ubuntu 26.10, kernel 7.3.0, x86_64 |
| C compiler | Apple clang 21.0.0 | gcc 15.3.0, glibc 2.44 |
| Rust | 1.99.1 stable | 1.99.1 stable (user-local rustup) |
| C reference build | **Built, no patches**: `BUILDVERSION=ref-d9d8b77 make` (4 warnings) | **Built, no patches**: same command (3 warnings) |
| CMake/webOS build | Not attempted: no `cmake`, no `cmake-modules-webos` | Not attempted: same |
| novacomd / device | Unavailable | Unavailable |

Note: `make BUILDVERSION=x` (command-line) breaks the build because it bypasses the makefile's quoting; the env-var form works.

**Proposed support** (not verified): Linux glibc x86_64 + aarch64; macOS 11+ arm64 + x86_64 (Rust's aarch64-apple-darwin floor).
**Verified so far:** C reference builds only. Gaps: Linux aarch64, macOS x86_64, musl, any real novacomd.

## 8. Decisions (proposed defaults, adopted 2026-10-30; change on user request)

1. **Bugs:** keep deterministic quirks (B3, B6, B11, B12, error texts). Fix unsafe or nondeterministic defects and list each as "intentionally changed": B1 (send the full command with no length limit; C overflows its 4096-byte buffer), B2 (`EAGAIN` → no data), B4 (never drop signals), B5 (zero padding), B7 (full writes), B8 (parse error instead of crash), B9 (no uninitialized print), B10.
2. **getopt diagnostics:** emulate each OS's libc text exactly, with `rovacom` as the program name.
3. **macOS `-t` NUL-as-INTR (P2):** preserved and documented; see IMPROVEMENTS.md I7.
4. **Version:** `version: rovacom-<CARGO_PKG_VERSION>`, overridable by `BUILDVERSION` at build time.
5. **C reference:** not vendored. `scripts/build-reference.sh` clones upstream at the pinned commit into gitignored `reference/` and builds it. Differential tests normalize the program name (`novacom` ↔ `rovacom`).

Improvement ideas found during the port are tracked in [IMPROVEMENTS.md](IMPROVEMENTS.md); they are **not** applied by the port.

## 9. Real-device testing

A Palm Pre3 (last known working unit) will be connected to the macOS host later. Device sessions are read-only by default, every command is approved by the user first, and all fuzz, malformed-packet and stress tests run against mock peers only.

## 10. Behaviors found during porting

- **Session id carry-over with `-w` (slice 3):** C keeps the session id of a matched device in a global (`devsnid`) that only a later match with a non-empty session id overwrites. With `-w`, a match with a negative port leaves its session id behind for a later match. Emulated.
- **macOS PENDIN (slice 3):** after B6 clears ICANON, `resetconsole` turns canonical mode back on and the BSD kernel sets PENDIN on the tty. Both C and rovacom leave PENDIN set. Not seen on Linux.
- **Password prompt visibility (slice 3):** with stdout a pipe the prompt appears after stderr messages; with stdout a tty but stdin not, the prompt appears only after the read; with stdin a tty but stdout not, echo stays on. All emulated.
- **B4 root cause (slice 4):** the C handler `signal_to_pipe` changes `errno` itself (its non-blocking pipe `read` sets EAGAIN; its `TIOCGWINSZ` sets ENOTTY when stdout is not a tty) without saving it. `data_xfer` forwards a signal only if `errno == EINTR`, so forwarding works only when the signal interrupts `select`. Observed on macOS: with the client blocked on a socket write, C forwarded none of 8 signals and printed `data_xfer: Error reading from signal pipe: 25`.
- **macOS `-t` and NUL (P2, slice 4):** confirmed. With the pty as controlling terminal, typing NUL kills both C and rovacom with SIGINT on macOS; on Linux NUL is passed through as data.
- **Relay REPORT_ERROR on local EOF (slice 5):** line 247 prints `strerror(errno)` with the thread's untouched errno: `Success` on Linux, `Undefined error: 0` on macOS.
