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

## One address per app: nothing in between

The fastest way: give each app an IP address of its own, and the kernel
sends each visitor straight to the app by the address it connected to. A
server usually comes with a whole IPv6 /64, so IPv6 addresses cost nothing;
point each hostname's AAAA record at its app's address.

```toml
# /etc/warden/api.toml
[app]
name = "api"
command = "bun"
args = ["run", "server.ts"]   # Bun.serve({ port, tls: {...}, http2: true })
port = 443
address = "2001:db8::10"
```

```toml
# /etc/warden/www.toml
[app]
name = "www"
command = "bun"
args = ["run", "server.ts"]
port = 443
address = "2001:db8::11"
```

Each app's listen on its port is on its address (the shim sets it, whatever
host the app asks for, under Bun.serve, node:http and Node), so both have port
443. The visitor's IP is the app's peer, with no rules and no privileges
beyond binding port 443. Measured with two Bun apps on 2 cores: 313k
requests/s together, the same as Bun on its own.

On Linux the supervisor adds a missing address to the interface of the
default route when the app starts, and removes it when the app stops (an
address that was already there is left alone):

```text
ip -6 addr add 2001:db8::10/128 dev eth0 nodad preferred_lft 0
```

`preferred_lft 0` keeps the server's own outgoing connections on its main
address. Adding an address needs root; otherwise add it yourself (netplan,
`ip addr`) and Warden only binds it. On macOS add it yourself.

IPv4 visitors: a spare IPv4 address per app works the same way. With one
IPv4 for the server, run the router on it and name the apps in its `hosts`:

```toml
# /etc/warden/edge.toml
[app]
name = "edge"
port = 443

[route]
hosts = { "api.example.com" = "api", "www.example.com" = "www" }
```

The router listens on `0.0.0.0:443` (IPv4) next to the apps on their IPv6
addresses. On Linux the apps' workers still listen on their own addresses,
so IPv6 visitors reach them with nothing in between, and the router hands
its IPv4 connections over (below); the app sees the visitor's IPv4 address.
When an app cannot take a handed-over connection (stock Bun), the router
passes the bytes to the app's address, without `client_ip`: the routing rules
that keep the visitor's IP would also catch the answers to the app's direct
visitors. Those apps see the router's address for IPv4 visitors.

## Handing connections over

The router only peeks at the ClientHello and leaves it in the socket. When
the app's supervisor takes connections from Warden, the
router hands the socket itself to it over `route.sock` in the app's runtime
directory, and the app's worker serves it as if it had accepted it. From then
on the router is out of the path: no second connection, no bytes copied, and
the app sees the visitor's own address without any routing rules.

An app named in a `[route]` on the same host takes its connections this way
when its supervisor starts (start the router's config first, or restart the
app after adding it to `hosts`; `WARDEN_HANDOFF=0` turns it off). Which
workers can take a handed-over socket:

- **Bun.serve**: with a Bun that has `server.adopt(fd)` (proposed upstream,
  oven-sh/bun#44768). Stock Bun cannot, so its workers listen on the port as
  usual and the router passes the bytes instead.
- **node:http / node:https** under Node: always.

Measured with a Node HTTPS app (2 workers, HTTP/1.1, client in its own
network namespace): 30-40k requests/s handed over, 32-35k direct, 14-23k
when the router copies. With a patched Bun the hand-off measured level with
Bun direct on HTTP/2 (235-315k against 274-304k requests/s), at about 40 µs
more CPU per new connection.

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

## Workers and speed (when bytes are passed)

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
