# Changelog

All notable changes to Warden are documented here.
Format inspired by [Keep a Changelog](https://keepachangelog.com/).

## [0.1.6] — 2026-10-10

### Changed

- An app that is not running reads as such: `warden status` and the other
  commands say "not running", and when a supervisor that was killed left its
  control socket behind, they say so, instead of "cannot reach warden at …
  (Connection refused). Is it running?".

### Fixed

- `warden update` (and `start`, `resurrect`) no longer leaves the apps without
  wardend when the launchd job is another Warden's. A job that starts a
  wardend for a different runtime directory (one written under another
  `$WARDEN_RUNTIME_DIR`, a test run's) was trusted: `update` stopped
  wardend, asked launchd for that job, said "wardend does not answer yet" and
  ended with exit code 0. Such a job is now named, with the fix (`warden
  startup` writes it again), and wardend is started without it meanwhile.
  `warden doctor` reports it under `boot` instead of saying the job brings
  the apps back.
- `warden update` ends with an error (exit code 1) when wardend does not
  answer afterwards, instead of reporting success.
- The desktop app starts itself again after an update made outside it
  (`warden upgrade` or the installer in a terminal, then `warden update`):
  when wardend comes back and the app's own program on disk is no longer the
  one it started from, the window reopens on the new version. If a form or a
  question is open, it only says so and stays. Before, it kept running the
  old version until it was quit.

## [0.1.5] — 2026-10-10

### Added

- `warden update` no longer stops the apps. Each app's keeper re-executes
  itself from the new binary (same pid, so wardend and systemd see nothing
  change) and the supervisor it starts takes the same workers back, as
  after a crash: requests are answered throughout and the workers keep
  their pids, connections and uptime. The new binary must read the app's
  config first (`warden check`), or nothing changes. An app without a
  keeper, or whose supervisor is too old to be asked, is restarted as
  before. `warden upgrade` and the GUI's "Restart everything" use it.
- A crash of an app's supervisor no longer touches its workers. `warden
  start` now runs as the app's keeper: a small process that starts the
  supervisor, holds a copy of each worker's pipes and fd 3, and becomes the
  workers' parent if the supervisor dies (a panic, an OOM kill, `kill -9`).
  It reads their output meanwhile (up to 1 MB per stream), starts the
  supervisor again at once, and hands it the same workers, which it
  supervises again with their pids and uptime; nothing is restarted but
  the supervisor. In a 2-minute chaos run (release build, same seed), 6
  `kill -9`s of a supervisor lost no request and every app was ready again
  in 193 ms (p50); on 0.1.4 the same fault took the app down for 1.4 s
  (p50) and lost 243 requests. The keeper costs about 1 MB per app (PSS
  7.9 MB for keeper and supervisor against 6.9 MB alone). After 6
  supervisor deaths in a minute the keeper stops the workers and exits, so
  wardend or systemd takes over. `status` gains `supervisor_pid`; `pid` is
  the keeper's (what wardend and systemd watch). `[restart]
  keep_workers_on_crash = false` or `WARDEN_KEEPER=0` turns it off.
- Workers come back under Warden when every Warden process of their app
  dies at once (the supervisor and the keeper, with wardend or not; Linux,
  Bun and Node apps under the shim). Each worker holds a copy of Warden's
  ends of its channels, so its output never hits a closed pipe (a Node app
  that logged crashed with EPIPE right away), and when it sees its
  supervisor and keeper gone it starts wardend again if needed and hands
  itself to the app's next supervisor, which wardend starts. The same
  workers are supervised again, with what they printed meanwhile; no
  request failed in testing, about 3 s from the kill to supervised again.
  wardend, when it starts, now starts any app whose workers run with no
  Warden process (workers not under the shim are replaced one by one).
  New workers also never reuse a kept worker's instance number (it names
  that worker's private socket).
- `cargo xtask chaos`: `kill-supervisor` now kills the supervisor under the
  keeper and checks that the same workers serve throughout; the new
  `kill-keeper` kills the app's process, as `kill-supervisor` did.

### Changed

- A Node TLS app's workers share one session-ticket key, so a returning
  visitor resumes their TLS session on whichever worker the kernel picks,
  not only on the one that issued the ticket (1 in N before, with N
  workers). A resumed handshake took a third less server CPU (Node 22, 1,229
  → 831 µs per connection); with 2 workers under Warden, returning visitors
  cost 5-20 % less server CPU and got 14-30 % more connections per second
  (3 interleaved rounds against 0.1.4). The key is made when the app's supervisor
  starts, handed to each worker on fd 3 before it runs, never written to
  disk or the environment, and steps forward every 12 hours by a one-way
  hash. An app that sets its own `ticketKeys` keeps them. Not yet for
  Bun.serve, which ignores `ticketKeys`.
- The `[route]` router no longer waits for the app's supervisor to confirm
  each handed-over connection: once the socket is sent, it is the
  supervisor's. New TLS connections through the hand-off took about 4 %
  less server CPU (median 437 against 457 µs, 10 interleaved rounds). A
  router and a supervisor of different versions still work together.

### Fixed

- Stopping wardend now waits until it has exited, not only until its socket is
  gone: a wardend started right after (`warden startup` handing it to
  systemd, `warden update`) found the old one's lock still held and exited
  at once.
- A supervisor restarted by its keeper (after a crash, or `warden update`)
  stopped the workers of an app scaled up since it started (`warden scale`)
  and started new ones: it now keeps them, and the app its size.
- On Bun 1.4, a reload or scale-down no longer cuts requests sent on idle
  keep-alive connections to a draining worker. Bun 1.4's `server.stop()`
  closes idle keep-alive connections at once (1.3 kept them open), so a
  request a client was sending on one right then was lost (4 of 219 in the
  keep-alive test, over 3 reloads). On Bun 1.4 a draining worker now keeps
  accepting through `drain_ms`, answering with `Connection: close` so
  clients move to the new workers, and stops accepting at its end, as Node
  workers do. Bun 1.3 is unchanged.
- An app whose servers run deep below its worker shows their ports again.
  The listener walk went four processes down; a turbo monorepo's dev
  script (`bun run dev`, dotenv, turbo's node shim, turbo, `bun run dev`,
  the server) puts its servers five down, so the app showed no ports. It
  now goes eight down.
- After a restart that took workers back, a reload no longer leaves the new
  workers without health sockets. The supervisor numbered its workers from 1
  again, so new workers reused the taken-back ones' socket names and lost
  their sockets when those drained (on macOS they were then marked
  unhealthy). Taken-back workers keep their numbers and new ones count on
  from above them.
- A static site no longer closes a keep-alive connection when a drain (a
  reload or scale-down) begins while it is answering. The answer said
  `Connection: keep-alive`, so the client sent its next request on a socket
  that was then closed, and lost it. Now only an answer that said
  `Connection: close` ends the connection; a request that arrives during
  the drain is answered with `Connection: close`.
- On macOS the integration tests no longer read or rewrite the user's real
  wardend LaunchAgent; each test gets a throwaway launchd directory.

## [0.1.4] — 2026-10-08

### Added

- `[app] address = "2001:db8::10"`: an app's own IP address, so several apps
  can each listen on port 443 of one server with nothing in front of them.
  The kernel sends each visitor to the app by the address it connected to,
  at the app's own speed and with the visitor's IP. The shim makes the app's
  listen use that address whatever host it asks for. On Linux the supervisor
  adds a missing address to the default route's interface (`nodad
  preferred_lft 0` for IPv6) and removes it when the app stops. With the
  `[route]` router on the server's IPv4 address the same apps take IPv4
  visitors too: their workers keep listening on their own address and only
  the router's connections are handed over.
- `warden expose api.example.com --app api`: nginx in front of an app in one
  command. It writes the app's nginx site file with the settings of
  `contrib/nginx.conf` (the app's port, the hostnames, a map and upstream
  named after the app), checks it with `nginx -t`, reloads nginx, and records
  the hostnames in the app's config (`[expose]`). A failed check puts the
  previous file back. HTTPS with HTTP/2 and a port 80 redirect: `--acme
  <email>` has nginx's ACME module get and renew a Let's Encrypt certificate,
  or `--cert`/`--key` (a certbot certificate in /etc/letsencrypt/live is found);
  `--websocket` and `--sse` paths; `--remove`; `--dry-run`. Warden stays off
  the request path: nginx connects to the app's port as before.
- `[route]`: Warden's own hostname router, for several apps on one IP and
  port without nginx or Cloudflare. It reads the hostname from each TLS
  ClientHello and passes the still-encrypted connection to that app's port
  (`"api.example.com" = "api"`, `*.example.com`, `"*"`); each app does its own
  TLS and HTTP/2. On Linux apps see the visitor's IP address (IP_TRANSPARENT,
  with two routing rules Warden adds while the router runs). One worker per
  core by default; small messages pass with one read and one write, bulk
  transfers with splice(2). As fast as nginx's `stream` pass-through in our
  measurements. Where the app can take a handed-over socket (node:http, or
  Bun.serve with `server.adopt`), the router hands the connection itself to
  the app's workers through the app's supervisor and leaves the path: no
  bytes are copied and requests run at the app's direct speed. See
  `docs/routing.md`.

### Fixed

- A Bun app that serves TLS itself (`Bun.serve({ tls })`, with no proxy in
  front) failed every health check, so with `[health]` set each reload rolled
  back ("not an HTTP response"). The shim's private health socket copied the
  app's `tls` options; it now stays plain HTTP. The public port keeps its TLS.
- The same for a Bun app with HTTP/3 on (`http3: true`): Bun refuses HTTP/3
  without TLS, so the private health socket never opened and every reload
  rolled back. The private socket now drops `http3` too.
- A `node:http2` app (`http2.createSecureServer` or `createServer`) got no
  private health socket, so with `[health]` set every reload rolled back
  ("only 0 of 1 private health socket(s) reported"). It now gets one, as a
  `node:http` app does.

## [0.1.3] — 2026-10-08

### Added

- Worker mode (Bun threads) warns at start, in `warden doctor` and in the GUI
  where its threads cost speed: on macOS (the threads share the port, but one
  gets most connections) and on Bun older than 1.4 (20-50 % slower than
  processes for NestJS). On Bun 1.4 on Linux it serves as fast as processes
  with 14-22 % less memory; docs/benchmarks.md has both runs.
- macOS: more than one worker now spreads the load. The kernel there gives
  every connection on a shared port to one worker, so Warden accepts on the
  port and hands each connection to the least busy worker, the way Node's
  cluster module does (the socket itself, over Node's IPC channel); the
  traffic then goes straight between client and worker. For `node:http`
  apps under Node and Bun 1.4+: about 3.3× the requests of one worker for a
  1 ms-per-request app, p99 a third. Node apps can now run several workers
  on macOS at all, and reload without `EADDRINUSE`. `Bun.serve` apps are
  unchanged. On by default with more than one worker; `WARDEN_HANDOFF=0`
  turns it off.
- Worker mode (`[workers] mode = "worker"`) is refused on macOS instead of
  warned about: the kernel there would give every connection to one Worker
  thread. The error points to process mode, which shares the load on macOS
  through the handoff. On Linux it stays as it is.
- With Bun older than 1.4 (which cannot take a handed-over connection),
  Warden warns once that the workers do not share the load and suggests
  `bun upgrade`.
- Linux: `warden doctor` and the GUI (a quiet line on the app's page) say when
  an app runs 1 worker on a host with more cores, and that `count = "max"`
  under `[workers]` runs one per core. There the kernel spreads connections
  over the workers sharing a port, so under load the other cores sit idle
  with one. Only a hint: an app that keeps state in memory needs its one
  worker. Supervisors send it as `status.hint`.

### Changed

- `warden reload`, `restart` and `safe-reload` return as soon as the rollout
  ends: they follow the supervisor's event stream instead of asking for its
  status every 250 ms, so they no longer wait up to a quarter second more
  (4 `node:http` workers on a busy host: reload median 648 → 500 ms, restart
  519 → 470 ms). They fall back to polling when the stream is not available
  (an older supervisor, too many live streams). `warden start` notices the
  supervisor and its ready workers sooner, and wardend starts the next saved
  app at boot sooner. Progress lines now show every phase of a rollout (a
  250 ms sample could skip a short batch); exit codes are unchanged.
- Reloads with a `[health] path` are faster: a new worker's first health check
  runs as soon as it listens, each next one `health_interval_ms` after the
  previous one ended, and the worker takes over the moment its last check,
  `verify_command` or soak passes. They used to wait for a shared 500 ms tick
  at each step: listening to taking over went from about 1.9 s to 1.0 s with
  the defaults, and `warden reload` of 4 node:http workers from 8.5 s to 5 s.
  The gates are the same: `health_passes` checks in a row, never closer than
  `health_interval_ms`. A first check that fails resets the passes but
  doesn't count toward failing the worker, so a slow starter gets no less
  time than before.
- Linux: a supervisor no longer reads the host's whole socket table to find
  what its workers listen on (`status`, `warden ports`) or whether a new
  worker listens yet. It asks the kernel for the listening sockets only (one
  netlink request for all the workers, as `port_lost` already did), where
  `/proc/net/tcp` has a row per connection: with 20,000 idle keep-alive
  connections on a 4-worker app, the supervisor went from 14 % to 3.7 %
  of a core (what is left is reading which sockets each worker holds). A
  worker without the shim is now seen listening within 20 ms (it was up to
  250 ms). Workers in another network namespace are read from `/proc` as
  before; the answers are the same.

### Fixed

- A stopped supervisor (SIGSTOP, a frozen VM, a bug) no longer freezes its
  workers. They report to it over a socket, and once its buffer was full
  (macOS: 8 KB, about a minute of heartbeats; Linux: a few minutes) the next
  report blocked the worker's event loop until the supervisor read again. Now
  the socket holds far more (hours of heartbeats where the system allows it),
  and under the shim and the static server it never blocks: when full, only
  the latest heartbeat waits, and every other message (`listening`, `ready`,
  `draining`) waits whole and in order until the supervisor reads.
- macOS: a connection a worker refused (`NODE_HANDLE_NACK`, e.g. at its
  file-descriptor limit) was handed out again forever, a busy loop. Like
  Node's cluster module, it now goes round at most 3 more times, to the least
  busy worker, and is then closed, with a warning (at most once a minute)
  saying how many were closed and what to raise.
- macOS: apps that wardend starts (at login, after `warden update`, or when
  it restarts a supervisor) ran at background priority (20 instead of 31):
  launchd ran wardend as a background job and every app inherited it, so on a
  busy Mac they got less CPU and more often the efficiency cores. `warden
  startup` now writes `ProcessType = Interactive`, and a job an older warden
  wrote gets it the next time warden starts wardend through launchd (`warden
  update`, the GUI's Update), with nothing to run by hand.
- Security: `warden serve` could serve a file outside its root when a
  symlink under the root was swapped between the check of a path and its
  open. On macOS every request had that window (the path was checked with
  realpath, then opened); on Linux only paths through an absolute symlink
  did. Now the kernel keeps the open inside the root: `O_RESOLVE_BENEATH` on
  macOS 15+, `O_NOFOLLOW_ANY` (no symlinks) on macOS 11-14, `openat2` on
  Linux; a path through a symlink is resolved, and its real path opened the
  same way. Symlinks that stay inside the root work as before, and a Mac
  now opens most files in one system call instead of a realpath walk.
- `warden logs -f` and the GUI's log no longer say "lines skipped" to a
  client that reads promptly when a worker writes many lines at once (more
  than 256 in one write did it): followers get a write's lines together, and
  only one that really falls behind is told how many lines it missed.
  wardend, relaying logs to the GUI, also gives its clients a turn during a
  burst.

## [0.1.2] — 2026-10-05

### Fixed

- `warden update`, `start` and `resurrect` no longer hang on a launchd job an
  older `warden startup` wrote (a warden that is gone, or the old `daemon`
  command): warden says so, names `warden startup` to rewrite it, and starts
  wardend itself. `launchctl` and `systemctl` calls stop after 20 s.
- `warden update` run while apps are down keeps them in the saved list and
  starts them again; `warden save` with nothing running keeps the saved list.
  Every save keeps the list it replaces in `dump.json.bak`.
- A wardend that finds another one running exits 0, so launchd no longer
  restarts it every 10 s beside one the GUI started.
- A supervisor started before `warden startup` no longer brings a killed
  wardend back itself once launchd or systemd runs it, so their restart is the
  one that runs.
- GUI: the top bar's CPU, memory and load readings keep their width, so the
  bar no longer shifts.

### Added

- GUI: an outdated supervisor's banner restarts everything itself (no command
  to copy).
- GUI logs: long lines wrap; a full-screen button; "Open in Terminal" follows
  the log in a terminal window (`warden logs <app>`, over `ssh -t` for a
  remote machine).
- GUI: Delete, beside Start and Reset, after typing the app's name.
- `warden upgrade`: installs the latest release (or `--version`) over this
  `warden` and its GUI, then restarts every supervisor and wardend onto it; no
  other step. `--check` (and `--json`) only says whether one is out.
- GUI: a new release shows a banner and, once per release, a desktop
  notification; Update installs it, restarts everything and reopens the window
  on the new version. Settings > Updates checks on demand and has an opt-in
  "Install updates automatically" switch.

### Changed

- The logo is the app icon's W shield everywhere, in green on a neutral dark
  plate: the macOS icon (now edge to edge, no frame in the Dock), the PNGs, the
  window's header, and the README images.
- GUI: the dark background is a more neutral gray, with less green.

## [0.1.1] — 2026-10-04

### Added

- Every GUI package carries the CLI: the Linux `warden-gui` packages (deb, rpm,
  Arch) ship `/usr/bin/warden`, provide and replace `warden`, and removing
  `warden-gui` removes the CLI with it. The GUI archives and `Warden.app`
  already did.
- `warden gui-install`: installs the GUI for this `warden`'s version
  (`Warden.app` on macOS; `warden-gui`, a menu entry and an icon on Linux).
  `--dry-run`, `--uninstall`, `--version`, `--dir`, `--app-dir`.
- `install-gui.sh`: `install.sh --gui`, checked against the release's
  `SHA256SUMS`; `install.sh --gui-only` leaves an existing CLI as it is.

### Changed

- `warden list`: watching is ✓ / ✗, ports are listed one per line, and the
  last-exit column is gone (uptime says what it said).
- Static serving: cached bodies of 8 KB and up are sent from the accept loop.
- Only a pushed `v*` tag releases; the Release workflow run by hand is always
  a dry run.
- `install.sh`'s next steps no longer point into the archive's `contrib/`.

## [0.1.0] — 2026-10-03

The first release.

### Added

- Process-mode supervisor for Bun/Node HTTP workers: `SO_REUSEPORT`, graceful
  drain, gated rollouts (`reload` / `deploy`), restart backoff, hot standbys,
  release pinning (`[app] pin_release`), exit reasons (including the kernel
  OOM killer).
- Rolling restarts that overlap the drains of old workers
  (`[reload] max_draining`), and WebSocket / SSE connections closed cleanly
  during a drain (`[shutdown] long_lived_timeout`).
- Experimental worker mode (Bun Workers + embedded shim).
- `warden update`: save, stop every supervisor and wardend, start them again
  from the warden binary on disk (a supervisor keeps the code it started with,
  so this is how a running host picks up an upgrade); the GUI has the same
  under Settings, "Restart everything". `warden list` and the GUI say when a
  supervisor is older than the warden asking, and which numbers it lacks.
- wardend is always on: `warden start` and `warden resurrect` start it, a
  supervisor starts it again when it dies without a clean exit (a crash or
  `kill -9`), and only `warden kill` stops it. There is no `warden daemon`
  command; `warden check -c wardend.toml` and `warden doctor` check the alert
  rules, and wardend reads the file again by itself when it changes.
- `warden resurrect`, `warden update` and `warden start a b c` start the apps at the same
  time, one per CPU core at once so a big host is not asked to start everything together:
  `--parallel N` (`-j N`, `all` for no limit) or `$WARDEN_PARALLEL` changes it; wardend's own
  `--resurrect` follows `$WARDEN_PARALLEL` too.
- The workers' directory (the folder a static site serves) shows under the
  project name in the GUI and in `warden describe`.
- GUI settings: Warden or the desktop's own colors (macOS, GNOME and
  Ubuntu), and light, dark or automatic.
- `wardend` control daemon: second-level supervision, alerts
  (webhook/command), a 24 h resource history that survives restarts, a
  Unix-socket protocol shared with the CLI and GUI (`protocol/`,
  [`docs/protocol.md`](docs/protocol.md)).
- Native GUI (`warden-gui`): live status, logs, history charts, actions;
  optional single-host SSH tunnel (`--ssh`).
- CLI: start/stop/reload/scale/status/logs, `doctor`, `flush`, `startup` /
  `unstartup` (systemd and launchd), PM2 migration, per-worker event-loop
  delay, `warden start` that fails fast when an app cannot start.
- `warden list` as a boxed table like `pm2 list`, with an id for every app
  (kept in the state directory's `ids.json`), and ids, lists and ranges as
  targets for every command: `warden start 0,1,2`, `warden stop 0-3`,
  `warden restart api web:2`.
- `warden describe`, `status <app>`, `daemon status` and `doctor` print boxed
  tables like PM2's, and `start`, `stop`, `restart`, `reload`, `delete`,
  `scale`, `reset`, `resurrect` and `serve` print the app table afterwards
  (on a terminal, or with `WARDEN_TABLE=1`). Wide cells wrap at spaces when
  the terminal width is known.
- [`docs/windows.md`](docs/windows.md): why Warden does not run natively on
  Windows, how to use it under WSL2, and the plan for a native build.
- Platform adapters (`src/platform/`): one trait for what Warden asks the OS
  (a process's memory, CPU, owner, ports, environment and working directory,
  every process's command line, the host's load, the boot id), with a Linux
  (`/proc`), a macOS (libproc, `sysctl`, Mach) and an "other Unix" (answers
  nothing) implementation, chosen at build time and checked by one set of
  contract tests. On macOS the CPU and mem columns, `warden list`'s ports
  check, wardend's `host` events, PM2 daemon detection and restarting a
  supervisor with its original environment now work (they were empty).
- `warden doctor` checks the OS adapter live (a `platform` row: it asks about
  its own process and says what it could not read), and
  `scripts/mac-check.sh` runs that, the platform tests and a smoke test of a
  real supervisor and wardend on a Mac (and times `warden serve`), and writes
  a report.
- The ports and Unix sockets each app really listens on, read from the OS
  (`/proc` on Linux, libproc on macOS) for the worker and the processes it
  started, so a wrapper like `npm run start` or `turbo` and a monorepo's
  several ports are found: a `ports` column in `warden list` and the workers
  table, a `ports` row in `describe` and `status` (with `all interfaces` or
  `localhost only`, and a warning when the configured `port` is not one of
  them), `warden ports [app] [--json]` with a URL for each, and
  `status.workers[].listening` in the protocol. Warden's own health sockets
  are not shown.
- A `user` column in `warden list` and the workers table, and a `user` row in
  `status` and `describe`: who the app's processes run as (`Status.user`,
  [`docs/protocol.md`](docs/protocol.md)).
- `pm2-migrate` leaves out environment names an env file cannot hold (PM2
  records one named after the app) and lists them in `MIGRATION.md`, instead
  of writing an env file Warden refuses to read.
- Built-in static file server with an in-memory cache.
- Log capture and `worker_output = "direct"` (splice into files), rotation,
  history and search.
- Benchmarks (`cargo xtask bench`), a seeded chaos soak (`cargo xtask chaos`),
  and a release script (`cargo release`).
- CI: Linux (x86_64, arm64) and macOS builds and tests; a workflow that checks
  `warden startup` against real systemd and launchd; benchmarks on x86_64 and
  ARM64 runners; a short chaos run.
- `install.sh` for the release assets, and a Release workflow that builds and
  tests the Linux archives and can also be started by hand from GitHub
  (Actions → Release → Run workflow), which creates the tag itself once
  everything is built and tested. The macOS archives are built on a GitHub
  macOS runner (the default) or on a Mac (`cargo release --macos local`, which
  runs `cargo dist-macos`), chosen per release, and published with the rest.
- [`contrib/nginx.conf`](contrib/nginx.conf) and
  [`docs/proxies.md`](docs/proxies.md) for running behind nginx or a cloud load
  balancer.
- Opt-in file watching: `[watch]` (`enabled`, `paths`, `ignore`, `debounce_ms`,
  `interval_ms`, `max_files`) and `warden start --watch [--ignore-watch ...]
  [--watch-delay ...]` restart an app with the same gated rolling restart
  `warden restart` does when its files change (off by default; a polling scan,
  debounced and throttled; [`docs/watch.md`](docs/watch.md)). `warden list`
  has a `watching` column, `describe` a `watch` row and `status.watching` is in
  the protocol. `pm2-migrate` maps PM2's `watch`, `ignore_watch` and
  `watch_delay` to `[watch]` instead of reporting them as unsupported.
- `[static] html_max_age` and `warden serve --html-max-age N`: seconds browsers
  may reuse HTML pages (unset: revalidate on every load, as before; at most
  one year; `private` with `basic_auth`).
- `install.sh` options: `--gui`, `--version`, `--uninstall`, `--prefix`,
  `--dry-run`, `--modify-path`; musl/glibc detection, a PATH hint, and every
  download verified before anything is installed
  ([`docs/install.md`](docs/install.md), `scripts/test-install.sh`).
- Linux packages: `.deb` and `.rpm` for `warden` and `warden-gui` (amd64/arm64,
  x86_64/aarch64), built with nfpm in the Release workflow and tested by
  installing them in containers ([`docs/packages.md`](docs/packages.md)); a
  macOS `.dmg` with `Warden.app` next to the zip.
- The Release workflow can be run as a dry run (build and check everything,
  publish nothing) and writes the release notes from this file.
- `warden migrate-wattpm [dir|file]` imports a Platformatic Watt project
  (checked against wattpm 3.71.0). It writes one config and one 0600 env file
  per application, plus `MIGRATION-wattpm.md`, which lists every setting as
  mapped, approximated, unsupported or to check. A Gateway and applications
  without a runnable command are reported, not converted. Options: `--dry-run`,
  `--apps`, `--out`, `--prefix`, `--command <id>=<line>`, `--overwrite`,
  `--cutover overlap|new-port:<port>` (starts Warden's copy and never stops
  wattpm). [`docs/wattpm.md`](docs/wattpm.md): what Watt and Warden each do,
  the wattpm-to-warden command mapping, and who should and should not move.
- macOS: a supervisor killed with SIGKILL no longer leaves a second set of
  workers behind. The next start of the app stops the workers it left running
  (recorded as pid and start time under the state directory; a reused pid is
  never touched), and `warden doctor` lists them.
- GUI: icons on buttons, tabs, state badges, chips, dialogs and Settings;
  Settings scrolls in short windows. "Install command line tool" in Settings
  (and a one-time banner on the first run from Warden.app) links `warden` into
  `/usr/local/bin` (asking for the administrator password if needed, else
  `~/.local/bin` with a PATH hint), and can remove the link; on Linux it links
  into `~/.local/bin`.
- CI runs `tests/static_perf.rs` (with strace on Linux).
- macOS: the supervisor that stops a killed supervisor's workers keeps answering
  while it does (`status` shows `sweep`), obeys SIGTERM at once without starting
  workers, and `warden start` waits for the sweep instead of reporting a failed
  start. Oversized or non-regular files in the record directory are not read.
- GUI: a damaged `gui.json` is kept as `gui.json.bad` instead of overwritten;
  "Restart everything" shows which `warden` runs and aims it at the connected
  wardend, runs up to 15 minutes and offers `warden resurrect` if it fails;
  Cancel on the administrator prompt installs nothing; text contrast is 4.5:1 in
  every look; the machines menu scrolls; replies from before a host switch are
  dropped; the tunnel's ssh ends with the window on Linux.
- `migrate-wattpm` and `pm2-migrate` never write through a symlink, keep an
  existing env file unless `--overwrite`, and keep secrets out of reports, dry
  runs and error text (a `{SECRET}` in a shell command is read from the 0600
  env file; in other commands the config is written 0600).
- `[watchdog] port_lost` (10 s by default, `0` = off): a worker that listened
  on a TCP port and then holds no listening socket for that long while it runs
  is stopped and restarted like a crash, with the same backoff, so one that
  keeps losing its port ends FAILED. It covers an app that crashed under a
  wrapper that stays up (nodemon, a dev server waiting for a change) and a
  server closed by an error the app caught. A server that comes back within
  the time is left alone, and a worker that never listened is not watched.
  Read from outside, every 2 s: on Linux one netlink socket-diagnostics
  request lists the TCP listeners, so a worker still holding the socket it was
  seen with costs nothing more, however many connections the host has; only a
  worker whose sockets are gone has its process tree walked, and only a whole
  walk can conclude that nothing listens.
- Request health, on by default (`[metrics] requests = false` turns the
  counting off): the responses each worker sends, by status (2xx, 3xx, 4xx
  with 404 apart, 5xx), as a rate over 10 s, the last minute and a total,
  per worker and for the app, in `warden list` (`req/s`, `4xx/5xx`),
  `status` and `describe` (`requests`), `status --json`, the GUI (a Requests
  tile, the app list, the worker table when the window has room) and
  Prometheus (`warden_responses_total{class}`). Warden's static server
  counts its own (one atomic add per response); under Node the shim
  subscribes to Node's `http.server.response.finish` diagnostics channel,
  which wraps nothing (a hello-world server: 25.89 µs of CPU per request
  against 25.80 without). Not under Bun, whose `node:http` does not publish
  it. The counts travel with the heartbeat the workers already send.
- The app's ports as the kernel sees them (`status.ports`, `warden status`
  `connections`, the GUI's port chips, `warden_port_*`): open connections,
  connections waiting to be accepted against how many may wait, and the ones
  dropped at the listening sockets (a full accept queue, nearly always). Read
  only when a status is asked for, at most every 2 s, off the event loop: one
  netlink request for the listeners and one per port for its connections.

### Changed

- README is short and aimed at people installing and using Warden; the detail
  moved to `docs/` (commands, configuration, comparison, deploys, platforms,
  static serving and more). `cargo xtask bench` now writes its tables to
  `docs/benchmarks.md` (`--no-docs`; `--no-readme` still works).
- Static serving: big files go out in 1 MiB `sendfile` pieces, so a download no
  longer holds up other requests on its worker (a 1 KB request next to a 10 MB
  download: 5.4 ms to 0.34 ms at p50); bodies of 1.25 MiB and more use
  `TCP_CORK` (21-38% less CPU per 2-100 MB response); 404, 401, 405 and listing
  responses are one packet, and HEAD to them no longer carries a body; a new
  connection costs two fewer syscalls. On macOS `SO_NOSIGPIPE` is set once per
  connection and head plus body go out in one `sendfile` (not yet run on a
  Mac). Measured on a loaded 2-CPU VM: [`docs/benchmarks.md`](docs/benchmarks.md).
- Static serving, the worker's own cost per request: the request head is
  parsed in place (no allocation), the 10 s and 15 s waits for a request are
  enforced by one task ticking once a second instead of a timer per request
  (so they hold to within about a second), and a new connection whose request
  has arrived and whose answer is cached is answered from the accept loop
  (`accept`, `recv`, `send`, `close`; no task, no epoll registration). User-space
  CPU per request is down by a fifth on a kept-alive connection and by 60 % on
  a new one; a cached 1 KB page on a new connection costs 13.1 µs of server CPU
  instead of 16.8 (nginx 19.4), 4 system calls instead of 9; the exit summary
  says how many requests the accept loop answered. Keep-alive throughput
  is bound by the kernel and does not change. Measured on a loaded 2-CPU VM, with
  a note on why a single pinned client misreads it:
  [`docs/benchmarks.md`](docs/benchmarks.md).
- Static serving keeps no copy of files by default: `[static] cache_size`
  is `0` (it was 16 MB per worker), as in nginx; files can be any size, an edit
  shows at once, and memory does not grow with the site. The response cache stays as an option
  (`cache_size = "16MB"`), and saves a tenth to a fifth of the CPU on small
  files. To make up for it the first request of every new connection, hit or
  not, is answered from the accept loop (8 system calls for a 100 KB file
  instead of 10, no epoll registration), and responses are built without
  allocation (user-space CPU per request 3.5 to 2.1 µs on a new
  connection). Server CPU per request without any cache, 1 KB file: 7.8 µs
  kept alive and 14.2 on a new connection (nginx 14.3 and 19.2; nginx with
  `open_file_cache` 9.3 and 17.9); 100 KB on a new connection 24.0 instead of
  28.4 (nginx 25.5). The exit summary line "N requests answered in the accept
  loop" is printed with or without a cache. The benchmark scenario
  `warden-nocache` is now `warden` and `warden` is `warden-cache`:
  [`docs/benchmarks.md`](docs/benchmarks.md).
- The static server's source is split by concern (`src/static_server/`: the
  accept loop, the request head, the output side, the handler, the response
  heads, the cache glue), with the same behaviour apart from details found
  in review: a response the socket takes only in part keeps its file open
  (a worker out of descriptors could cut it short), the accept loop contains
  a panicking handler to its connection as a task would, a conditional
  request with an absurd date no longer overflows the date arithmetic, a
  worker without `openat2` does not try the accept-loop path twice, and the
  generated responses (errors, 301, 416, listings, 401) are made by one
  function, so their header lines can come in another order (the 401 now says
  `charset=utf-8`).
- Static serving no longer looks for a `.br` / `.gz` sibling of a file that is
  compressed already (`[static] precompressed_skip`: by default images, fonts,
  audio, video, archives, PDF and office documents; a list replaces it, `[]`
  looks up every file; a sibling of such a file would save nothing). Browsers
  send `Accept-Encoding: gzip, br` with every request, and each lookup that
  fails costs a system call: a 1 KB PNG took 11.9 µs of server CPU, and now
  takes 7.9. Text files without siblings still pay the two lookups (11.9 µs
  against 7.8); a site with no precompressed files at all can set
  `precompressed = false`.
- Static serving compresses files in the background (`[static] compress`, on by
  default). The first request for a text file with no compressed copy is
  answered at once with the file as it is and queues a job; a few processes at
  the lowest CPU and disk priority (`compress_jobs`, one per core by default)
  make `.br` and `.gz` copies, and the next request is answered from them. The
  copies are kept in a private folder of their own (`compress_dir`, by default
  in Warden's state directory, never in the folder that is served) and belong
  to one version of the file (device, inode, size, modification and change
  times): an edited or replaced file is never answered from an old copy, there
  is nothing to hash on the request path, and old copies are removed when the
  new one is made. A copy is kept only when it is smaller than the file, files
  outside `compress_min_file`..`compress_max_file` (1 KB to 8 MB) and the
  formats of `precompressed_skip` are left alone, and the folder is held to
  `compress_dir_size` (256 MB, oldest first). `file.br` / `file.gz` siblings
  from your build are still used, and files that have one are not compressed
  again. The worker stays one thread and a request that takes no copy costs
  what it did (about 8.3 µs of server CPU for a 1 KB file with and without); a
  request answered from a copy opens one more file (13.3 µs against 11.9 for
  a 20 KB stylesheet, 2.8 KB sent instead of 20 KB). With a compressor pinned to
  the core of a saturated worker, throughput fell 3 to 4 % and the 99th
  percentile latency did not move. Adds the `brotli` crate (pure Rust).
- `Accept-Encoding` is read the way RFC 9110 says: `br;q=0` refuses brotli (the
  precompressed `.br` file used to be sent anyway) and encoding names match in
  any letter case. The docs have a section on compression: precompressed
  files only, what a missing one means, and how to make them.
- `Status.watching` (the `watching` column) says whether a watcher is running
  now, not only that `[watch]` is enabled: it reads `disabled` for a stopped
  app or a watcher that died (`describe` says "set, but not running").
- All `unsafe` code is in `src/sys.rs` and `src/sys/darwin.rs`: the two
  `pre_exec` blocks moved behind safe wrappers.
- Two load-flaky integration tests (hot-standby takeover, OOM attribution of
  two SIGKILL deaths) are deterministic: they were races in the tests.
- `warden start --watch` is no longer rejected ("Warden is for production").
  `warden serve --watch` is an error: the file server reads from disk.
- Static serving follows RFC 9110 on dates and validators, so a browser or a
  cache is never told an old copy is current
  ([`docs/static-serving.md`](docs/static-serving.md#dates-and-validators)):
  every response has a `Date` (there was none); the `ETag` includes the nanoseconds of the modification time,
  so two edits of the same size within a second are two versions (files with
  sub-second times get a new tag once, and browsers download them once more
  after the upgrade); `Last-Modified` is sent once the file's second is over
  and never later than `Date` (a file stamped in the future has none); an
  `If-Modified-Since` later than the server's clock is ignored; `If-Match` and
  `If-Unmodified-Since` get a 412 for another version; `If-Range` resumes a
  range only for the same version (a whole file otherwise); a 304 says `Date`,
  `ETag`, `Last-Modified`, `Cache-Control` and `Vary`; conditions apply only
  where the answer would be a 200. Cost: within 1.3 % without the response
  cache; with it, +2.9 % for a 20 KB body and +4.2 % for 48 KB, the head of a
  memfd body being sent on its own (bodies under 24 KB, was 8 KB, now stay in
  memory, where that is cheaper; [`docs/benchmarks.md`](docs/benchmarks.md)).
- `[static] cache_max_age` defaults to `0`: files that are neither HTML nor
  fingerprinted get `Cache-Control: no-cache` (a 304 while unchanged) instead
  of `public, max-age=3600`, so a deploy reaches browsers at once instead of
  up to an hour later. `cache_max_age = 3600` gives the old behaviour.
- With `[static] basic_auth`, files are sent `private` (they were `public`,
  which allows a shared cache such as a CDN to keep a file that needed a
  password and give it to anyone), fingerprinted ones included.
- Fingerprinted names (cached a year, immutable) are recognised more strictly:
  lowercase hex (webpack, Parcel, Angular, Next), uppercase base32 (esbuild),
  or mixed case with a digit before a letter (Vite, Rollup). `report2024.pdf`,
  `background1.jpg` and `MyPhoto2024.png` used to count, so a new version
  under the same name could stay unseen in browsers for a year.

### Notes

- macOS GUI notarization is not done (it needs an Apple Developer account).
- `SECURITY.md` says how to report a vulnerability.
