# Production setup and surviving reboots

Warden runs under systemd on Linux (one unit per app) and under launchd on
macOS. This page covers the unit files, the sysctl setting, nginx, and
`warden save` / `warden startup`, which bring the apps back after a reboot
or a crash.

## Production setup (systemd)

- [`contrib/warden@.service`](../contrib/warden@.service): `Type=notify`, so the unit
  counts as started only once every worker is listening. It uses
  `ExecReload=warden safe-reload` and `KillMode=mixed`, and runs as an
  unprivileged user. Warden never needs root. Each instance gets its own
  `RuntimeDirectory=warden/%i`; set `[control] socket =
  "/run/warden/<name>/control.sock"`.
- [`contrib/99-warden.conf`](../contrib/99-warden.conf): sets
  `net.ipv4.tcp_migrate_req = 1`. Without it, a few connections queued on a
  closing listener get reset during reloads. Measured: 9–15 per worker-mode
  reload, 0 with the setting. Warden logs a warning at startup when it's off.
- nginx: [`contrib/nginx.conf`](../contrib/nginx.conf), a commented site file:
  one upstream address (the kernel does the balancing) with keep-alive,
  retries for idempotent requests only, WebSockets and SSE, X-Forwarded-*
  headers, a `/health` for a load balancer. A test restarts the workers
  under load through it without a failed request. Load balancers (AWS
  ALB/NLB, GCP, Cloudflare), no proxy at all, and the timeouts that must
  agree with Warden's drain: [`proxies.md`](proxies.md).
- Logs go to stdout in journald format (timestamps dropped, priority prefixes
  added). Use `journalctl -u warden@api`.
- Metrics: set `[metrics] listen = "127.0.0.1:9464"` to get Prometheus text at `/metrics`.
- `sudo warden startup` installs the unit (with this binary's path, running
  as root like `sudo warden start`), the sysctl file and
  [`contrib/wardend.service`](../contrib/wardend.service) for you: see the next
  section.

The CLI talks to the running supervisor over a Unix socket with mode 0600.
That socket's directory also holds the shim every worker preloads and the
per-worker health sockets. Warden refuses to use the directory unless it
owns it, it is not a symlink, and no other user can write to it.

## Surviving reboots and crashes

`warden save` records which apps run, with their worker counts. `warden
startup` makes them come back after a reboot, and keeps them up after a
crash, with the service manager the host has:

| Host | `warden startup` installs | After a reboot | After a crash |
|---|---|---|---|
| Linux, root (`sudo warden startup`) | `warden@.service` (enabled for every saved app), `wardend.service`, the sysctl file | systemd starts each app's unit | systemd restarts the supervisor |
| Linux, a user (`warden startup`, or `--user` as root) | the same two units in `~/.config/systemd/user`, and `loginctl enable-linger` so they run without a login | your user manager starts at boot and starts the units | your user manager restarts the supervisor |
| macOS | a launchd job: `~/Library/LaunchAgents/io.github.oceanwap.warden.daemon.plist` (as root `/Library/LaunchDaemons/`: at boot, no login needed) running `warden wardend --resurrect` | launchd starts wardend, which starts the saved apps | wardend restarts dead supervisors; launchd restarts wardend |
| Containers, other init systems | nothing (it says what to run instead) | `warden resurrect` in the entrypoint, or `warden wardend --resurrect` as the entrypoint | with `warden wardend --resurrect`, wardend restarts dead supervisors |

- Under systemd each app is its own unit (cgroup, limits, `journalctl -u
  warden@api`), and wardend never starts apps there: it would start them
  twice. `wardend.service` adds `warden events` and restarts supervisors
  that `warden start` launched outside a unit.
- The apps come back as the user that ran them: system units run as root,
  like the `sudo warden start` that started the apps (with root's saved
  worker counts and runtime directory, so `warden list` and wardend find
  them). For another user, `systemctl edit warden@<app>` and set `User=`
  and `Group=`; the app's files must then be theirs.
- The unit reads `<config dir>/<app>.toml` (`/etc/warden` for root,
  `~/.config/warden` for a user); `startup` says how to link a config that
  lives elsewhere.
- User units and the launchd job carry your `PATH`, so `bun` and `node`
  resolve, and Warden's directory variables (`WARDEN_HOME`, ...).
- Lingering needs root on some systems: `startup` then prints the exact
  `sudo loginctl enable-linger <you>`. Until then the apps start when you log
  in and stop when you log out.
- macOS LaunchAgents start at login to the desktop. On a Mac you only reach
  over SSH, use `sudo warden startup` (a LaunchDaemon).
- `warden unstartup` removes all of it; running apps keep running. Under
  systemd, `warden@.service` stays while apps still run under it (a running
  unit whose file is removed is left half configured), disabled: `warden
  kill`, then `warden unstartup` again removes it. `warden kill` stops
  wardend's unit or job too, so it stays down until the next boot.
- CI checks all of this against real systemd (system and user units, a
  restart of the user manager) and launchd (LaunchAgent, LaunchDaemon):
  `.github/workflows/service-managers.yml`.
- In a container, run `warden wardend --resurrect` under an init
  (`docker run --init`, tini) that reaps orphaned processes.
- `--resurrect` runs once per boot: when launchd restarts a crashed wardend,
  apps you stopped since boot stay stopped (`warden resurrect` starts them).

## Upgrading

`warden update` saves the running apps and moves every supervisor and
wardend to the `warden` binary on disk (like `pm2 update`). A supervisor keeps
the code it started with, so this is what picks up a rebuild or an upgrade.

The apps keep serving: each app's keeper (the process `warden start` runs as,
and the one wardend and systemd watch) re-executes itself from the new binary
with the same pid, and the supervisor it starts takes the same workers back.
Nothing restarts but Warden's own processes; the workers keep their pids and
connections, and what they print meanwhile is kept for the log. An app
without a keeper (`[restart] keep_workers_on_crash = false`), or whose
supervisor is older than this way of updating, is stopped and started again
instead, as before: it stops for a few seconds. wardend restarts too, which
no app depends on. It asks first on a terminal (`--yes` skips the question).

The workers keep running the code they started with, the shim included: a
`warden reload` moves them to it, through the health gates.

## See also

- [`wardend.md`](wardend.md): the host daemon that the units above start.
- [`proxies.md`](proxies.md): nginx, load balancers and Cloudflare in front of Warden.
- [`troubleshooting.md`](troubleshooting.md#reboots-crashes-and-wardend): reboots, crashes and wardend.
