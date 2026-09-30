# warden-gui

A native window on [wardend](../README.md#wardend-one-socket-for-every-app):
every app on a host and its live monitoring, and the CLI's actions, in one
place. Rust and [iced](https://iced.rs) 0.14, drawn on the CPU (tiny-skia):
one ~8 MB binary, no web view, no GPU driver.

![The main screen, rendered headless by the tests](../docs/gui-main-screen.png)

## What it does

- **Connection bar**: wardend's version and pid, the host's CPU, memory and
  load (from `host` events). When wardend is not running it says so, why it
  matters, and offers **Start wardend** (`warden daemon --background`, with
  the `warden` next to the GUI, else the one on PATH). It reconnects by
  itself, with backoff (0.25 s up to 5 s), and says `reconnecting…` meanwhile.
- **App list**: each app's state, who supervises it, workers ready /
  configured, CPU and resident memory (workers, the Bun host in worker mode,
  the supervisor), and its problem with the fix (`AppEntry.problem`). Live
  from `apps` and `status` events.
- **App detail**: the workers table (id, state, pid, uptime, restarts, CPU,
  RSS, health, last exit, and a per-worker restart), the rollout in progress
  with a progress bar (`rollout` / `rollout_done`), the last rollout's
  outcome, and the app's events as `warden events` prints them (the last 500).
- **Actions**, each a request to wardend (`{"cmd":"app",…}`): reload, safe
  reload, rolling restart, hard restart, restart one worker, scale − / +,
  stop, start (wardend's `start` when the supervisor is not running, which
  also clears `gave_up`), reset. Stop, hard restart and scaling to 0 ask
  first. The supervisor's answer shows as a toast; a failure says what
  failed and how to fix it.
- **Logs**: the app's recent lines, then live ones (`subscribe` with
  `logs: true` for that app only), kept in a ring of 2,000 lines, with pause,
  a filter, and stdout / stderr / events toggles. A flood never freezes the
  window: lines are handed over in batches at most 20 times a second, a batch
  keeps the newest 2,000 lines, and every gap shows as "… N lines skipped",
  as `warden logs -f` does.
- **Add app**: script, program or command line, name, instances, port, and
  an optional env file. It runs `warden start <what> --name <name> -i <n>
  --port <port>` (the exact command is shown) and shows what it printed. The
  env file's variables go into the new supervisor's environment, never onto
  a command line or into the config, and `env_file = "<path>"` is then added
  to the config so restarts from it keep them.
- **Edit config**: the app's TOML (`AppEntry.config`) in an editor.
  **Validate** runs `warden check -c` on the text (a temporary file beside the
  config, so relative paths resolve the same) and shows its error; **Save**
  validates, then writes the file (mode and owner kept); **Reload now** follows.
- **Remote host**: see below.

It is a separate process and only a client: closing or killing it touches no
app, and it never talks to a supervisor directly.

## Run it

```sh
warden-gui                                    # this machine's wardend
warden-gui --socket /run/warden/wardend.sock  # a given socket (root's wardend)
warden-gui --ssh deploy@web-1                 # a remote host (below)
warden-gui --warden ~/src/warden/target/release/warden   # the CLI to run here
WARDEN_GUI_THEME=light warden-gui             # the light theme
```

Start wardend, Add app and Edit config run the `warden` CLI: `--warden`,
else the one next to `warden-gui` (the GUI + CLI download, `Warden.app`),
else the one on PATH.

From a checkout: `cargo run -p warden-gui` (it finds `target/debug/warden`
next to itself for Start wardend, Add app and Edit config).

On Linux it needs a desktop session (Wayland or X11) and loads
`libxkbcommon` plus `libwayland-client` or the usual X11 libraries at run
time; it links nothing but glibc. macOS: `Warden.app` holds both `warden-gui`
and `warden`.

## A remote host over SSH

wardend has no TCP listener, so there is nothing to expose: the GUI runs

```sh
ssh -N -T -o BatchMode=yes -o ExitOnForwardFailure=yes -o StreamLocalBindUnlink=yes \
    -L <private local socket>:<remote wardend.sock> -- user@host
```

and speaks to wardend through the local end (OpenSSH forwards Unix sockets).
The local socket lives in a directory only you can open
(`$XDG_RUNTIME_DIR/warden-gui-<uid>/`, mode 0700). Set it with **Connection…**
or on the command line:

```sh
warden-gui --ssh deploy@web-1                                           # root's wardend: /run/warden/wardend.sock
warden-gui --ssh deploy@web-1 --remote-socket /run/user/1000/warden/wardend.sock   # a user's wardend
warden-gui --ssh deploy@web-1 --remote-warden '~/.local/bin/warden'   # for Add app, Edit config
```

- No password ever goes through the GUI: ssh runs with `BatchMode=yes` and
  uses your agent and keys (`ssh-add`), your `~/.ssh/config` (aliases, ports,
  jump hosts) and `known_hosts`. Connect once with `ssh user@host` in a
  terminal to accept a new host key.
- When ssh fails, its own message is shown, with the usual fix (key not
  loaded, host key, name, wardend not running there).
- Add app, Edit config and Start wardend run `warden` on the remote host
  through `ssh user@host '<command>'`, every argument quoted for its shell
  (a POSIX shell: sh, bash, zsh). An env file for Add app must be on this
  machine; on a remote host add `env_file` with Edit config.
- The tunnel is dropped with the connection, and re-opened (with backoff) if
  ssh exits.

## How it works

- `client.rs` keeps **one** `subscribe` connection to wardend
  (`interval_ms: 1000`) for the whole window. It reads continuously and
  hands the UI one batch per 50 ms at most, with each app's newest status and
  the newest host metrics only; it never waits for the UI, so wardend never
  waits for (or drops) it. While the Logs tab shows an app, a second
  `subscribe` with `logs: true, apps: [<app>]` runs for it (a filter on the
  main stream would also filter out the other apps' statuses). Actions are
  one short connection each.
- Socket I/O and subprocesses run on the tokio executor, never on the UI
  thread. The UI redraws when something arrives (about once a second, the
  status cadence), never in a loop.
- The wire types come from the `warden-protocol` crate (`../protocol`), which
  `warden` itself uses: the two cannot drift. The GUI does not depend on the
  `warden` crate.
- Long lists (events, logs) draw only the rows in view.

## Memory and CPU

Measured on Linux x86_64 (Xvfb, 1280×820 window), release build, connected
to a wardend watching 10 apps (2 workers each), the first app's events
shown, after 15 s, then 60 s idle:

| Build | RSS | PSS | Private | Threads | Idle CPU |
|---|---|---|---|---|---|
| default (tiny-skia, CPU) | 20.4 MB | 16.0 MB | 13.5 MB | 4 | 0.18% |
| `--features wgpu` (GPU renderer; here Mesa's llvmpipe through OpenGL, no GPU) | 143.6 MB | 120.6 MB | 99.6 MB | 9 | 1.9% |

The target was under 60 MB. The default build is the CPU renderer: loading
a GPU driver costs more memory than drawing this window on the CPU (a real
GPU driver weighs less than llvmpipe, but still tens of MB), and iced's
tiny-skia backend redraws only what changed. The binary is 7.7 MB (12.2 MB
with wgpu). `cargo build -p
warden-gui --features wgpu` adds the GPU renderer (used first when a GPU is
found; `ICED_BACKEND=tiny-skia` forces the CPU one).

To measure again: start 10 apps (`warden start "sleep 100000" --name appN -i
2 --no-wait`), `warden daemon`, then read `Rss`/`Pss` in
`/proc/<pid>/smaps_rollup` and the CPU ticks in `/proc/<pid>/stat`.

## Tests

```sh
cargo build --bin warden && cargo test -p warden-gui
```

- Unit tests: every `update` path (feed, actions and confirmations,
  toasts, logs, dialogs), event → state, the bounded buffers, batching and
  backoff, and the command lines for Add app and ssh (quoting included:
  quoted command lines are run through `sh` and must come back unchanged).
- `tests/render.rs`: headless rendering with `iced_test` (tiny-skia): the
  main screen with fake wardend data, an app that gave up, the logs tab, a
  confirmation and the "wardend is not running" screen. Each is searched for
  what it must show, clicked, and saved as a PNG in
  `$WARDEN_GUI_SNAPSHOT_DIR` (default `target/tmp/snapshots`; CI uploads
  them as the `gui-snapshots` artifact).
- `tests/daemon.rs`: the client layer against a real `warden daemon` (the
  workspace's `target/debug/warden`, with a private `WARDEN_HOME`): apps,
  statuses and host metrics arrive, a reload is followed to `rollout_done`,
  errors come back with words, log lines stream, and the feed reconnects
  after wardend restarts. It stops everything it started.
- The SSH tunnel is covered by its command line and error tests; it is not
  run end to end in CI (no sshd there).
