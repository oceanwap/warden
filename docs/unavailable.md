# A 503 page while the app is down

```toml
[unavailable]
enabled = true
after = 2          # seconds without a serving worker before the page appears
# host = "0.0.0.0" # the address the app listens on (default 127.0.0.1)
# page = "503.html" # your HTML (up to 64 KB, relative to the config file)
retry_after = 5    # the Retry-After header, seconds
```

Without it, a request to an app with no worker up is refused (`ECONNREFUSED`),
and nginx in front answers 502. With it, Warden answers on the app's port
itself: `503 Service Unavailable`, your page, `Retry-After`, `Cache-Control:
no-store`, `Connection: close`. Off by default.

## Why this cannot slow the app

Warden does not sit on the request path, and this does not change that. The
page is a listener in the app's `SO_REUSEPORT` group that exists **only while
no worker serves**:

- It appears after `after` seconds without a serving worker (a first start
  that is slow, a crash loop, `FAILED`). A crash restart, a standby
  promotion or a reload is shorter and never sees it.
- The supervisor drops it on the first tick (within a second) after a worker
  serves, and answers the connections already waiting on it before it closes.
  While workers serve, nothing is listening for it: no request an app
  answers meets it, and nothing is added to one.
- It runs on a thread of its own with a 64-connection cap, so a flood during
  an outage never reaches the supervisor's event loop.
- It is not there after `warden stop` or during shutdown: a stopped app is
  stopped, not failing.

One honest limit: between a worker starting to listen and the supervisor
noticing (milliseconds), the kernel may still hand a new connection to the
page, which answers 503 with `Retry-After` instead of the app answering. In
six reloads from down to up with about 60,000 back-to-back requests each, none
did; it is a possibility, not a proof of zero.

## Keys

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Turn the page on |
| `after` | `2` | Seconds without a serving worker before the page appears (max 3600) |
| `host` | `127.0.0.1` | The address the app listens on. Must match: Bun's default is `0.0.0.0` and Node's `::`, so a public app sets `host`. With `app.address`, that address is used. A page on another address than the app's would take every connection to it while it is up (and be gone within a second) |
| `page` | built-in | An HTML file read when the page appears. Unreadable or over 64 KB: logged, and no page |
| `retry_after` | `5` | `Retry-After` seconds (max 86400) |

Needs `app.port` and workers that share it: not with `[static]`, `[route]`,
`[balance]` or `port_strategy = "offset"`, and not where workers take their
connections by hand-off (macOS with several workers), where it stays off.

A TLS connection (an app serving HTTPS on this port) is closed without an
answer: it sees what it saw before. Reloading the config applies changes.
