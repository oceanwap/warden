# Several apps on one IP: the hostname router

An app with a `[route]` section is Warden's own router instead of a program.
It listens on one port (usually 443), reads which hostname each TLS
connection asks for (the `server_name` in its ClientHello), and passes the
connection, still encrypted, to the app serving that hostname. Several apps
then share one IP address and port with plain DNS: `api.example.com` and
`www.example.com` both point at the server, and each app does its own TLS
and HTTP/2 with its own certificate. Nothing is decrypted or parsed past the
ClientHello, and there is no Cloudflare or proxy in between.

```toml
# /etc/warden/edge.toml
[app]
name = "edge"
port = 443

[route]
hosts = { "api.example.com" = "api", "*.example.com" = "web", "*" = "web" }
```

```toml
# /etc/warden/api.toml: an ordinary app that serves TLS itself
[app]
name = "api"
command = "bun"
args = ["run", "server.ts"]   # Bun.serve({ port, tls: {...}, http2: true })
port = 8443
```

`warden start edge` and the router listens on 443. A target is an app's name
(its `[app] port`, looked up each time a router worker starts, so a reload
picks up an app that moved) or a port number. `*.example.com` matches one
label (`www.example.com`, not `a.b.example.com`); `"*"` takes every other
hostname, and connections that name none.

## The visitor's IP address

With `client_ip` (on by default on Linux) the router connects to the app from
the visitor's own address, so `server.requestIP(req)` (Bun) or
`req.socket.remoteAddress` (Node) is the visitor's IP. No PROXY protocol and
no `X-Forwarded-For` are needed. The mechanism, the same as nginx's
`proxy_bind $remote_addr transparent`:

- The router's connection is bound to the visitor's address (IP_TRANSPARENT,
  so Warden runs as root or with CAP_NET_ADMIN), and goes to the app's port on
  the address the visitor reached.
- The app answers to the visitor's address. Two routing rules, which Warden
  adds when a router worker starts and removes when the router stops, deliver
  those packets back to the router instead of the internet:

  ```text
  ip rule add pref 22356 ipproto tcp sport <app port> lookup 22356
  ip route replace local 0.0.0.0/0 dev lo table 22356      # and ::/0
  ```

Because of the rules, while the router runs an app's port answers only
through the router: a visitor connecting to `:8443` directly gets no answer.
The app has to listen on all addresses (`0.0.0.0` or `::`), not only
`127.0.0.1`. `client_ip = false` turns all of this off: apps then see
`127.0.0.1`, and the router needs no privileges (that is the only mode on
macOS). The rules use priority and table 22356, so run one router app per
host.

## Workers and speed

The router runs one worker per CPU core by default (`[workers] count` sets
another number), and the kernel spreads connections over them
(SO_REUSEPORT). Each worker is one thread. Small messages are passed with
one read and one write; a stream carrying bulk data switches to splice(2), so
its bytes never enter the router.

Measured on 2 cores (2 router workers and 2 Bun workers, client in its own
network namespace, visitor IP kept), HTTP/2 with small responses: the router
and nginx's `stream` pass-through came out level, around 130-180k requests/s
each (run-to-run noise was about 15 %). Bun alone on its port did 210-240k.
Most of the cost is the kernel's work for the second TCP connection, which
any pass-through pays. Where cores are spare the router's workers use them
and the app keeps its own speed.

Long-lived connections (HTTP/2, WebSockets) stay open through a reload of the
router until they end or `[shutdown] grace_period` runs out: a pass-through
cannot ask a client to reconnect.

## Keys

| Key | Default | Meaning |
|---|---|---|
| `hosts` | required | Hostname → app name or port. Lowercase; `*.example.com` (one label) and `"*"` (everything else) |
| `host` | `"0.0.0.0"` | Address to listen on (the port is `[app] port`) |
| `client_ip` | `true` on Linux | Apps see the visitor's IP (needs root; adds the routing rules above). `false`: apps see `127.0.0.1` |

The router needs `[app] port`, process mode, and no `[static]` or `[watch]`.
A name in `hosts` that matches no app config on the host stops a reload in
preflight.
