# Warden and wattpm (Platformatic Watt)

Warden and Watt both start several copies of a Node.js app, restart them when
they crash and put them behind one port, so their commands look alike
(`start`, `stop`, `restart`, `reload`, `ps`, `logs`). They are different
tools: Watt is an application server that runs several applications inside one
runtime; Warden is a process supervisor that runs independent programs. This
page says where they overlap, where they do not, which `wattpm` command is
which `warden` command, who should move and who should not, and how
`warden migrate-wattpm` converts a Watt project.

It was written from wattpm 3.71.0 (the version the benchmarks in
[`benchmarks.md`](benchmarks.md) use, installed in `bench/`): its `wattpm help`
output, its source and its own config loader, and the Watt documentation at
[docs.platformatic.dev](https://docs.platformatic.dev/). For another Watt
version, run `warden migrate-wattpm --dry-run` and read the report.

## What Watt is, and what Warden is

| | Watt (wattpm 3.71.0) | Warden |
|---|---|---|
| What runs | Node.js only (`engines`: Node >= 22.19.0). No Bun | Any program: Bun, Node, Python, a binary, a shell line |
| Unit | A **runtime**: one `watt.json` listing several **applications**, started and stopped together by `wattpm start` | An **app**: one config file, one supervisor. Apps are independent; namespaces group them for `restart` |
| Processes | The applications run as worker threads of one process (a command-based application runs as a child process of it) | A process per worker. A thread mode exists for Bun (experimental) |
| Entry point | One application is the `entrypoint` and gets the runtime's public port (`server.port`). The others are reachable only inside the runtime | Each app owns its port. Workers share it through `SO_REUSEPORT`; Warden is not on the request path |
| Between applications | The mesh: `http://<id>.plt.local` is delivered in the process, with no network hop. The Gateway application routes by path prefix and merges OpenAPI | None. Apps talk over real addresses; nginx or Caddy in front does the routing ([`proxies.md`](proxies.md)) |
| Frameworks | Capabilities for Next.js, NestJS, Remix, Astro, React Router, Nuxt, TanStack Start, Nitro and Vite, plus Platformatic Service, DB and PHP; Express and Fastify run through the Node.js capability | A framework app is a command (`next start`) like any other |
| Runtime services | An HTTP cache shared by the applications, undici interceptors, a scheduler for HTTP cron jobs, OpenTelemetry, a management API, a Prometheus and probes server, autoscaling of workers, an admin UI | Metrics and health from the supervisor ([`configuration.md`](configuration.md)), a rolling-restart engine, [wardend](wardend.md), a GUI, a static file server |
| Restarts | `wattpm restart` replaces the workers of each application one at a time, the applications in parallel. `wattpm reload` stops the runtime and starts it again from its saved command line: there is downtime | One worker at a time through health gates, with a canary and rollback; `deploy` adds a preflight and a soak ([`deploys.md`](deploys.md)). No downtime |
| Daemon | None. `wattpm start` runs in the foreground; you give it to systemd, a container or a shell | Supervisors in the background, wardend above them, systemd units from `warden startup` |

### Where they overlap

Running N copies of a Node.js app on one port, restarting what crashes, scaling
the count, setting environment, graceful stop, and a rolling restart. If that
is all you use Watt for (one application in a project, or several that never
call each other and are each fronted by a proxy), Warden does the same job.

### Where they do not

Watt composes applications into one service. If your project relies on any of
these, Warden has no equivalent and `migrate-wattpm` can only hand you the
pieces:

- **One entry point in front of several applications**: the Gateway, its path
  prefixes, merged OpenAPI. Warden has no proxy.
- **In-process calls between applications** (`http://<id>.plt.local`,
  `policies`, the shared HTTP cache, undici interceptors). Warden leaves the
  application's HTTP client alone.
- **Framework integration**: Watt starts Next.js, Astro or Vite inside the
  runtime, rewrites base paths and serves their dev servers (`wattpm dev`).
  Warden can run `next start`, nothing more.
- **Runtime services**: the scheduler, autoscaling (`workers.dynamic`), the
  management API and the admin UI.

Warden does what Watt does not: it runs Bun apps, supervises apps that are not
Node.js, isolates a crash to one worker process (threads share a process; a
fault that kills the process takes every application in the runtime, a native
crash or the kernel's OOM killer for example), gates rolling restarts on
health checks, keeps long-lived connections clean across a restart (close
code 1001, end of stream), installs its own boot units (`warden startup`), has a
GUI and a static file server.

### What the benchmarks measured

[`benchmarks.md`](benchmarks.md) has the method and every number. For the same
Node.js app with 4 workers on a 2-CPU machine, Watt (4 worker threads) against
Warden (4 processes):

| | Watt | Warden |
|---|---|---|
| Memory, idle (PSS) | 478 MB | 106 MB |
| Idle CPU of the manager | 2.8 % | 0.2 % |
| Startup to 4 workers serving | 3.3 s | 0.2 s |
| A crashed worker answers again | 1.4 s | 0.14 s |
| `/plaintext` requests per second, p99 | 50.2k, 5.3 ms | 73.5k, 2.4 ms |
| Rolling restart under load | 0 of 3250 requests failed, 3.3 s | 0 of 3686 failed, 0.3 s |
| Status command | `wattpm ps` 425 ms | `warden status` 1.7 ms |

Read that for what it is: one application per runtime. None of Watt's
composition was in use, so these numbers are the cost of the runtime, not a
measure of what composing applications is worth. If you do compose, memory per
application is not the question; the in-process mesh is.

## Commands

`wattpm help` in 3.71.0 lists these commands. "Runtime" below is what wattpm
calls an application in `ps` and `stop`: the whole project.

| wattpm | Warden | Difference |
|---|---|---|
| `wattpm init`, `create`, `add` | none | Creates a Watt project. `warden start server.js --name api -i 4 --port 3000` writes an app config for you |
| `wattpm build [root]` | none | Warden does not build. Run the application's build before `warden start` or `reload`; the report lists each `application.commands.build` |
| `wattpm dev [root]` | `warden start app.js --watch` | Warden's watch mode is a gated rolling restart when files change ([`watch.md`](watch.md)). There is no framework dev server, no hot module replacement |
| `wattpm start [root] -c <config> -e <env>` | `warden start app.toml`, or `warden migrate-wattpm` first | `wattpm start` stays in the foreground and starts every application. `warden start` starts one app in the background and waits until it is up; `warden start -c app.toml` with no app is the foreground supervisor systemd runs |
| `wattpm stop [id]` | `warden stop <app>`, `warden kill` | `wattpm stop` stops the runtime, so every application in it. `stop` takes one app, a namespace or `all` |
| `wattpm restart [id] [application...]` | `warden restart <app>` | Watt restarts the applications in parallel. Warden replaces one worker at a time through health gates and rolls back on failure; `restart api:2` is one worker; `restart --hard` stops all, then starts |
| `wattpm reload [id]` | `warden reload <app>`, `warden deploy <app>` | Watt stops the runtime and spawns it again. Warden re-reads the config, then does the gated rolling replace |
| `wattpm ps` | `warden list`, `warden status` | One row per runtime against one row per worker (ids, state, cpu, memory, ports, last exit) |
| `wattpm applications [id]` | `warden list`, `warden describe <app>` | |
| `wattpm env [id] [application]` | `warden env <app>` | Values hidden unless `--show-secrets`; `app:N` is one worker |
| `wattpm config [id] [application]` | `warden config <app>` | The effective config as JSON |
| `wattpm logs [id] [application]` | `warden logs <app>` | Also `--history`, `--grep`, `--since`, rotated and gzipped files, `--json` |
| `wattpm inject [id] [application]` | `curl` against the app's URL from `warden ports` | There is no in-process request injection |
| `wattpm metrics [id]` | `[metrics] listen` (Prometheus text), `warden top` | Warden's metrics are the supervisor's and each worker's; the application exposes its own |
| `wattpm pprof`, `heap-snapshot`, `repl` | none | Node's own tools work on a process Warden started: put `--inspect` or `--cpu-prof` before the script in `args` |
| `wattpm applications:add`, `applications:remove` | `warden start`, `warden delete` | Apps are independent; adding one does not touch the others |
| `wattpm scheduler`, `scheduler:run`, ... | none | Watt fires HTTP requests on a cron schedule. Use cron or a systemd timer. Warden's `[restart] schedule` restarts workers; it does not call a URL |
| `wattpm admin` | the Warden GUI | |
| `wattpm version` | `warden version` | |
| `-S, --socket <path>` | `-s, --socket <path>` | The control socket to talk to. Warden has one per app |

Differences that are not commands:

- **The id.** wattpm takes a pid or a package name for the runtime and an
  application name after it. Warden takes ids, names, namespaces or `all`
  ([`commands.md`](commands.md)). The converter puts every application of a
  project in a namespace named after the project's package, so `warden restart
  shop` restarts them all.
- **The environment.** Watt reads `.env` files and fills `{NAME}` placeholders
  in its config. Warden has no placeholders; the converter resolves them and
  writes the result to a 0600 env file.
- **Logs.** Watt prints its own pino lines for each application. Warden keeps
  what the application writes to stdout and stderr, as it is.

## Who should move, and who should not

Move if:

- Watt is how you run N copies of a Node.js app, or a few apps that do not call
  each other, and you do not use the Gateway or the mesh. The converter turns
  each application into an app and you keep serving on the same ports.
- You want Bun, or Node.js and non-Node.js apps under one manager.
- You want the gated rolling restart with rollback, or the lower memory and
  faster restarts in the table above.

Stay with Watt if:

- Applications call each other as `http://<id>.plt.local` and you want that to
  stay an in-process call.
- A Gateway composes your applications behind one entry point and you do not
  want to run a proxy.
- You run Next.js, Astro or Vite through Watt's integration, or use `wattpm
  dev`, the scheduler, the HTTP cache or `workers.dynamic`.

Mixed projects can be split. `--apps` converts only the applications you name,
so you can move the stand-alone ones and leave the composed ones in Watt, as
long as nothing that stays calls one that left over the mesh.

### What the converter does with composition

- **A Gateway (or Composer) application is not converted.** Warden has no
  proxy to put in its place. The report lists the applications it fronted and
  their path prefixes, which is what an nginx or Caddy config needs
  ([`proxies.md`](proxies.md), [`contrib/nginx.conf`](../contrib/nginx.conf)).
- **The entry point keeps its port.** Its `server.port` (with `{PORT}`
  placeholders resolved from `.env` and the environment) becomes `[app] port`.
  Without a Gateway in front, that port is now served by the entry
  application's own workers.
- **An internal application has no port** unless its own environment sets
  `PORT`. In Watt it was reachable only as `http://<id>.plt.local`. The
  converted app runs but nothing can reach it until you give it a `port` and
  point its callers at that address. The report says so for each one, and
  `grep -r plt.local` finds the calls.
- **The mesh itself, `policies`, the shared HTTP cache, undici interceptors,
  the scheduler, extensions, `basePath`, the metrics and probes server, the
  management API, telemetry and autoscaling** are listed as unsupported, each
  with what to do instead. Nothing is dropped without a line in the report.
- **A port two applications would share is flagged.** Warden shares a port
  between workers with `SO_REUSEPORT`, so two different programs on one port
  would both start and the kernel would split the requests between them.

## `warden migrate-wattpm`

```sh
warden migrate-wattpm --dry-run                  # what it would write, for the project in this directory
warden migrate-wattpm ~/shop                     # <app>.toml + <app>.env (0600) + MIGRATION-wattpm.md
warden migrate-wattpm ~/shop --cutover new-port:4200
```

It reads the project and writes Warden's files; it does not run wattpm or
Node.js, and it does not stop anything.

### What it reads

- The config file, found as Watt finds it: `watt.json`, `platformatic.json`,
  then `.json5`, `.yaml`, `.toml` and the `runtime`, `service`, `application`,
  `db`, `gateway` and `composer` names. A directory or the file itself.
  Formats read: JSON, JSON5 with comments and trailing commas, TOML. YAML is
  refused with a message (Watt reads it; save the same settings as JSON).
  Unquoted keys and single-quoted strings in a `.json5` file are refused too.
  A `.json` file with a comment fails with the line, because Watt's own reader
  rejects that too.
- The applications from `applications`, `services` (Watt 2's name) and `web`,
  and from `autoload` (`path`, `exclude`, `mappings`), merged by id as Watt
  does. `enabled` is read as Watt reads it for `wattpm start`: a boolean, a
  string, or a value per environment.
- Each application's own config for its type: `@platformatic/node` and the
  others, from `$schema` or `module`, else detected from `package.json`.
- A config that is one application (no runtime key) is wrapped the way Watt
  wraps it: its settings under `runtime` are the runtime's.
- `{NAME}` placeholders in every string, from the project's `.env` (or `-e`,
  or `envfile`) and then the environment of the command, with Watt's
  precedence. A name with no value becomes an empty string, as in Watt, and the
  report says so.

### What it writes

For each application, in the output directory (Warden's config directory by
default: `$WARDEN_HOME`, `/etc/warden` for root, else `~/.config/warden`; `--out`
chooses another):

- `<name>.toml`, the Warden config. It is checked with the same parser `warden
  check` uses before it is written. The header comment is written by this
  command, not copied from a command line, so a command with a line break cannot
  leave it.
- `<name>.env`, mode 0600, the application's environment in Watt's order: the
  runtime's `.env`, the application's own `.env` or `envfile`, the runtime's
  `env`, the application's `env`. `NODE_ENV=production` is added when none is
  set, as `wattpm start` does. The shared `PORT` is left out of internal
  applications. A value from the environment or a `.env` file reaches the env
  file and, in one case, the config; it is in no other output (see below).
- `MIGRATION-wattpm.md`, once per run, replaced each time (not `MIGRATION.md`,
  which `pm2-migrate` writes in the same directory). A table of the
  applications, then for each one every setting as **mapped**, **approximated**
  (with the difference), **unsupported** (with the reason) or **check**, then
  what applies to the whole runtime.

Each file is written to a temporary file in the same directory and renamed, so
a reader never sees half a file. Without `--overwrite` the names are created
exclusively, so a file that appears meanwhile is not lost.

| Watt | Warden |
|---|---|
| Application `id` | `[app] name` (`--prefix p` gives `p-<id>`; characters a name cannot hold are replaced; `.` and `..`, which name directories, become `app-dot` and `app-dotdot`, with a note) |
| `path` | `working_directory` |
| Node.js entry: `node.main`, `package.json` `main` and `exports`, then `index`, `main`, `app`, `application`, `server`, `start`, `bundle`, `run`, `entrypoint` with `.js`, `.mjs`, `.cjs` | `command = "node"`, the file in `args` |
| `application.commands.production`, or `--command <id>=<line>` | The program and its arguments (split as Watt splits them); `node_modules/.bin` is searched as Watt does; a line with `&&`, `\|\|` or `;` runs through `sh -c` (a `{NAME}` in it becomes `"${NAME}"`, read from the env file) |
| `workers` (a number or `{static, ...}`, inherited from the runtime) | `[workers] count` |
| `server.port` on the entry point | `[app] port` |
| `server.portAssignment: perWorkerIncrement` | `port_strategy = "offset"` |
| `gracefulShutdown.application` (default 10 s) | `[shutdown] grace_period` |
| `restartOnError` on an application | `[restart] enabled = false`, or `backoff_initial` (at most 225 s: Warden's `backoff_max` is 16 times it and may not pass an hour; a longer delay is capped, with a note) |
| `startTimeout` | `[workers] ready_timeout` (at most an hour, as is `grace_period`) |
| `health.maxHeapTotal` | `node --max-old-space-size` (Watt's total minus its young generation) |
| `execArgv`, `nodeOptions`, `preload`, `sourceMaps` | node flags, for a `node <file>` application |
| `arguments` | arguments after the script |
| `watch` | `[watch] enabled` |
| `enabled: false` for production | The application is left out |

#### Secrets

Watt fills `{NAME}` placeholders in when it reads the config; Warden has no
placeholders, so the value of a `{NAME}` in a command line lands in the config
as text. Here is exactly what is guaranteed. A value from the environment of the
command, or from a `.env` file, appears:

- in the env file (mode 0600), always;
- in the config only where the program has to be given it on its command line:
  `application.commands.production`, `arguments`, `execArgv`, `nodeOptions`,
  `preload` and `node.main`. When the command is a shell command (it chains with
  `&&`, `||` or `;`), the config holds no value at all: it reads `"${NAME}"`,
  quoted for where it stands, and the env file supplies the variable. Every other
  form needs the value itself in `args`; that config is written with mode 0600
  instead of 0644, and the run says so;
- nowhere else. The report, `--dry-run`, the messages on stdout and stderr and
  the errors show it as `***`; an error about an invalid config shows the
  validation message and its line number, never the config text.

A value counts as a secret when it came from the environment or a `.env` file
and is not a number of five digits or fewer (a port, a count). It is hidden
wherever the same text appears in that application's section of the report, so a
plain word such as `info` can be hidden too. A value written literally in a
config file (`"env": {"TOKEN": "..."}`) is not a placeholder value and is not
hidden, but it goes to the env file and not to the config.

Warden's restart policy differs from Watt's, which restarts a crashed worker
for ever: Warden marks a worker failed after `max_restarts` crashes in
`restart_window`. The report says so for every application. Watt's `health`
checks (event-loop utilization, heap thresholds) have no Warden equivalent;
`[watchdog]`, `[limits] max_memory` and `[health] path` are the nearest, and the
report names them.

### What it does not convert

An application it cannot run on its own is **not converted**, says why in the
report, and makes the exit code 1. The others are still written.

- A Gateway or Composer application (above).
- An application of a type that has no command in its config: Next.js, NestJS,
  Remix, Astro, React Router, Nuxt, TanStack Start, Nitro, Vite, and Platformatic
  Service, DB and PHP. Watt starts these through a capability, not a command.
  Give the framework's own production command with
  `--command site="next start"`, or set `application.commands.production` in the
  application's `watt.json`. It then runs as an ordinary process, without Watt's
  integration (base path, mesh, dev server).
- An external application (`url` instead of `path`) that `wattpm resolve` has
  not fetched.
- A Node.js application whose entry file is not there yet (Watt reads it after
  `wattpm build`: build first), or that has none.

Two warnings worth knowing before you start:

- If the entry file exports `create()` or `build()` and never calls `listen()`,
  Watt calls the factory and listens for it. Run as `node <file>` it starts no
  server, and the report flags it.
- Watt makes the entry point listen on `server.port` whatever the application
  asked for. Warden gives the app its `port` as `PORT`, so the application has to
  listen on `PORT`.

### Options

| Option | Meaning |
|---|---|
| `[dir\|file]` | The project directory, or its config file. Default: the current directory |
| `-c, --config <name>` | The config file's name in that directory (wattpm's `-c`) |
| `-e, --env <path>` | A `.env` file for placeholders and environment (wattpm's `-e`) |
| `--apps a,b` | Only these Watt application ids |
| `--out <dir>` | Where to write. Default: Warden's config directory |
| `--prefix <p>` | Put `<p>-` in front of every app name |
| `--command <id>=<line>` | The command to run for an application (repeatable) |
| `--dry-run` | Print every config, the env variable names and the report; write nothing |
| `--overwrite` | Replace files that exist. Without it an existing `<name>.toml` or `<name>.env` is kept (the env file may hold what the app needs and nothing else has), that application is reported and the exit code is 1. A symbolic link at either path is never written through, with or without it: that application is reported and the exit code is 1 |
| `--cutover overlap\|new-port:<port>` | Start Warden's copy next to the running Watt; see below |

Exit code: 0 when every application was converted (and started, with
`--cutover`); 1 when one was not, a file was kept, an app was not started, or
the project could not be read; 2 for a bad command line.

### Cutover

`--cutover` starts the converted apps with Warden. It never stops Watt.

- `overlap`: Warden's workers share each application's port with Watt's (Watt
  sets `reuseTcpPorts` by default), so for a while requests are split between the
  two. Use it to see the new app serve real traffic before you switch.
- `new-port:<port>`: the entry point's `port` is `<port>` and the rest keep
  theirs. Switch your proxy when you are satisfied.

What it does when it cannot start everything:

- Two apps on one port are not started. If the entry point's `server.port` (or
  the `new-port` you gave) is also an internal application's own `PORT`, Warden
  would start both and the kernel would split the requests between two different
  programs. Neither is started, the reason is printed on stderr before anything
  starts, and the exit code is 1. The other apps start.
- An app that runs already is not started again: `<name>: already running with
  its previous config`. Its supervisor keeps the config it started with, so after
  `--overwrite` it needs `warden reload <name>` to take the new file.
- If an app's workers do not come up, the apps this run had started are stopped
  too, and the message names them: nothing of the run is left running. Watt was
  never touched, and neither was any process this run did not start. The configs
  stay; fix the app and `warden start` it.
- When every app is up it says which, and `warden stop <names>` stops them.

When you are satisfied, stop Watt yourself with `wattpm stop <project>`
and run `warden save` and `sudo warden startup`.

`pm2-migrate` can stop PM2's copy of an app and put it back if Warden's fails
(`--cutover same-port`, `--finalize`). This command offers neither, on purpose.
PM2 manages each app on its own, so it can stop one and restore it. Watt's unit
is the whole runtime: `wattpm stop` takes every application down at once, and
Warden cannot start it again with the command line and environment it had.

An application with no port (a worker that consumes a queue, a job runner) runs
twice during an overlap, in Watt and in Warden, and does its work twice. Move
those with `--apps` while Watt is stopped, or stop Watt first.

### Limits

- It reads configs; it does not run Watt. What Watt works out at start (a
  capability that generates its config, an application that reads its settings
  from code) is not known to it.
- It does not build, install or resolve anything.
- Placeholders use the environment of the command that runs the migration. If
  wattpm runs with another environment (a unit file, a container), check the
  values; the report lists the names that came from the environment.
- It does not reproduce the mesh, composition or in-process routing. It reports
  them.
- It has been checked against wattpm 3.71.0: `tests/migrate_wattpm.rs` runs
  Watt's own config loader on a project and compares the applications, worker
  counts, ports and directories, and starts a real runtime and runs Warden's copy
  next to it (those two tests skip themselves without `npm ci` in `bench/`). A
  config that uses something newer is read as far as it is understood; the rest is
  not mapped.

### An example

A project with an entry point `web` and two internal applications, from a
`watt.json` with `"workers": 2` and `"server": {"port": "{PORT}"}`:

```
$ warden migrate-wattpm ~/shop --dry-run
# ---- /home/me/.config/warden/web.toml
[app]
name = "web"
namespace = "shop"
command = "node"
args = ["/home/me/shop/web/index.mjs"]
working_directory = "/home/me/shop/web"
env_file = "web.env"
port = 4100

[workers]
count = 2

[shutdown]
grace_period = 10
...
```

and, in `MIGRATION-wattpm.md`, lines like these:

```
- server.port: mapped: port = 4100
- restart policy: approximated: Watt restarts a crashed worker for ever; Warden marks a worker FAILED after [restart] max_restarts (10) crashes in restart_window (60 s) ...
- port: check: no port: in Watt it is reached only inside the runtime as `http://<id>.plt.local`. ...
- mesh: unsupported: the applications call each other in the process as `http://<id>.plt.local`; there is no mesh in Warden. ...
```

Next: `warden check -c web.toml`, `warden start web`, point the proxy at the
port, stop Watt, `warden save`.
