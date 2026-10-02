# wardend: one socket for every app

wardend is the host daemon. It is always on: `warden start` and `warden
resurrect` start it, a supervisor starts it again if it dies without a clean
exit (a clean exit removes its socket; a crash or `kill -9` leaves it), and
only `warden kill` of everything stops it. There is no `warden daemon`
command. (`warden wardend [status]` exists, hidden from `--help`, as the
entry point for service units, launchd, the supervisors and the GUI.)
`WARDEN_NO_DAEMON=1` never starts it. Apps never depend on it: killing it
stops nothing.

wardend (started by `warden start` and `warden resurrect`, by the units in
[`production.md`](production.md), or by a supervisor when it died) watches
every supervisor on the host and pushes what happens to `warden events` and
the [GUI](../gui/README.md):

```
$ warden events
12:00:01 wardend pid=4211 version=0.1.0 (Ctrl-C to stop)
12:00:01 api running (supervised by wardend, pid 4208)
12:00:01 api 4/4 workers ready
12:00:09 api worker 2 crashed pid=4230 exit code 3
12:00:09 api worker 2 restarting in_ms=0
12:00:09 api worker 2 starting pid=4262
12:00:09 api worker 2 ready pid=4262 startup_ms=31
```

A supervisor that dies (`kill -9`, OOM) is started again with backoff when
nothing else would (systemd restarts its own units; one run in a terminal is
yours); a hung one is reported, never killed, because its workers are still
serving. The protocol, for scripts and other clients:
[`protocol.md`](protocol.md).

## Alerts

Put rules in `<config dir>/wardend.toml` (`/etc/warden` for
root, `~/.config/warden` for a user) and wardend tells a command or a
webhook when something goes wrong:

```toml
[[alert]]
on = ["crash_loop", "gave_up", "rollout_failed", "unresponsive", "worker_failed", "oom"]   # or ["all"]
apps = ["api"]                                    # optional; default every app
webhook = "https://hooks.slack.com/services/…"    # POSTed as JSON, through curl
min_interval = "5m"                               # repeats within it are counted and sent as one

[[alert]]
on = ["all"]
command = ["/usr/local/bin/notify", "--channel", "ops"]   # the alert as JSON on stdin
```

The kinds also include `died`, `unhealthy`, `recycled` and `recovered`
(healthy again after an alert). `warden check -c wardend.toml` validates the file
with every problem and its line, and `warden doctor` checks it too; wardend
reads it again by itself when it changes (or on SIGHUP), and a broken file
keeps the rules in force. Deliveries never hold
wardend up: a bounded queue, 10 s per try, one retry. Webhooks go through
`curl` (Warden has no TLS stack of its own), with the URL on curl's stdin,
never in a process list or a log line. Details:
[Alerts](protocol.md#alerts).

## History

wardend keeps the last 24 hours of every app's CPU, memory,
workers ready and restarts, and the host's CPU, memory and load, from the
statuses it already receives (a sample per 10 s; at most 135 KiB per app).
It saves them every minute and when it stops (`<state dir>/wardend-history.bin`,
written atomically), so a restart of wardend (an upgrade, a crash, a
reboot) keeps the charts; a damaged file is moved aside with a warning.
The GUI charts them; scripts ask `{"cmd":"history"}`
([Resource history](protocol.md#resource-history)).

## See also

- [`production.md`](production.md): the units and launchd job that run wardend at boot.
- [`protocol.md`](protocol.md): the wardend socket protocol.
- [`troubleshooting.md`](troubleshooting.md#reboots-crashes-and-wardend): wardend problems by symptom.
