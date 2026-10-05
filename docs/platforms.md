# Platforms

- **Linux x86_64 and ARM64**: supported for production. CI runs the unit
  and integration tests on both (and formatting and clippy on x86_64).
- **macOS**: for development only, to run an app locally with the same
  config. CI builds it and runs clippy and the unit tests (Apple Silicon).
  What it lacks:
  - `SO_REUSEPORT` does not load-balance on macOS: several workers can
    share the port, but one of them gets every connection. So with more than
    one worker, Warden owns the port itself, accepts, and hands each
    connection to the worker with the fewest in flight, over the worker's
    Node IPC channel (fd 4, `NODE_CHANNEL_FD`: the message Node's own
    cluster module sends, socket attached). From then on client and worker
    talk directly: Warden never touches the traffic, and the app sees the
    client's address. It works for `node:http` servers (Express, Fastify,
    NestJS, …) under Node and under Bun; measured on an M-series Mac, 4
    workers of a 1 ms-per-request app served 3.2–3.4× what 1 worker did
    (p99 3 ms instead of 7–10), and a trivial app the same within noise.
    Apps on `Bun.serve` itself (Elysia, Hono on Bun) cannot take a handed-over
    connection: they share the port as before (one worker gets the
    traffic), so keep `count = 1` for them locally. `WARDEN_HANDOFF=0` in
    Warden's environment turns the handoff off (`=1` turns it on elsewhere,
    as the tests do on Linux). It is decided when the supervisor starts: an
    app scaled from 1 worker to more needs a `warden restart`.
  - No parent-death signal: workers outlive a supervisor killed with
    SIGKILL (or one that crashed, or that launchd killed in the middle of a
    shutdown). Warden stops them the next time the app starts, before it
    starts new ones: [below](#workers-a-killed-supervisor-leaves-behind-macos).
  - `warden serve` checks static paths with realpath instead of `openat2`
    (same confinement, slower).
  - No out-of-memory attribution: a worker the kernel killed for memory
    shows as `killed by SIGKILL`, not as an OOM kill (macOS has no cgroup
    events to read).
  - CPU, memory, the user, listening ports, a process's environment and
    the host's numbers come from macOS's own interfaces (libproc, `sysctl`
    and Mach calls) through a small adapter per OS (`src/platform/`), so
    the CPU and mem columns, readiness by port, wardend's `host` events and
    its restart of a supervisor with the environment it was started with
    all work as on Linux. `warden doctor` says what the OS you are on
    cannot do.
- **Windows**: not supported, and not planned for 0.1. Warden is built on
  Unix sockets, signals, process groups and `SO_REUSEPORT`, none of which
  Windows has in the same form. **WSL2 works as Linux** (it is a real Linux
  kernel): follow the Linux instructions inside the distribution, and keep
  the app on the Linux filesystem. [windows.md](windows.md) has the
  tips and the plan for a native build, with the one hard problem (sharing a
  port between workers) spelled out.

## Workers a killed supervisor leaves behind (macOS)

On Linux a worker gets SIGTERM the moment its supervisor dies
(`PR_SET_PDEATHSIG`). macOS has no such thing, so a supervisor that is killed
(`kill -9`, a crash, launchd's timeout in the middle of a shutdown; a normal
`warden stop` or SIGTERM stops its workers first) leaves them running, still
holding the app's port. The next supervisor of the app would start a second
set next to them, and where `SO_REUSEPORT` lets two processes share the
port, silently double the workers while the old set keeps answering.

Warden covers this with a sweep at the start of the app:

- Each supervisor keeps a record of its processes in the state directory
  (`~/.local/state/warden/orphans/<app>.<supervisor pid>.json`, or under
  `$WARDEN_HOME/state`; `/var/lib/warden` for root): the supervisor and each
  worker as a pid and the time that process started (libproc,
  `PROC_PIDTBSDINFO`). It is rewritten, atomically (a file renamed over the
  old one, so a reader sees the old or the new, never half), whenever a
  worker starts or exits, and removed when the supervisor has stopped its
  workers and exits.
- When a supervisor starts, before it starts any worker, it reads the records
  of its app and stops a recorded worker only if all of these hold: the
  record is from this boot; its supervisor is not running (no process has
  that pid, or the process that has it started at another time); a process
  with the worker's pid exists **and started at the recorded time**; and
  that process's parent is not the recorded supervisor. A pid that has been
  given to another process since is never touched: the start time is what
  tells them apart. Workers of other apps and of a supervisor that is still
  running are never candidates.
- It stops them as a normal shutdown does: `shutdown.signal` (SIGTERM unless
  set) to the worker's process group, SIGKILL after `shutdown.grace_period`
  to what is still there. A worker that is no longer the leader of its own
  process group (it moved itself to another) is signalled alone: that group
  is not Warden's to stop.
- The log says what it did: a WARN naming the workers and the supervisor
  they belonged to before it stops them, an INFO with the count after, a WARN
  if SIGKILL was needed, an ERROR with the `kill -9` to run for one that would
  not die. A record that cannot be read is removed with a WARN; a record
  written by a newer Warden is left alone.
- `warden doctor` lists, on a Mac, the workers that are running right now
  after their supervisor was killed (`warden start <app>` then stops them),
  and checks that the record directory is private to you.

`parent_death_signal` stays false in the OS capabilities: the sweep is not a
parent-death signal, only the cure for its absence, and it has limits:

- It acts at the next start of the app, not when the supervisor dies. Until
  then the orphans keep serving, holding the port and their memory. With
  wardend that is seconds (it restarts a dead supervisor); without it, until
  you `warden start` the app or kill them by hand (`warden doctor` names them).
- The sweep takes as long as the orphans take to stop: at most
  `shutdown.grace_period` plus 3 s when they ignore the stop signal. The new
  supervisor does not wait in silence, and starts no worker until it is over
  (the old ones hold the port):
  - `warden status` and `warden list` show it as a rollout of its own kind,
    `sweep 1/2: waiting for the previous supervisor's workers to stop (up to
    33 s)`, with how many are gone; the app is not "unreachable".
  - `warden start` prints the same line and waits for the sweep before it
    starts counting `ready_timeout` for the first worker, so a long grace
    period does not make it report a failed start.
  - SIGTERM, SIGINT and `warden shutdown` end the supervisor at once, without
    starting a worker; what the sweep did not reach is found again by the next
    start (the records stay until their workers are gone). `warden stop` and
    `warden start` are remembered for when the sweep ends. `reload`,
    `restart`, `scale`, `reset` and SIGHUP are refused until then, with a
    message saying so.
- Processes a worker started that outlived it are stopped with its group while
  the worker is still running at the next start. If the worker itself exited
  before that, its group is not touched: the number of an empty group may
  belong to someone else by then, and nothing proves it does not. The same
  for a worker that left its group: what it started stays.
- A worker started in the instant before the record was rewritten (a
  supervisor killed within a millisecond of a spawn) is not in it.
- Between reading a process and signalling it there is a gap in which it
  could exit and its pid go to another process; macOS gives out pids in
  sequence, so it takes the whole pid space (99999) being used within microseconds.
- Nothing survives a reboot, and nothing needs to: a record from an earlier
  boot is ignored.
- If the state directory cannot be written Warden says so (WARN) and runs
  without the protection.

## Release archives and what they need

Prebuilt archives are attached to each
[GitHub Release](https://github.com/oceanwap/warden/releases). Each release
has two products for Linux (x86_64, arm64) and macOS (arm64, x86_64):

- **CLI only**: `warden-<version>-<os>-<arch>.tar.gz` with the `warden`
  binary, the README and `contrib/` (systemd units, sysctl file, nginx site). The Linux
  binaries need glibc 2.28 or newer (RHEL 8, Debian 10, Ubuntu 18.10+).
- **GUI + CLI**: `warden-gui-<version>-<os>-<arch>` (`.tar.gz` on Linux, a
  zipped `Warden.app` on macOS) with `warden-gui` next to `warden` (see
  [the GUI](../gui/README.md)). On Linux the GUI links only glibc (2.35+: built on Ubuntu
  22.04) and loads the display libraries at run time, which every desktop
  has: `libxkbcommon`, then on Wayland `libwayland-client` and
  `libwayland-cursor`, on X11 `libX11`, `libX11-xcb`, `libXcursor`, `libXi`
  and `libxkbcommon-x11`. It draws on the CPU
  (tiny-skia): no GPU, Vulkan or OpenGL driver is needed. macOS: the app
  is not notarized by Apple yet, so the first open is refused ("Apple could
  not verify…"); then System Settings → Privacy & Security → Open Anyway
  (macOS 15 dropped the right-click → Open shortcut), or
  `xattr -dr com.apple.quarantine /Applications/Warden.app`. The CLI
  installed by `install.sh` is not affected.

How to download, verify and install them: [install.md](install.md).

## See also

- [`how-it-works.md`](how-it-works.md#limitations): the shim and its limits.
- [`production.md`](production.md): systemd units on Linux, launchd on macOS.
- [`windows.md`](windows.md): Windows status and plan.
