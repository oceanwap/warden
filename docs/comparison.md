# Warden compared with PM2, systemd and Watt

Warden replaces "4 systemd units / PM2 + an nginx upstream list" with one
service, one config file and one port. It is inspired by
[Platformatic Watt](https://github.com/platformatic/platformatic)'s worker
model, but it is a single binary of about 5 MB that embeds no JavaScript runtime.

At a glance, against PM2 (from the benchmark notes in
[`benchmarks.md`](benchmarks.md)):

- **Request path.** Warden never sits on it: the workers own the port and
  the kernel balances with `SO_REUSEPORT`. PM2's cluster mode passes every
  connection through its daemon. PM2 has no cluster mode for Bun, so it runs
  fork-mode instances that share the port with `reusePort`.
- **Reloads.** Warden replaces workers one at a time through health gates,
  with a canary and rollback. PM2's `reload` is graceful only in cluster
  mode (Node); for Bun apps it runs fork mode, where reload is a restart.
- **Long-lived connections.** Warden ends WebSockets and SSE streams cleanly
  during a restart (close code 1001, end of stream); under PM2 they are cut.
- **Manager cost.** Warden's supervisor is about 8 MB RSS; PM2's daemon about
  65 MB.

Watt runs the app inside its own runtime, where Warden runs plain processes.

The numbers (requests per second, crash recovery, rolling
restarts, memory, `list` latency) are in [`benchmarks.md`](benchmarks.md), with
the method in [`../bench/README.md`](../bench/README.md).

## Warden and systemd

Warden does not replace systemd; it replaces the pile of units that systemd
would otherwise need for N copies of an app. systemd supervises Warden,
Warden supervises the workers.

- Under systemd each app is its own unit (cgroup, limits, `journalctl -u
  warden@api`). [`contrib/warden@.service`](../contrib/warden@.service) is
  `Type=notify`, so the unit counts as started only once every worker is
  listening, and `ExecReload=warden safe-reload` makes `systemctl reload
  warden@api` a gated rolling reload.
- The kernel spreads connections across the workers, so nginx has one upstream
  address instead of a list of ports.
- `warden startup` installs the units for you, and wardend restarts
  supervisors that `warden start` launched outside a unit.

Details: [`production.md`](production.md).

## Warden and PM2

If you know PM2 you know most of it: `warden` in place of `pm2`. `warden help`
has everything; the differences are in the right column. The full command
list is in [`commands.md`](commands.md).

| PM2 | Warden | Difference |
|---|---|---|
| `pm2 start app.js -i 4 --name api` | `warden start app.js -i 4 --name api` | Waits until the app is up and says so if it isn't (`--no-wait` returns at once). An app that can't start (every worker crashes before one is ready: a syntax error, a missing module, a port in use) fails at once with exit 1, its last error output and a hint; its workers are stopped and it stays listed as `errored`, as PM2 leaves it, until `warden start` again. Any program or command line works, as with PM2 |
| `pm2 list`, `pm2 jlist` | `warden list`, `warden list --json` | The same boxed table with an `id` column, ~2 ms instead of ~140-160 ms. One row per worker; colors on a terminal (`NO_COLOR` turns them off) |
| `pm2 restart 0`, `pm2 stop 1 2` | `warden restart 0`, `warden stop 1,2` | Every command that takes an app takes its id from `warden list`, a name, a namespace or `all`, one or several: `warden start 0,1,2`, `warden stop 0-3`, `warden restart api web:2` (`:2`: one worker). Ids are numbered the first time Warden sees an app (alphabetically for the first batch, then in creation order), kept in `ids.json` in the state directory, and never change; only `warden delete` frees one. Use names in scripts |
| | `warden ports` | The ports and Unix sockets each app listens on, read from the OS (wrapper processes like `npm run start` included): which interfaces they are on (`all interfaces`, `localhost only`) and, for a TCP port, an `http url` to try (a `curl --unix-socket` hint for a Unix socket). `warden list` has a `ports` column, `describe` and `status` a `ports` row, `--json` the same data. PM2 cannot tell you this: it knows only what the app printed |
| `pm2 describe api` | `warden describe api` | A key \| value box and the workers' box, like PM2's; also the last exits and the last rollout. `status <app>` and `doctor` are boxes too. Like PM2, `start`, `stop`, `restart`, `reload`, `delete`, `scale`, `reset`, `resurrect` and `serve` print the app table when they finish, on a terminal (set `WARDEN_TABLE=1` to get it in a log; a script's output is unchanged) |
| `pm2 reload api` | `warden restart api`, `warden reload api` | One worker at a time through health gates; a failure stops and rolls back. `restart --hard` is PM2's `restart` |
| | `warden deploy api` | Preflight, canary with soak, then the rest, with rollback |
| `pm2 logs api` | `warden logs api` | `--history --grep --since 2h --json` over rotated and gzipped files (every worker's with `per_worker_files`), pipe-friendly |
| `pm2 flush api` | `warden flush api` | The same: empties the in-memory buffer and the current log files (Warden's, each worker's out and err file), lists them, keeps rotated ones; safe while workers write |
| `pm2 start app.js --watch` | `warden start app.js --watch` | Opt-in, and a gated rolling restart instead of a plain restart: a version that fails its health checks is rolled back where workers can overlap (see `watch.md`). `ignore_watch` and `watch_delay` map to `--ignore-watch` and `--watch-delay`; see [`watch.md`](watch.md) |
| `pm2 serve dist 8080` | `warden serve dist 8080` | A static server as fast as nginx, faster on small files (see [`benchmarks.md`](benchmarks.md) and [`static-serving.md`](static-serving.md)) |
| `pm2 save`, `resurrect`, `startup` | `warden save`, `resurrect`, `startup` | One systemd unit per app (root or `--user`), a launchd job on macOS; see [Surviving reboots and crashes](production.md#surviving-reboots-and-crashes) |
| `pm2 update` | `warden update` | Saves, kills and resurrects: restarts every supervisor and wardend from the binary on disk, which is what picks up a rebuild or an upgrade |
| `pm2 monit` | `warden top`, `warden events` | `events`: every worker, rollout and supervisor event as it happens (`--json` for scripts, `--logs` for output) |
| PM2's daemon | wardend (no command: always on) | Live events for every app on one socket, and restarts dead supervisors; a supervisor starts it again if it dies. `warden start` and `warden resurrect` start it and `warden kill` stops it (`WARDEN_NO_DAEMON=1` never starts it). Apps never depend on it; killing it stops nothing. See [`wardend.md`](wardend.md) |
| | `warden doctor` | Environment problems (kernel settings, limits, ports, permissions), each with its fix |

PM2's `pm_id` and `name` environment variables are not set: use
`WARDEN_WORKER_ID` and `WARDEN_APP` (see
[`configuration.md`](configuration.md#environment-variables)).

Found while benchmarking PM2
([`benchmarks.md`](benchmarks.md#findings-about-the-other-managers)): its
cluster mode can hang a connection it handed to a worker that is exiting;
`pm2 --version` starts a daemon; and `pm2 jlist` includes the full environment
of every app, secrets included, where `warden env` hides values unless
`--show-secrets`.

### Moving from PM2

```sh
warden pm2-migrate --dry-run                   # what it would write, from `pm2 jlist`
warden pm2-migrate                             # <app>.toml + <app>.env (0600) + MIGRATION.md
warden pm2-migrate --cutover same-port         # PM2 stops each app, Warden starts it; PM2 is restored on failure
warden pm2-migrate --finalize                  # remove the migrated apps from PM2, `warden save`
```

It reads the running daemon (`--from dump` for `pm2 save`'s file, or an
ecosystem file with `--env production`). Env values go to a 0600 file, never
into the config, and only the variables the app was given: `pm2 jlist` also
carries the whole shell of whoever ran `pm2 start`, which is left out and
listed by name in `MIGRATION.md` for review. PM2's defaults carry over where
apps depend on them (SIGINT to stop, `NODE_APP_INSTANCE`). `--cutover
overlap` runs both side by side first (apps that share their port with
`reusePort`); `new-port:<p>` starts Warden on another port for you to switch
the proxy.

All options: `--from jlist|dump|<ecosystem file>`, `--env <name>`,
`--apps a,b`, `--out <dir>`, `--dry-run`, `--mode process|worker`,
`--overwrite`, `--cutover overlap|same-port|new-port:<port>` (switch with
rollback) and `--finalize` (remove them from PM2).

## Warden and Watt

Warden is inspired by Watt's worker model, but it is a single binary of about 5 MB
that embeds no JavaScript runtime. Watt runs the app inside its own runtime:
in the benchmark that was about 4.5 times the memory of 4 processes for the
Node app (PSS 478 vs 106 MB) and 2-3 % idle CPU. Warden's worker mode
(experimental, Bun only) is the thread variant: about half the memory of
process mode, but one crash takes all workers down. The measurements, for
the same apps on the same machine, are in [`benchmarks.md`](benchmarks.md).

Watt is an application server, not only a process manager: it composes several
applications into one runtime with one entry point and calls between them in
the process. Warden supervises independent programs, so they overlap on running
N copies of a Node.js app, restarting them and scaling, and not on composition.
The commands that mean the same:

| wattpm | Warden |
|---|---|
| `wattpm start [root]` | `warden start <app>` (one app, in the background) |
| `wattpm stop` (the whole runtime) | `warden stop <app>`, `warden kill` |
| `wattpm restart` | `warden restart <app>`: one worker at a time through health gates |
| `wattpm reload` (stops, then starts again) | `warden reload <app>`, `warden deploy <app>`: gated, no downtime |
| `wattpm ps`, `applications` | `warden list`, `warden describe <app>` |
| `wattpm logs`, `env`, `config` | `warden logs`, `env`, `config` |
| `wattpm build`, `dev`, `inject`, `scheduler`, `pprof` | no equivalent |

Where the two overlap, who should move and who should not, and what is lost
(the Gateway, the `plt.local` mesh, framework integration), is in
[`wattpm.md`](wattpm.md), with the full command mapping.

### Moving from wattpm

```sh
warden migrate-wattpm --dry-run                 # what it would write, for the project in this directory
warden migrate-wattpm ~/shop                    # <app>.toml + <app>.env (0600) + MIGRATION-wattpm.md
warden migrate-wattpm ~/shop --cutover overlap  # start Warden's copy next to the running Watt
```

One Warden app per Watt application, with the environment in a 0600 file and a
report that lists every setting as mapped, approximated, unsupported or to
check. A Gateway is not converted, an internal application gets no port, and
the `plt.local` calls between applications are listed for you to replace; see
[`wattpm.md`](wattpm.md#warden-migrate-wattpm). Unlike `pm2-migrate` it never
stops the old side: `wattpm stop` would take the whole runtime down.
