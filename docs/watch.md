# Restarting when files change (`[watch]`)

```sh
warden start server.ts --watch                       # like `pm2 start server.ts --watch`
warden start server.ts --watch --ignore-watch "dist,*.map" --watch-delay 2
```

```toml
[watch]
enabled = true
```

Warden looks at the app's files, and when they change it starts a rolling
restart: the same gated restart `warden restart <app>` does, so the new
workers must start and pass their health checks before they take over, and
a version that fails is rolled back while the old workers keep serving.

It is **off by default**. Production apps are deployed (a new release, then
`warden reload` or `warden safe-reload`, see [`deploys.md`](deploys.md)), not
edited in place, and a restart that nobody asked for is the last thing a
production server needs. Watching is for development, staging and apps that
really are edited where they run. When it is on, `warden list` shows
`enabled` in its `watching` column, `warden describe` has a `watch` row, and
`warden status --json` has `"watching": true`.

## What happens

1. Every `interval_ms` (1 s) Warden looks at the files under `paths`: one
   `readdir` per directory and one `stat` per file, on a worker thread, so the
   supervisor's event loop never waits for the disk.
2. A file that was created, removed or changed since the last look is a
   change. Warden then waits until the files have stayed the same for
   `debounce_ms` (500 ms): a build writes many files, and a restart in the
   middle of it would start the app on half of them.
3. Then every worker is replaced, one at a time (or `[reload] surge` at a time),
   as in `warden restart <app>`:
   - the new worker has to listen (or call `ready`), pass `[reload]
     health_passes` health checks, `verify_command` and `min_ready`, if you set
     them (not `preflight` or `canary_soak`: those belong to `reload` and
     `safe-reload`);
   - a version that fails a gate is rolled back: the old workers keep serving,
     `warden status` shows the failed rollout and the reason, and the next
     change tries again;
   - hot standbys (`[workers] standby`) are replaced too, and a worker that
     had given up (`FAILED`) is tried again: fixing the file that crashed it is
     the point.
4. The log says why:
   `files changed: rolling restart file=src/main.js change=modified files=3`
   (`file` is the first changed file in path order, `files` how many changed).

## Keys

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Turn watching on |
| `paths` | `["."]` | What to watch: directories (everything under them) or single files. Relative paths start at `[app] working_directory` (else where Warden was started); absolute paths work too |
| `ignore` | `["node_modules", ".git", ".hg", ".svn", "*.log", "*.pid", "*.sock", "*.swp", "*.swx", "*~", ".DS_Store"]` | Names and patterns that are never watched (below). Setting it **replaces** the list: keep `node_modules` and `.git` in it |
| `debounce_ms` | `500` | How long the files must stay unchanged before the restart starts (0 to 600000; PM2's `watch_delay`) |
| `interval_ms` | `1000` | Time between two looks at the files (100 to 3600000). A slow scan stretches it (see Cost) |
| `max_files` | `10000` | Most files and directories looked at (1 to 100000). The rest is not watched, and one warning says so |

`warden check -c app.toml` validates all of it. A `[watch]` section for an
app with a `[static]` section is an error: Warden's file server reads the
files from disk and serves a changed file after `cache_valid_ms`, so there is
nothing to restart.

`[watch]` is read again by `warden reload`: turning it on, off or changing it
takes effect without restarting Warden. For an app that exists already,
`warden start --watch` only says so; edit the config and `warden reload <app>`.

## Which files are ignored

An `ignore` entry is a name or a pattern with `*` (anything but `/`), `?` (one
character but `/`) and `**` (anything, `/` too; `**/` also matches no
directory at all). `\` takes the next character literally. There are no
regular expressions, braces or `!` negation: list what to skip, and narrow
`paths` for what to include.

| Pattern | Matches |
|---|---|
| `node_modules`, `*.log` | A name, at any depth: the directory or file called that. An ignored directory is not entered |
| `src/generated` | A path from the working directory (a pattern with a `/` is one); `./src/generated` is the same |
| `**/*.test.ts`, `dist/**` | Paths with patterns; `**` crosses directories |
| `uploads/` | A trailing `/`: directories only, a file called `uploads` is still watched |
| `/var/app/cache` | A leading `/`: an absolute path |

Besides, Warden never watches what it writes itself: its own log files (`[logging]
file`, `out_file`, `err_file`, and their rotations `app.log.1`,
`app.log.2026-09-30`, `.gz`), its control socket and its runtime directory,
wherever they are.

What counts as a change: a file created, removed, written, replaced (an
editor's atomic save, `rsync`, `mv` over it), touched, or with changed
permissions or owner. Warden compares inode, size, modification time and
change time (to the nanosecond), not contents, so a file that was written
with the same bytes is a change, and reading a file is not. An empty directory
that appears or disappears is not (files are what the app is made of).

Symlinks: a `paths` entry (or `working_directory`) that is a symlink is
followed, so a `current` symlink works, and swapping it to a new release is a
change (see below). Inside the tree, a symlink to a file stands for that file;
a symlink to a directory is **not** followed, so a link back to a parent can
never loop. Directories deeper than 64 levels are not entered. Sockets, FIFOs
and devices are not files of the app.

## Debounce, and a build that never ends

`debounce_ms` is the quiet time. While changes keep arriving, the restart keeps
waiting: a build that writes for ten seconds restarts the app once, ten
seconds in plus `debounce_ms`. While a change is settling, Warden looks every
`min(interval_ms, debounce_ms)` (at least 50 ms). Changes that arrive during
that wait are one change: `files=` counts them all.

If files are still changing 30 s after the first one, Warden says so once
(`watched files keep changing: waiting for them to settle before restarting`):
a build that is still running is fine; a program writing into a watched
directory all the time is not (add it to `ignore`).

Restarts are also spaced out. A restart does not begin less than 2 s after the
last one began. When changes keep coming right after each restart (an app that
writes a file into its own directory every time it starts, which would restart
itself forever) the gap doubles with every restart in a row, up to 30 s, and
starts again from 2 s after a minute without one. From the fourth restart in a
row one warning says `files keep changing right after each watch restart` with
the last file, and what to do: add it to `ignore`.

## Rolling restarts and the gates

A watch restart is an ordinary rollout, so everything about rollouts applies:
[`deploys.md`](deploys.md) for how workers are replaced, drained and
gated, and `[reload]` / `[shutdown]` in
[`configuration.md`](configuration.md#reload) for the knobs.

- **One rollout at a time.** If a `reload`, `safe-reload`, `restart`, a cron
  restart or a health replacement is running when the files change, the change
  waits (`files changed: restart waits for the rollout in progress`) and is not
  lost: when the rollout is over, one restart covers everything that changed
  meanwhile. Waiting changes are looked at every second.
- **A bad version is rolled back**, as in a reload. The files stay as they are
  and nothing restarts again until they change again; fix the file and save.
- **Stopped apps are not restarted.** Watching runs while the workers run:
  `warden stop` ends it, `warden start` starts it again from the files as they
  are (what changed while the app was stopped is what it starts with), and a
  change that arrives while Warden is shutting down is dropped.
- **`pin_release`.** A restart of every worker moves the workers to the release
  `current` points to now. With watching on, swapping that symlink is itself a
  change (the files seen through it are other files), so a deploy that only
  swaps the link gets the same gated restart. A deploy script that also runs
  `warden reload` makes it happen twice: harmless (the second waits for the
  first), but pointless; keep `[watch]` for apps that are not deployed that way.
- **A watch restart does not re-read the config or the `env_file`.** Only
  `warden reload` does. Edit `app.toml` or `api.env`, then `warden reload`.
- A watch restart adds one to each worker's `↺` count, like any restart, but it is not a crash:
  it does not count towards `max_restarts`.

## Cost

Polling is what works the same everywhere: network and bind-mounted file
systems, containers and every OS, and it needs no per-directory resource
(inotify watches run out on a big tree). The price is a `stat` per file at every
look.

- **CPU.** The pause between two looks is at least four times the last look's
  duration, so a big tree on a slow disk costs at most a fifth of a core. When
  that stretches the interval Warden says so once: `a scan of the watched files
  takes long: changes are noticed later than interval_ms`, with `took_ms`.
- **Memory.** One path and a small fingerprint per watched file, so it grows with
  the number of files: at most `max_files`.
- **`max_files`.** Files and directories are counted. Past the limit the rest is
  not watched, in a fixed order (directories sorted by name, depth first), so
  what is watched is the same at every look; one warning says so:
  `the watched tree is bigger than watch.max_files (or deeper than 64 levels)`.
  Ignored entries are skipped without being looked at, but a directory
  with a million ignored files still has to be listed, so the work of one look is
  bounded as well. Put big directories (build output, caches, data) in `ignore`
  rather than raising `max_files`; the cap is 100000.
- **Latency.** A change is noticed within `interval_ms`, and the restart starts
  `debounce_ms` after the last change was seen. Raise `interval_ms` to look less
  often.

Watch the directories that hold the source, not the disk:
`paths = ["src", "package.json"]` is much cheaper than `"."` in a repository
with large data or build directories.

## What happens to odd files and directories

| Situation | Behaviour |
|---|---|
| A watched path does not exist (the directory is being deployed, or `working_directory` was deleted) | Not a change. One warning (`a watched path cannot be read; its files count as unchanged until it can`); watched again when it can be read, and what differs from before is then a change |
| A directory cannot be listed (permissions) | Its files count as unchanged: no restart, one warning with the directory (`a watched directory cannot be listed`) |
| A file vanishes between the listing and the `stat` | It is simply not there |
| A scan is running when Warden stops the workers, shuts down, reloads another `[watch]` or the watcher is replaced | It is told to stop and ends within a directory's worth of work; its result is thrown away |
| A restart cannot begin (the release `current` points to does not resolve: a deploy may be halfway) | Tried again 3 s later, three times in all, then given up until the next change |

## From PM2

`warden pm2-migrate` and `warden start` map PM2's options:

| PM2 | Warden |
|---|---|
| `watch: true` (or `[]`, or `--watch`) | `[watch] enabled = true`, watching the working directory |
| `watch: ["src", "lib"]` or `"src"` | `paths`; a glob (`lib/**/*.js`) becomes the directory it starts in (`lib`), and the migration report says it is approximated |
| `ignore_watch` / `--ignore-watch` | `ignore`, **on top of** the built-in list. Names, `*`, `?`, `**` and paths carry over; regular expressions, braces and `!` do not, and are listed in the report |
| `watch_delay` / `--watch-delay 4` (seconds, or `4000ms`) | `debounce_ms` |
| `watch_options` (chokidar) | not supported: Warden always polls, never follows a symlink to a directory and waits for the files to stop changing |

Differences:

- PM2 uses file system events (chokidar); Warden polls, so a change is noticed
  after up to `interval_ms` (1 s) and works on file systems that send no events.
- PM2's default `ignore_watch` (when you set none) skips every dot file and
  `node_modules`. Warden's built-in list names `node_modules`, `.git`, `.hg`,
  `.svn`, `*.log`, `*.pid`, `*.sock`, editor swap files and `.DS_Store`: a `.env`
  or `.eslintrc` is watched. When you do set `ignore_watch` PM2 uses only your
  list, while `warden start --ignore-watch` and the migration add yours to
  the built-in one; in a config file `ignore` replaces it.
- PM2 restarts the process; Warden does a gated rolling restart (a broken
  build does not take the app down), and never starts one while another
  rollout runs.
- `warden start --watch` is accepted for a new app only; the flags
  `--ignore-watch` and `--watch-delay` need `--watch` (without it Warden says so
  instead of ignoring them).

## Troubleshooting

The full list is in [`troubleshooting.md`](troubleshooting.md#file-watching-watch).

| You see | Why | Fix |
|---|---|---|
| Nothing restarts | `warden list` says `disabled` in the `watching` column, or the file is ignored, or outside `paths`, or beyond `max_files` | `warden describe <app>` shows the `watch` row; `warden logs <app> --events` has `file watching on paths=… ignore=…` and the warnings above |
| It restarts again and again | Something writes into a watched directory: a log file with another extension, a database file, uploads, a build output the app triggers itself | The last file is in the `files changed` line; add it to `ignore`. Restarts are spaced out (up to 30 s apart) meanwhile |
| It restarts late | Polling: `interval_ms` plus `debounce_ms`, and a slow scan stretches the interval | Lower both, or watch fewer files |
| It never restarts while the build runs | By design: it waits for the files to stay unchanged for `debounce_ms` | `watched files keep changing` after 30 s names a file that never settles |
| A new worker started but the old release still runs | The restart failed a gate and was rolled back | `warden status` shows the rollout and its reason; fix the code and save again |
| An edit of `app.toml` or the `env_file` changed nothing | A watch restart does not read the config again | `warden reload <app>` |
