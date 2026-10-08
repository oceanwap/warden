# Command reference

`warden --help` is the authority and is always current; this page is the same
information with room to breathe. If you know PM2, `warden` in place of `pm2`
gets you most of the way: see [`comparison.md`](comparison.md) for the
command-by-command table.

```
warden <COMMAND> [TARGET] [OPTIONS]
```

## Targets

`TARGET` is an app name, its id (the first column of `warden list`), a
namespace, `all`, or `app:N` for one worker. Several at once, with commas or
spaces; ids can be ranges:

```sh
warden start 0,1,2
warden stop 0-3
warden restart api web:2        # all of api, and worker 2 of web
```

With `-c <config>`, commands act on that one app and `TARGET` may be a worker
number (`0,1`, `0-2`). Ids are numbered the first time Warden sees an app
(alphabetically for the first batch, then in creation order), kept in
`ids.json` in the state directory, and never change; only `warden delete`
frees one. Use names in scripts.

## Starting an app

`warden start <app|id|config.toml|script>` starts an app and waits for it. If
every worker crashes before one is ready (a syntax error, a missing module, a
port in use), it exits 1 with the app's errors and a hint, and leaves the app
stopped, listed as `errored`, until you `warden start` again. `--no-wait`
returns as soon as the start has begun.

A script, a program or a command line works, as with PM2; Warden writes a
config for it:

```sh
warden start server.js --name api -i 4 --port 3000
warden start worker.py --name queue                       # interpreter picked by extension
warden start ./bin/server --name go-api -- --flag         # any executable
warden start npm --name web -- start                      # a program on PATH
warden start "python3 -m http.server 8000" --name files   # a command line
warden start -c warden.toml                               # an app that already has a config
```

Start options:

| Option | Meaning |
|---|---|
| `--name <NAME>` | The app's name |
| `-i, --instances <N\|max\|max-1>` | Number of workers |
| `--port <PORT>` | The app's port |
| `--namespace <NS>` | Group, for `warden restart <ns>` |
| `--cwd <DIR>` | Working directory |
| `--interpreter <bun\|node\|python3\|bash\|none\|...>` | Override the interpreter |
| `--interpreter-args "<args>"` (also `--node-args`) | Arguments for the interpreter |
| `--env KEY=VALUE` | Environment variable |
| `--max-memory-restart <300M>` | Recycle a worker above this memory |
| `--cron "<m h dom mon dow>"` | Rolling restart on a schedule |
| `--watch` | Rolling restart when the app's files change (off by default), see [`watch.md`](watch.md) |
| `--ignore-watch "<name,glob,...>"` | Names and patterns `--watch` ignores (replaces the default list); needs `--watch` |
| `--watch-delay <4\|4000ms>` | How long files must stay unchanged before the restart (seconds, or `ms`); needs `--watch` |
| `--no-autorestart` | Do not restart crashed workers |
| `--kill-signal SIGINT` | Signal that asks a worker to stop |
| `--kill-timeout <ms>` | Time to exit before SIGKILL |
| `--restart-delay <ms>` | Delay before a restart |
| `--max-restarts <N>` | Restarts allowed before the worker is FAILED |
| `--stop-exit-codes 0,1` | Exit codes that mean "done, don't restart" |
| `--wait-ready` | Ready on `process.send('ready')` instead of listening |
| `--listen-timeout <ms>` | Time to start listening |
| `--no-shim` | Do not inject Warden's shim |
| `-o, --output <file>`, `-e, --error <file>`, `-l, --log <file>` | Log files |
| `--time`, `--merge-logs` | PM2's log options |
| `-- <args for the app>` | Everything after `--` goes to the app |

With no app, `warden start -c <config>` runs the supervisor in the foreground
for that config (what systemd runs; also `warden run`).

## Apps

| Command | What it does |
|---|---|
| `start <app\|id\|config.toml\|script>` | Start an app (see above) |
| `list` | Every app and worker as a table with ids (also `ls`, `ps`, `status`); `--json` |
| `describe <app>` | Config, paths (including `cwd`, the folder the workers run in), restart policy, workers, last exits, last rollout (also `show`) |
| `ports [app]` | The ports and Unix sockets each app listens on, read from the OS, with where they are reachable (all interfaces / localhost only) and a URL; `--json` |
| `restart <target>` | Replace the workers one at a time through the health gates (no downtime); `--hard` stops them all, then starts them (like PM2) |
| `reload <target>` | Like `restart`, but re-reads the config first (new code or settings); a failure rolls back |
| `deploy <target>` | The safest reload: preflight, canary with soak, then the rest, with rollback (also `safe-reload`). See [`deploys.md`](deploys.md) |
| `stop <target>` | Stop the workers; the app stays listed (`start` brings it back) |
| `delete <target>` | Stop the app for good and move its config to `deleted/` |
| `scale <app> <N>` | Set the number of workers (`N`, `+N` or `-N`) |
| `logs [target]` | Recent lines, then follow on a terminal (see below) |
| `search <text> [target]` | Search all of an app's logs (= `logs --history --grep`) |
| `flush [target]` | Empty the log buffer and the current log files (`[logging] file`, out and err files, every worker's); rotated files (`.1`, `.gz`) are kept |
| `env <app>` | The environment a worker starts with: `env_file`, `env`, then Warden's variables (`app:N` for worker N; values hidden unless `--show-secrets`) |
| `reset <target>` | Zero restart counters and retry FAILED workers now |
| `signal <SIG> <target>` | Send a signal to the workers (`SIGUSR2`, `USR2`, `12`) |
| `top` | Live view of every app (also `monit`) |
| `expose <host>... --app <app>` | Send hostnames to the app through nginx: writes the app's nginx site file, checks it with `nginx -t` (a failure puts the old file back), reloads nginx and records the hostnames in the app's `[expose]`. HTTPS with HTTP/2 through `--acme <email>` (nginx's ACME module gets and renews a Let's Encrypt certificate) or `--cert`/`--key` (a certbot certificate in `/etc/letsencrypt/live/<host>/` is found by itself), `--websocket PATH`, `--sse PATH`, `--remove`, `--no-reload`, `--dry-run`. See [`proxies.md`](proxies.md#warden-expose) |
| `serve [dir] [port]` | Serve static files (default: `.` on 8080) with Warden's built-in server, like `pm2 serve`: `--name`, `--spa`, `--listing`, `-i N`, `--html-max-age SECONDS`, `--basic-auth user:pass` (or `--basic-auth-username` / `--basic-auth-password`). See [`static-serving.md`](static-serving.md) |

`reload`, `deploy` and `restart app:N` wait for the rollout, print its
progress and exit 1 if it failed (or 2 if Warden is unreachable), so they fit
`ExecReload=` and deploy scripts.

Like PM2, `start`, `stop`, `restart`, `reload`, `delete`, `scale`, `reset`,
`resurrect` and `serve` print the app table when they finish, on a terminal
(set `WARDEN_TABLE=1` to get it in a log; a script's output is unchanged).

### Logs

```sh
warden logs api                          # recent lines, then follow on a terminal
warden logs api --history --grep error --since 2h
```

Options: `--lines N`, `--err` or `--out`, `--events`, `--nostream`, `-f`.
`--worker N` picks one worker; `s1`, `s2`… one hot standby; `standby` all of them.
`--history` reads the log files (rotated and `.gz` too, every worker's with
`per_worker_files`) or journald; `--lines N` there is the last N of each file.
Filters: `--grep TEXT`, `--exclude TEXT`, `--ignore-case`, `--since 2h`,
`--until 2026-09-30T12:00`, `--level warn`, and `--json` (one object per line).

## Saving, surviving reboots and upgrading

| Command | What it does |
|---|---|
| `save` | Remember the running apps, worker counts and stopped state |
| `resurrect` | Start what `save` remembered (and wardend, if it is not running), the apps at the same time but no more than one per CPU core at once (see [Parallel starts](#parallel-starts)) |
| `update` | Restart every supervisor and wardend from the `warden` binary on disk (save, kill, resurrect: like `pm2 update`). A supervisor keeps the code it started with, so this is what picks up a rebuild or an upgrade. The apps stop for a few seconds; asks first on a terminal (`--yes`) |
| `startup` | Bring the saved apps and wardend back after a reboot or a crash: systemd units (root: system units; a user or `--user`: your own, with lingering), a launchd job on macOS. Without a service manager it says what to run at boot instead; `--user` or `--system` |
| `unstartup` | Remove what `startup` installed (apps keep running); `--user` or `--system` |
| `kill [target]` | Stop every app's supervisor, and wardend when it is every one (asks first on a terminal; `--yes`) |
| `pm2-migrate` | Import PM2's apps: a config, a 0600 `.env` file and a `MIGRATION.md` report per app. See [`comparison.md`](comparison.md#moving-from-pm2) |
| `migrate-wattpm [dir\|file]` | Import a Platformatic Watt project: a config and a 0600 `.env` file per application and a `MIGRATION-wattpm.md` report of what Warden cannot express. `--dry-run`, `--cutover overlap\|new-port:<port>`. See [`wattpm.md`](wattpm.md) |

What `startup` installs on each OS, and how apps come back:
[`production.md`](production.md).

### Parallel starts

`resurrect`, `update` and `start a b c` start apps at the same time, but no
more than one per CPU core at once. `--parallel N` (`-j N`) changes that limit;
`--parallel all` removes it; `$WARDEN_PARALLEL` sets the default.
`--parallel` only applies to `start`, `resurrect` and `update`; wardend's own
`--resurrect` follows the environment variable.

```sh
warden resurrect -j 2
WARDEN_PARALLEL=1 warden update --yes
```

## wardend

wardend is the host daemon, always on: `start` and `resurrect` start it, a
crash or `kill -9` brings it back, a `kill` of everything stops it;
`WARDEN_NO_DAEMON=1` never starts it. Apps never depend on it. Alert rules live
in `<config dir>/wardend.toml` and are read again whenever the file changes.
There is no `warden daemon` command. See [`wardend.md`](wardend.md).

| Command | What it does |
|---|---|
| `events [target]` | Live events, one line each: workers, rollouts, supervisors. `--json` (NDJSON), `--logs` (log lines too), `--interval MS`. From wardend when it runs, else from the apps' sockets |

`warden wardend [--background|--resurrect|status]` exists, hidden from
`--help`, as the entry point for service units, launchd, the supervisors and
the GUI. You should not need to run it by hand.

## Supervisor and diagnostics

| Command | What it does |
|---|---|
| `start` (no app) | Run the supervisor in the foreground for `-c` (what systemd runs; also `run`) |
| `shutdown` | Stop the workers and exit the supervisor |
| `config <app>` | Effective config as JSON (values hidden unless `--show-secrets`) |
| `log-level [target] [debug\|info\|warn\|error]` | Show or change it at runtime |
| `check` | Validate the config file and exit. `-c wardend.toml` validates the alert rules |
| `doctor` | Check this host for the problems Warden knows about, with a fix for each; `--json`. Checks `wardend.toml` too |
| `version` | Print the version |
| `gui-install` | Install the GUI for this `warden`'s version (macOS: `Warden.app`; Linux: `warden-gui` next to `warden`, a menu entry and an icon). `--dry-run`, `--uninstall`, `--version`, `--dir`, `--app-dir`. It runs the release's `install.sh --gui-only`, checked against `SHA256SUMS`; for a `warden` from a Linux package it points at the `warden-gui` package instead |

## Options

| Option | Meaning |
|---|---|
| `-c, --config <PATH>` | One app's config. Default: `$WARDEN_CONFIG`; else every app in `$WARDEN_HOME`, `/etc/warden` (root) or `~/.config/warden` |
| `-s, --socket <PATH>` | One app's control socket |
| `--json` | JSON output for `list`, `status` and `describe` |
| `--no-wait` | Return as soon as a start, reload or restart has begun, without waiting for workers to be ready |
| `-j, --parallel <N\|all>` | How many apps `start`, `resurrect` and `update` start at once |
| `-y, --yes` | Do not ask first (`update`, `kill`; also `pm2-migrate`) |
| `-h, --help` | Show help |

## Example

```
$ warden status travelerwe-api
 travelerwe-api
┌───────────┬──────────────────────────────────────────────┐
│ status    │ online                                       │
│ id        │ 0                                            │
│ namespace │ default                                      │
│ mode      │ process                                      │
│ workers   │ 4 configured, 4 ready                        │
│ pid       │ 8139                                         │
│ uptime    │ 27s                                          │
│ release   │ /srv/apps/travelerwe/api/releases/2026-09-30 │
│ memory    │ 4.0 MB (supervisor)                          │
└───────────┴──────────────────────────────────────────────┘
 Workers
┌────────┬─────────┬──────┬────────┬───┬──────┬─────────┬──────────┬────────┬───────────┐
│ worker │ status  │ pid  │ uptime │ ↺ │ cpu  │ mem     │ loop p99 │ health │ last exit │
├────────┼─────────┼──────┼────────┼───┼──────┼─────────┼──────────┼────────┼───────────┤
│ 1      │ RUNNING │ 8172 │ 25s    │ 1 │ 0.0% │ 40.1 MB │ 0.21ms   │ ok     │ -         │
│ 2      │ RUNNING │ 8180 │ 13s    │ 1 │ 0.0% │ 40.1 MB │ 0.18ms   │ ok     │ -         │
│ 3      │ RUNNING │ 8188 │ 10s    │ 1 │ 0.0% │ 40.2 MB │ 0.20ms   │ ok     │ -         │
│ 4      │ RUNNING │ 8196 │ 8s     │ 1 │ 0.0% │ 40.2 MB │ 0.19ms   │ ok     │ -         │
└────────┴─────────┴──────┴────────┴───┴──────┴─────────┴──────────┴────────┴───────────┘
```

Where Warden counts the responses (its static server, and Node apps through
the shim), `warden list` and the worker table have two more columns: `req/s`,
the worker's responses per second over the last 10 s, and `4xx/5xx`, its
client and server errors of the last minute (yellow with 4xx, red with a
5xx); `-` elsewhere. `warden status` and `describe` add a `requests` row
(the rate, the last minute and the total by class) and a `connections` row:
each port's open connections, the ones waiting to be accepted out of how
many may wait, and the ones the kernel dropped, from the kernel.

In the tables, `watching` is ✓ when `[watch]` restarts the app on file
changes and ✗ when it does not, and `ports` lists each socket on a line of
its own (four at most; `+N more` says what `warden ports` shows in full).
`last exit`, how the worker's previous process ended (`exit code 1`, a
signal, an OOM kill, Warden stopping it), is a column of the worker table
of `warden status <app>` and `describe`; `warden list` leaves it out and
says it under the table for a worker that is down (`crashed` or `failed`).

Every problem Warden logs says what happened, why, what it did and how to fix
it; [`troubleshooting.md`](troubleshooting.md) collects them by symptom.

The CLI talks to the running supervisor over a Unix socket with mode 0600.
That socket's directory also holds the shim every worker preloads and the
per-worker health sockets. Warden refuses to use the directory unless it
owns it, it is not a symlink, and no other user can write to it.
