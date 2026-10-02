# Deploying without downtime

```sh
# ship the new release (e.g. new dir + atomic `current` symlink swap), then:
warden safe-reload -c /etc/warden/api.toml     # or: systemctl reload warden@api
```

`safe-reload` (also `warden deploy`) goes through these stages. It exits
non-zero on failure, so a deploy script can stop there.

1. **Preflight** (like `nginx -t`): re-reads and validates the config and checks
   that the command and entry script exist. It also runs `reload.preflight` if
   set. If anything fails, nothing is touched and the running config stays in
   effect. If a later step fails, the previous config is restored too, so crash
   restarts don't roll forward to the rejected version.
2. It refuses to start if any worker is already down or unhealthy.
3. **Canary**: one new worker starts *next to* the old one. It must be listening,
   pass `health_passes` checks on its private socket, pass `verify_command` if
   set, and stay healthy through `canary_soak` seconds of real traffic. Only
   then is the old worker drained. **If the canary fails it is stopped, and
   every worker still runs the previous version.**
4. The remaining workers are replaced one at a time with the same gates, with an
   optional `pause` between them (each old worker drains while the next one is
   replaced). The rollout **halts on the first failure**.

```
$ warden safe-reload                      # 4 workers, canary_soak = 10
safe-reload started
  [0/4] worker 1 (canary): health checks 1/3
  [0/4] worker 1 (canary): health checks 3/3
  [0/4] worker 1 (canary): soaking, 9s left
  ...
  [0/4] worker 1: draining old process (pid 8141)
  [1/4] worker 2: health checks 2/3
  [1/4] worker 2: draining old process (pid 8142)
  ...
safe-reload complete: 4 worker(s) replaced in 20.1s

$ warden safe-reload                      # a release whose /health returns 503
safe-reload started
  [0/4] worker 1 (canary): health checks 0/3
warden: safe-reload failed at worker 1: new worker keeps failing health checks: HTTP 503.
Rolled back: every worker still runs the previous version. The previous config is back in effect.
$ echo $?
1
```

With a health path configured, the gates are mandatory. A worker that can't be
checked fails them; they are never skipped. With `port_strategy = "offset"`,
workers can't overlap, so each old worker is stopped before its replacement
starts, and a failure can't be rolled back.

`warden reload` (also SIGHUP) runs the same gates without the canary soak,
fleet check or pauses. `warden restart N` replaces one worker through the gates.

## WebSockets and SSE

A long-lived connection never finishes by itself, so
a draining worker would hold it until `grace_period` and then be killed, and
its clients would see a reset (WebSocket 1006, a broken EventSource). Instead,
the shim gives these connections `[shutdown] long_lived_timeout` (default
2 s) to end by themselves, then closes each WebSocket with **1001 (Going
Away)** and ends each SSE response cleanly after its last complete event.
Clients reconnect, and land on the new workers: EventSource does this by
itself, and WebSocket clients should reconnect on close. This covers `Bun.serve`
(`websocket` handlers; `text/event-stream` bodies from a ReadableStream, a
`type: "direct"` stream or an async generator) and `node:http` on Node and
Bun (WebSockets taken over through `'upgrade'`, as the `ws` library does;
SSE written with `res.write`), in process and worker mode. Other streamed
responses, such as downloads, are never cut short: they finish like any
request in flight, within `grace_period`. Each worker logs what it closed:
`closed long-lived connections … websockets=3 sse=12`.

## Drains overlap

A rolling restart doesn't wait for each old worker to
finish: once its replacement listens and has passed the gates, the old one
stops taking connections and drains in the background while the next
worker is replaced. So 4 workers holding WebSocket clients restart in about
one `long_lived_timeout` plus the startups (2.3-2.5 s measured), not one
per worker (~8.5 s). Up to `[reload] max_draining` (default 4) old workers
drain at once; past that the next replacement waits for one to exit, since
each holds its memory until then (`max_draining = 1`: one worker at a time, as
before). The command returns, and the rollout counts as done, once every
old worker has exited. `warden status` lists the ones still draining as
`2 (old)  DRAINING`.

## Faster rollouts

`[reload] surge = 2` (or `"all"`) starts that many new
workers at once, each next to the worker it replaces. Once every one of them has
passed the gates, the old ones drain together and the next batch starts. If one
fails, every new worker of the batch is stopped and the old ones keep serving.
safe-reload still runs its canary alone first. The cost is memory: surge N runs up
to N extra workers for a few seconds (`"all"`: twice the workers), and old ones
still draining count too: a rollout runs at most max(`surge`, `max_draining`)
processes beyond the worker count. It needs
workers that can overlap, so not with `port_strategy = "offset"`.

## Release pinning

`[app] pin_release` (on by default): Warden resolves a
`current` symlink in `working_directory` when it starts and when a reload,
safe-reload or restart begins, and starts workers in that real path (with
`args` that go through the symlink rewritten too). A worker that crashes after
you swapped the symlink but before you reloaded comes back on the release the
others run, not the new one. `warden status` shows the pinned release. If
that directory is deleted, the next restart falls back to `current` and logs a
warning.

## See also

- [`configuration.md`](configuration.md#reload): the `[reload]` and `[shutdown]` keys.
- [`reliability.md`](reliability.md): crash recovery, health checks, hot standbys.
- [`proxies.md`](proxies.md): timeouts in front of Warden that must agree with the drain.
- [`troubleshooting.md`](troubleshooting.md): a rollout that failed or hung.
