# Configuration

Each app has one TOML file. [`warden.example.toml`](../warden.example.toml) is
the annotated version of everything below. Every key except `[app] name` and
the command (or `entry` in worker mode) is optional. Unknown keys are
rejected, so a typo is an error, not a silent default.

- **Where the file lives.** `warden start -c warden.toml` (or `-c` on any
  command) uses that one file. Without `-c`, `$WARDEN_CONFIG` if set, else every
  app in `$WARDEN_HOME`, `/etc/warden` (root) or `~/.config/warden`. Relative
  paths in the file (`working_directory`, `env_file`) are relative to the file.
- **Check it.** `warden check -c warden.toml` validates the file and exits
  (like `nginx -t`). `warden config <app>` prints the effective config of a
  running app as JSON, defaults included (values hidden unless `--show-secrets`).
- **Change it.** `warden reload <app>` re-reads the file and rolls the workers
  through the health gates; a failure rolls back. `warden safe-reload` (also
  `warden deploy`) adds preflight and a canary. See [`deploys.md`](deploys.md).
- **No file at all.** `warden start server.js --name api -i 4 --port 3000`
  writes one for you ([`commands.md`](commands.md#starting-an-app)).
- **Alert rules** are a different file, `wardend.toml`
  ([`wardend.md`](wardend.md#alerts)); `warden check -c wardend.toml` validates it.

## Minimal config

Your app only has to listen on `process.env.PORT`.

```toml
[app]
name = "api"
args = ["run", "dist/main.js"]
working_directory = "/srv/api/current"
port = 3000

[workers]
count = 4

[health]
path = "/health"          # checked on each worker's private socket
```

## `[app]`

| Key | Default | Meaning |
|---|---|---|
| `name` | required | `[A-Za-z0-9._-]`, not `all` |
| `command` | `"bun"` | Process mode: the app (`bun`, `node`, `python3`, any program). Worker mode: the Bun binary that hosts the Workers |
| `args` | `[]` | Arguments of the command, e.g. `["run", "dist/main.js"]` (process mode) |
| `entry` | none | Worker mode: the module each Worker imports |
| `working_directory` | | Where workers run; relative to the config file |
| `port` | none | Sets `PORT` and enables readiness detection (a worker is ready when it listens) |
| `env` | `{}` | Environment variables, e.g. `{ NODE_ENV = "production" }` |
| `env_file` | none | `KEY=value` lines (dotenv / systemd `EnvironmentFile`), relative to the config file, read again by `warden reload`. Keep secrets there (mode 0600). See [Environment variables](#environment-variables) |
| `shim` | `true` when `command` is `bun` or `node`, else `false` | The shim: `reusePort`, readiness, drain, private health socket |
| `namespace` | `"default"` | A group for `warden restart backend` |
| `instance_var` | `"NODE_APP_INSTANCE"` | Environment variable holding the worker's 0-based index (PM2's `instance_var`); `""` sets none |
| `pin_release` | `true` | Start workers in the real path of `working_directory` (a `current` symlink is resolved at start and at each reload, safe-reload or restart), so a crash restart after the symlink swap stays on the running release. See [`deploys.md`](deploys.md#release-pinning) |

## `[workers]`

| Key | Default | Meaning |
|---|---|---|
| `count` | `1` | Number of workers; also `"max"` (one per CPU) or `"max-1"` |
| `mode` | `"process"` | `"process"` (production, one crash affects one worker) or `"worker"` (experimental: Bun `Worker` threads in one process) |
| `port_strategy` | `"shared"` | `"shared"` (`SO_REUSEPORT`) or `"offset"` (`PORT = port + i - 1`) |
| `ready_timeout` | `30` | Seconds to start listening, else killed and counted as a crash |
| `wait_ready` | `false` | Ready on `process.send('ready')` instead of listening (PM2's `wait_ready`) |
| `min_uptime` | `1000` | Milliseconds an app without a port must stay up to count as ready |
| `overlap` | yes when the port can be shared | Start the new worker before stopping the old one in rolling restarts |
| `standby` | `0` | Hot standbys: extra workers kept started (app initialized) but not listening; when a worker dies one takes its slot in a few ms instead of a cold start. Each costs about one worker's memory. Process mode, shared port, bun/node shim. See [`reliability.md`](reliability.md#hot-standbys-crash-recovery-in-milliseconds) |

## `[restart]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Restart crashed workers |
| `max_restarts` | `10` | Restarts allowed in `restart_window`... |
| `restart_window` | `60` | ...seconds, then the worker is FAILED |
| `backoff_initial` | `100` | Milliseconds before the 2nd crash-in-a-row restart (the 1st is immediate), doubling per further crash |
| `backoff_max` | `10000` | Milliseconds |
| `failed_cooldown` | `300` | Seconds before a FAILED worker is retried (`0` = never) |
| `schedule` | none | Rolling restart on a cron schedule, local time (PM2's `cron_restart`), e.g. `"0 3 * * *"` |
| `stop_exit_codes` | `[]` | Exit codes that mean "done, don't restart" (PM2's `stop_exit_codes`) |

## `[shutdown]`

| Key | Default | Meaning |
|---|---|---|
| `grace_period` | `30` | Seconds to exit after the stop signal before SIGKILL |
| `drain_ms` | `500` | Milliseconds the shim keeps answering with `Connection: close` |
| `signal` | `"SIGTERM"` | Or `"SIGINT"` for apps written for PM2 (its default) |
| `long_lived_timeout` | `2` (or half of `grace_period` if that is shorter) | Seconds (fractions ok) a draining worker lets WebSockets and SSE streams end by themselves, then closes them (1001 / end of stream) so clients reconnect to new workers. Below `grace_period`; `0` = leave them open |

## `[health]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Periodic per-worker checks (the rollout gates use `path` whenever it is set) |
| `path` | none | Checked on each worker's private socket, e.g. `"/health"` |
| `live_path` | `path` | "Is this process OK": periodic checks |
| `ready_path` | `path` | "Can it serve": rollout gates |
| `url` | none | Optional app-level check through the shared port, e.g. `"http://127.0.0.1:3000/health"` |
| `interval` | `5` | Seconds |
| `timeout` | `2` | Seconds |
| `failure_threshold` | `3` | Consecutive failures |
| `initial_delay` | `10` | Seconds after a worker is ready before periodic checks start |
| `outage_threshold` | `0.5` | This share of workers failing at once means a dependency is down: replacements are held (`1.0` = off) |
| `on_failure` | `"replace"` | `"replace"` the worker gracefully, `"log"`, or `"reload"` (app-level) |

## `[reload]`

Gates for every replacement: `reload`, `safe-reload`, `restart N` and recycling.

| Key | Default | Meaning |
|---|---|---|
| `health_passes` | `3` | Consecutive passes on the new worker's private socket |
| `health_interval_ms` | `500` | Milliseconds between those checks |
| `verify_command` | none | A smoke test, e.g. `curl -sf --unix-socket "$WARDEN_WORKER_SOCKET" http://w/ready` |
| `min_ready` | `0` | Seconds each new worker must stay healthy before taking over |
| `canary_soak` | `30` | Seconds the safe-reload canary runs next to the old worker |
| `pause` | `0` | Seconds between workers (safe-reload) |
| `preflight` | none | Command that runs before anything is touched, e.g. `"bun run check:deploy"`; exit 0 = go |
| `timeout` | `120` | Seconds allowed per worker for all gates |
| `surge` | `1` | New workers started at once, each next to the one it replaces: `1`, `N` or `"all"`. The old ones drain once the whole batch passed the gates; a failure stops the batch's new workers. Briefly runs up to N extra workers (memory). Not with `port_strategy = "offset"` |
| `max_draining` | `4` | Old workers that may still be draining (closing WebSockets/SSE, finishing requests) while the next ones are replaced; past that, the next replacement waits for one to exit. Each holds its memory until it exits: a rollout runs at most max(`surge`, `max_draining`) extra processes. `1` = each old worker exits before the next starts |

How the gates work: [`deploys.md`](deploys.md).

## `[watchdog]`

| Key | Default | Meaning |
|---|---|---|
| `timeout` | `60` | Seconds without an event-loop heartbeat means hung (`0` = off) |
| `loop_delay_warn` | `0.5` | Seconds (fractions ok). Warn when a worker's event-loop delay (p99, from the shim's heartbeat; the `loop p99` column of `warden list`) stays at least this high for 10 s; at most once per 10 min per worker (`0` = off) |

## `[limits]`

| Key | Default | Meaning |
|---|---|---|
| `max_memory` | `0` | MB per worker process; recycle gracefully above it (`0` = off) |
| `max_lifetime` | `0` | Seconds; recycle each worker after this, ±10% jitter (`0` = off) |

## `[watch]`

Off by default. Rolling restart when the app's files change; every key,
the ignore rules and the cost are in [`watch.md`](watch.md).

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Turn watching on (`warden start --watch`) |
| `paths` | `["."]` | Directories or files to watch, relative to `working_directory` |
| `ignore` | `node_modules`, `.git`, `*.log`, ... | Names and patterns never watched; setting it replaces the list |
| `debounce_ms` | `500` | Files must stay unchanged this long before the restart |
| `interval_ms` | `1000` | Time between two looks at the files |
| `max_files` | `10000` | Most files and directories looked at |

## `[logging]`

| Key | Default | Meaning |
|---|---|---|
| `level` | `"info"` | `debug`, `info`, `warn` or `error` |
| `timestamps` | auto: off under journald (it adds its own) | `true` or `false`: timestamps on Warden's lines |
| `worker_output` | `"capture"` | `"capture"`: through Warden (prefixed, in `warden logs`, in files). `"inherit"`: workers write straight to Warden's stdout. `"direct"`: straight into `out_file` / `err_file`, see below |
| `max_lines_per_sec` | `10000` | Worker output kept per worker and stream; the excess is counted and dropped. `0` = keep every line (a flooding app is slowed instead). Capture only |
| `file` | none | Everything, as `warden logs` shows it, e.g. `"/var/log/warden/api.log"` |
| `out_file` | none | Worker stdout as written (PM2's `out_file`) |
| `err_file` | none | Worker stderr as written (PM2's `error_file`) |
| `per_worker_files` | `false` | `api-out-1.log`, `api-out-2.log`... (PM2 without `merge_logs`) |
| `file_timestamps` | `false` | Prefix out/err lines with a timestamp (PM2's `time`; capture only) |

`warden flush` empties all of these files that exist now (every worker's too),
never the rotated ones; `warden logs --history` reads them back, rotated and
`.gz` ones included, oldest first.

`worker_output = "direct"` is for apps that log heavily and only need the files:

- The kernel moves the bytes from each worker's pipe into its file (splice): no
  parsing, no copy through Warden, no line budget, no line lost; the file is
  byte for byte what the app wrote (no prefix, no timestamp), rotated by
  `[logging.rotate]` at line boundaries (a rotated file ends with the line that
  reached `max_size`; a line that never ends is cut once the file is
  `max_size` + max(`max_size`, 1 MiB) long).
- Nothing goes to stdout/journald or into memory. `warden logs` reads the tails
  of the files (stamped with the file's last write time, since lines carry
  none) and `-f` follows new lines; `tail -f` works as always. A slow disk slows
  the app's writes (as if it wrote the file itself) instead of dropping lines.
- It needs `out_file`. stderr goes to `err_file`, or into `out_file` (like
  `2>&1`, in the order written) when `err_file` is unset. Several worker
  processes (`workers.count > 1` in process mode) need `per_worker_files = true`:
  each worker's bytes arrive unparsed, and in one shared file a partial line of
  one could meet another's. Filesystems without splice work too, copied through
  Warden (one warning says so).

### `[logging.rotate]`

Applies to every log file above; built in, no logrotate needed.

| Key | Default | Meaning |
|---|---|---|
| `max_size` | `"10M"` | Rotate at this size (`"500K"`, `"1G"`; `0` = only on interval) |
| `keep` | `5` | Rotated files kept per log file |
| `interval` | none | Also rotate on a cron schedule (local time), e.g. `"0 0 * * *"` |
| `compress` | `false` | gzip rotated files (read back by `warden logs --history`) |
| `date_suffix` | `false` | `api.log.2026-09-30T00-00-00` instead of `api.log.1` |
| `max_age_days` | `0` | Also delete rotated files older than this (`0` = off) |

## `[metrics]`

| Key | Default | Meaning |
|---|---|---|
| `listen` | none | e.g. `"127.0.0.1:9464"`: Prometheus text at `/metrics` |

## `[control]`

| Key | Default | Meaning |
|---|---|---|
| `socket` | `<runtime dir>/<name>/control.sock` | The control socket, e.g. `"/run/warden/api/control.sock"` (`RuntimeDirectory=warden/%i` in the unit). The runtime dir is `/run/warden` for root, else `$XDG_RUNTIME_DIR/warden` or `/tmp/warden-<uid>`; `$WARDEN_RUNTIME_DIR` overrides it |

## `[static]`

A static site instead of an app (what `warden serve <dir> <port>` writes).
The workers are Warden's own file server: no command needed, process mode.
Every key but `root` is optional. What the server does and why:
[`static-serving.md`](static-serving.md).

| Key | Default | Meaning |
|---|---|---|
| `root` | required | The directory to serve; a `current` symlink is resolved per worker start |
| `host` | `"0.0.0.0"` | Address to listen on (the port is `[app] port`) |
| `spa` | `false` | Unknown paths get `index.html` (single-page apps) |
| `index` | `"index.html"` | The file served for a directory |
| `cache_max_age` | `3600` | Seconds; fingerprinted names are cached a year |
| `html_max_age` | unset | Seconds browsers may reuse HTML pages. Unset or `0`: HTML is revalidated on every load with its ETag (a 304 when unchanged). 0 to 31536000; `public`, or `private` with `basic_auth` |
| `listing` | `false` | HTML listing for directories without an index |
| `dotfiles` | `false` | Serve dotfiles (`.well-known` is always served) |
| `precompressed` | `true` | Serve `file.br` / `file.gz` when the client accepts them. A request that accepts them looks for the sibling of every file but images, fonts, audio, video and archives: two failed lookups (about 4 µs of CPU) when there is none, so a site without precompressed files can set `false` |
| `basic_auth` | none | `"user:password"`; keep the config file private (0600) |
| `headers` | `{}` | Extra response headers, e.g. `{ "X-Frame-Options" = "DENY" }` |
| `access_log` | `false` | One stdout line per request |
| `cache_size` | `0` | Per worker: prebuilt responses of small files in memory, e.g. `"16MB"` (`0` = off, the default: files are read from the OS page cache on every request); bodies of 8 KB and up are kept in memfds and sent with `sendfile` (one descriptor each, at most 1/8 of the open-files limit) |
| `cache_max_file` | `"64KB"` | With a cache: larger files are not cached (sent with `sendfile`) |
| `cache_valid_ms` | `1000` | With a cache: a cached file is re-checked on disk at most this often |

## Environment variables

A worker starts with the supervisor's own environment (`PATH`, `HOME`, what
systemd or your shell gave it), then, each one winning over the ones
before:

1. `[app] env_file`: `KEY=value` lines (dotenv / systemd `EnvironmentFile`
   syntax), relative to the config file. Keep secrets there, mode 0600;
   `warden reload` reads it again.
2. `[app] env`.
3. Warden's variables below. They win over the two above: `env = { PORT =
   "8080" }` next to `port = 3000` gives the app 3000 (`warden env` marks
   the override).

`warden env api` prints exactly that for worker 1 (`api:3` for worker 3),
with where each value comes from; the app's values are hidden unless
`--show-secrets`. `warden config api` prints the whole effective config as
JSON, defaults included.

| Variable | Value | In |
|---|---|---|
| `PORT` | `[app] port`; with `port_strategy = "offset"`, `port + N - 1` for worker N | every worker and standby, `verify_command`; only with `app.port` |
| `NODE_APP_INSTANCE` (the name is `[app] instance_var`; `""` sets none) | worker N gets `N - 1` (PM2's 0-based index). A hot standby gets a number past the workers' until it is promoted, then its slot's | process workers and standbys; in worker mode each Worker (set by the shim) |
| `WARDEN_APP` | the app's name | everything Warden runs: workers, `verify_command`, `preflight` |
| `WARDEN_WORKER_ID` | the worker number, 1..N. `0` in a standby until promoted (then its slot's). Worker mode: each Worker 1..N, none in the host process | workers; `verify_command`: the worker it checks (`host` in worker mode, `s1`… for a standby) |
| `WARDEN_WORKER_COUNT` | workers configured when the process started (after `warden scale`, running workers keep the old count; a promoted standby gets the current one) | workers, standbys, worker-mode host and Workers |
| `WARDEN_MODE` | `process` or `worker` | workers |
| `WARDEN_STANDBY` | `1` in a hot standby until it is promoted | standbys, their `verify_command` |
| `WARDEN_WORKER_PID`, `WARDEN_WORKER_SOCKET`, `WARDEN_WORKER_SOCKETS` | the new worker's pid and private health socket(s) (space-separated; one per Worker in worker mode) | `verify_command` only |
| `WARDEN_WORKERS`, `WARDEN_ENTRY`, `WARDEN_SHIM` | how many Workers, the module each imports, the shim | worker-mode host process |
| `WARDEN_STATIC` | the `[static]` section as JSON | `warden serve` workers |
| `WARDEN_IPC_FD` | `3`: the shim's channel to Warden (readiness, heartbeat) | every worker |
| `WARDEN_HEARTBEAT_MS`, `WARDEN_DRAIN_MS`, `WARDEN_LONG_LIVED_MS`, `WARDEN_STOP_SIGNAL`, `WARDEN_WAIT_READY`, `WARDEN_REUSE_PORT`, `WARDEN_HEALTH_DIR`, `WARDEN_INSTANCE_VAR`, `WARDEN_INSTANCE` | settings for the shim: heartbeat period (1000), `shutdown.drain_ms`, `long_lived_timeout` in ms, `shutdown.signal`, `1` with `wait_ready`, `1` with a shared port, where private health sockets go, the instance variable's name, an internal process number (unique per supervisor run) | every worker |

PM2's `pm_id` and `name` are not set: use `WARDEN_WORKER_ID` and
`WARDEN_APP` (`warden pm2-migrate` leaves them out of the migrated env).

Variables that configure Warden itself (not the workers): `WARDEN_CONFIG`
(the default for `-c`), `WARDEN_HOME` (the directory of the apps' configs),
`WARDEN_RUNTIME_DIR` (the runtime directory: control sockets), `WARDEN_PARALLEL` (how many apps start at once, see
[`commands.md`](commands.md#parallel-starts)), `WARDEN_NO_DAEMON=1` (never
start wardend), `WARDEN_TABLE=1` (print the app table after a command, also in
a log) and `NO_COLOR` (no colors).
