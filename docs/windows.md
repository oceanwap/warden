# Windows: status and plan

**Today: not supported.** Warden runs on Linux (production) and macOS
(development). On Windows, use **WSL2**: it is a real Linux kernel, so
everything in the README's Linux column applies unchanged. This page records
what a native Windows build would take, so the decision to do it (or not) can
be made with the costs in view. Nothing here is scheduled.

## Using Warden on Windows now (WSL2)

- Install a WSL2 distribution, then follow the Linux instructions inside it
  (`install.sh` accepts WSL2 as Linux).
- Keep the app and its files on the Linux filesystem (`~/…`), not under
  `/mnt/c`: file watching, `openat2`, `sendfile` and sockets are slower or
  behave differently across the 9P mount.
- `warden startup` needs systemd inside the distribution: set
  `[boot] systemd=true` in `/etc/wsl.conf`, then `wsl --shutdown`. Without
  systemd, start `warden wardend --resurrect` from a Windows Task Scheduler
  entry that runs `wsl.exe -d <distro> --exec warden wardend --resurrect`.
- A port an app listens on in WSL2 is reachable from Windows at
  `localhost:<port>` (WSL2's localhost forwarding).
- The GUI: WSLg shows `warden-gui` on Windows 11; or run `warden-gui` on the
  Mac or Linux desktop and point it at the host over SSH, as for any remote
  host.

## Why a native build is not a port, but a second platform layer

Warden's core is Unix all the way down. Counted in the source today: 22 of its
source files use Unix-only std or tokio APIs (`std::os::unix`,
`tokio::net::Unix*`, `std::os::fd`), there is no `cfg(windows)` anywhere in
`src/`, and the pieces below have no direct Windows counterpart.

| Warden relies on | Where | Windows has |
|---|---|---|
| Unix domain sockets: the control socket, wardend's socket, each worker's private health socket | `control.rs`, `daemon/`, `health.rs`, the shim | Named pipes (`tokio::net::windows::named_pipe` is stable), or loopback TCP with a token. AF_UNIX exists since Windows 10 1803 but neither std nor tokio expose it on stable |
| `SO_REUSEPORT`: N workers on one port, the kernel balances | `sys::listen_tcp`, the shim | **Nothing equivalent.** `SO_REUSEADDR` on Windows lets a second socket bind the same port but does not balance, and is a known hijacking hazard (see "The hard part") |
| Signals: SIGTERM then SIGKILL, SIGHUP/USR1/USR2 forwarding, exit reasons from wait status | `signals.rs`, `process/exit.rs`, `process.rs` | No signals. A console process can get CTRL_BREAK; anything else is `TerminateProcess`. Exit reasons (OOM, killed by SIGKILL) have no source |
| Process groups, parent-death signal | `process.rs`, `sys::child_*` | **Job objects**: stronger than Linux's pdeathsig (`KILL_ON_JOB_CLOSE` kills the whole tree when Warden dies) |
| `socketpair` on fd 3 for the worker's IPC (`WARDEN_IPC_FD`) | `process.rs`, the shim | Inheritable handles, or a named pipe whose name goes in the environment |
| `flock` (ids, state files), `rename` over an open file | `ids.rs`, `sys::try_lock_exclusive`, `logging.rs` | `LockFileEx`. Open files cannot be renamed or deleted unless opened with `FILE_SHARE_DELETE`: log rotation needs care |
| `/proc`: RSS, CPU, owner, listeners, environ, boot id | already behind the `Platform` trait in `src/platform/` (Linux and macOS adapters); cgroups and OOM events stay in `process/exit.rs` | A `platform/windows.rs` adapter: `GetProcessMemoryInfo`, `GetProcessTimes`, `GetExtendedTcpTable`, performance counters. The same `ProcStats` shape fits |
| `sh -c` for shell-style `command`s, `/bin/sh` in alert commands | `fleet.rs`, `daemon/` | `cmd.exe /C`, or require an explicit program and argv on Windows |
| systemd and launchd for `warden startup` | `startup.rs`, `systemd.rs` | A Windows Service (the `windows-service` crate) or a Task Scheduler entry |
| XDG and `/run`, `/var`, `/etc`, `/tmp` paths | `config.rs`, `fleet.rs`, `protocol/src/paths.rs` | `%LOCALAPPDATA%`, `%ProgramData%`, `%TEMP%` |
| `sendfile`, `openat2`, `memfd`, `splice`, `pidfd`, `TCP_DEFER_ACCEPT` | `sys.rs` | Not needed: each already has a portable fallback (written for macOS), selected at compile time. `TransmitFile` could replace `sendfile` later |
| The JS shim: Unix-socket health servers, `process.send`, signal handlers | `shim/warden-shim.mjs` | Named pipes in Node; to verify for Bun (see Spikes) |

What helps: **all `unsafe` and every syscall already sit in `src/sys.rs`** (and
`src/sys/darwin.rs`) behind safe functions, **what differs per OS is behind the
`Platform` trait** (`src/platform/`, one adapter per OS, one set of contract
tests), and the macOS port forced the non-Linux fallbacks to exist and be
tested. A Windows layer is a third implementation of those same seams, not a
rewrite of the supervisor.

## The hard part: sharing a port

A Warden app with `count = 4` is four processes that all `listen()` on one
port and let the kernel spread connections. Windows cannot do that for
Bun/Node servers. The options, in the order we would try them:

1. **One worker per app, plus `port_strategy = "offset"`** for several
   (ports 3000, 3001, …), with a reverse proxy in front. This is also what
   macOS does today for Node, and it needs no new mechanism. Rolling
   restarts work as `restart --hard`, or as an offset rollout.
2. **Worker (thread) mode** (`[workers] mode = "worker"`): one Bun process,
   several threads, one listener. Exists today; depends on Bun's Windows
   support.
3. **A small built-in TCP forwarder** in the supervisor (listen once, hand
   connections to workers' private ports). It breaks the rule that Warden
   never touches application traffic, adds a hop to every request and puts the
   supervisor on the data path: only if users ask and the numbers hold up.
4. **Handle duplication** (`WSADuplicateSocket`), as Node's `cluster` does from
   its primary: needs the runtime's cooperation, which Bun does not offer.

Do not use `SO_REUSEADDR` to fake `SO_REUSEPORT`: on Windows any process of
the same user can then bind the port and take connections.

## Plan

Each phase ends with something shippable and a CI job that keeps it true.

### Phase 0: spikes (about a week; decides the design)

Answer these on a real Windows 11 machine, with a throwaway script each:

- Does `Bun.serve({ reusePort: true })` work on Windows, and what does it do
  with two processes on one port? Same for Node's `net` and `http`.
- Can Bun and Node listen on a named pipe (`\\.\pipe\…`) and connect to one?
  (The health sockets and the IPC channel depend on it.)
- How does Bun handle `SIGBREAK` and `SIGINT`? Does a worker started in a new
  process group with `CTRL_BREAK_EVENT` run its shutdown handlers?
- Does a Job object with `KILL_ON_JOB_CLOSE` also kill grandchildren (npm,
  `sh`, `cmd`)?
- Does `std::fs::rename` over a log file that another process holds open
  work with `FILE_SHARE_DELETE`, as rotation needs?

Output: a short result table in this file and a go/no-go.

### Phase 1: compile and run `warden start` (CLI + supervisor, one worker)

- Split `sys.rs` into `sys/unix.rs` and `sys/windows.rs` behind the same
  function names (the `cfg` arms in the file today are the template).
  `windows-sys` replaces `libc` there; the rest of the crate stops naming `libc`.
- Gate Unix-only modules (`signals`, `startup`'s systemd/launchd code) with
  `cfg(unix)` and give Windows stubs that say "not supported on Windows" with
  a `hint=`, as the house error style requires.
- A transport layer for the control socket: `Listener`/`Stream` over a Unix
  socket or a named pipe, chosen by `cfg`. The wire protocol is unchanged
  (`docs/protocol.md`), so the CLI, wardend and the GUI keep one codec.
- Job object per app: spawn workers into it, stop = CTRL_BREAK, wait the grace
  period, then terminate the job. Exit codes only: report `exit code N`, never
  a signal.
- Worker IPC and health: the supervisor creates the pipe, passes its name in
  `WARDEN_IPC_PIPE`; the shim connects (as it does for fd 3 today).
- Paths: `%LOCALAPPDATA%\warden` (config, state, runtime), `%ProgramData%` when
  elevated.
- CI: a `windows-latest` job that runs `cargo check` and `cargo test --lib`
  first, so the layer cannot rot while it grows.

Exit criteria: `warden start server.ts --name api`, `list`, `describe`, `logs`,
`restart`, `stop`, `delete` work for one Bun app on Windows; unit tests pass.

### Phase 2: the metrics and the rest of the CLI

- CPU and RSS (`GetProcessTimes`, `GetProcessMemoryInfo`), the process owner,
  listening-port detection (`GetExtendedTcpTable`) for readiness, uptime from
  creation time. `max_memory` recycling works once RSS does.
- `warden pm2-migrate` on Windows (PM2's `jlist` is the same; the `/proc`
  environment and port reads fall back to PM2's own dump).
- Log rotation with `FILE_SHARE_DELETE`; `logs -f`, `--history` work.
- Shell commands: `cmd /C`, plus a clear error for scripts that assume `sh`.
- `warden serve` on the portable read/write path (no `sendfile`/`openat2`);
  the path-confinement check uses canonicalisation plus a reparse-point test.

### Phase 3: several workers and rollouts

Pick from "The hard part" using the Phase 0 results: `port_strategy = "offset"`
rollouts first, thread mode if Bun supports it, and say in `warden doctor`
which of the two a Windows host is using and why `count > 1` on one port is
refused.

### Phase 4: boot, wardend and the GUI

- `warden startup` installs a Windows Service (or a Task Scheduler entry) that
  runs `warden wardend --resurrect`; `unstartup` removes it.
- wardend's host stats (`GetSystemTimes`, `GlobalMemoryStatusEx`) so the GUI's
  host panel works.
- The GUI: iced's Windows backend (winit) is already part of the toolkit; drop
  the `x11`/`wayland` features on Windows. Test the SSH-tunnel path too, since
  there is no Unix socket to forward to.

### Phase 5: release

`warden-x86_64-pc-windows-msvc.zip` (CLI) and a GUI bundle, in the release
workflow with the SHA256SUMS; `install.ps1`; Authenticode signing (SmartScreen
treats unsigned binaries as suspicious); README Platforms and `docs/` updated
with a list of the differences below.

## What will stay different on Windows

Things we should say plainly in the README and `warden doctor` instead of
hiding:

- No `SO_REUSEPORT` balancing: one worker per port (Phase 3 describes the
  workarounds).
- Graceful stop depends on the runtime honouring CTRL_BREAK; after the grace
  period the job is terminated, which is the equivalent of SIGKILL.
- No signal names in exit reasons, no OOM attribution, no `SIGUSR1/2`
  forwarding, no `warden signal` beyond stop and break.
- Hot standbys that rely on promoting an idle process need the port-sharing
  answer from Phase 3 first.

## Cost and risk

Rough, and only an estimate: Phases 0 and 1 are where the uncertainty is
(two to three weeks, most of it the transport layer and the process
lifecycle). Phases 2 to 5 are mostly mechanical once the layer exists, but
touch a lot of files: expect another month, and a standing cost afterwards,
because every new Unix-only convenience (a new syscall, a new `/proc` read)
needs a Windows arm and a CI run. Windows runners are also dearer than Linux
ones on private repositories, so keep that job to `check` and unit tests until
the integration suite (which leans on `sh`, `sleep` and Unix sockets) is ported.

A smaller option with most of the value for people who only want a local dev
supervisor: ship **Phase 1 plus 2 with one worker per app**, label it
"experimental", and stop there.

## Decision rule

Do not start until Phase 0's answers exist. If Bun on Windows can neither
share a port nor listen on a named pipe, a native build is not worth it and
WSL2 stays the answer; say so in the README and close this page's plan as
"rejected" with the evidence.
