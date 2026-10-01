# Behind a proxy or a load balancer

Warden is never on the request path: it runs the app's workers on one port
(`[app] port`), and the kernel spreads connections over them
(`SO_REUSEPORT`). Whatever is in front (nginx, a cloud load balancer,
Cloudflare, or nothing) connects to that one port, and nothing in front
changes when you scale or deploy. This page is about the timeouts that must
agree, and what a rolling restart looks like from the front.

The short version: put nginx on the host with
[`contrib/nginx.conf`](../contrib/nginx.conf), set
`net.ipv4.tcp_migrate_req = 1` ([`contrib/99-warden.conf`](../contrib/99-warden.conf)),
keep every idle timeout *towards the app* shorter than the app's own, and
every idle timeout *of a load balancer* shorter than the one of what it
connects to.

## A rolling restart, seen from the front

`warden reload`, `safe-reload`, `restart` and recycling replace workers
without closing the port:

1. A new worker starts next to the old one and joins the port: it takes new
   connections as soon as it listens, then passes the health gates.
2. The old worker drains: it closes its listener (new connections go to the
   other workers), answers requests on its open keep-alive connections with
   `Connection: close` for `[shutdown] drain_ms`, finishes the requests in
   flight (for at most `grace_period`), and ends WebSockets (close 1001,
   Going Away) and SSE streams (cleanly, after the last whole event) after
   `long_lived_timeout`, so their clients reconnect, to new workers.
3. It exits.

A proxy sees its pooled connections to the old worker answered with
`Connection: close` (it opens new ones, which reach the new workers), its
requests in flight completed, and its long-lived streams ended cleanly. The
port answers all along: health checks pass, and nothing has to be taken out
of a load balancer for a deploy.

Two things can still cost a request. Both are rare, and both are avoidable:

- **A reset in the accept queue.** Connections waiting in a closing
  listener's accept queue at that instant are reset by the kernel, unless
  `net.ipv4.tcp_migrate_req = 1` moves them to another worker (Warden warns
  at start when it is 0). nginx with `contrib/nginx.conf` also retries such
  a request over a new connection, but only an idempotent one (GET, HEAD,
  PUT, DELETE…): a POST that was sent is never sent twice. Set the sysctl.
- **A pooled connection closed under a request.** This one is the app's
  idle timeout, not Warden: a proxy that keeps an idle connection to the
  app longer than the app keeps it open now and then sends a request on a
  connection the app is closing, and gets a reset (a 502 for its client).
  The side that sends requests must close idle connections first.

## The timeouts, side by side

| Setting | Default | What it is | What must agree in front |
|---|---|---|---|
| `[shutdown] drain_ms` | 500 ms | How long an old worker keeps answering on its keep-alive connections, with `Connection: close` | Nothing: within it, a request on a pooled connection is answered (and the proxy opens a new connection for the next); after it, the proxy finds the connection closed |
| `[shutdown] grace_period` | 30 s | The longest an old worker waits for requests in flight | The proxy's read timeout ≥ your longest request; a load balancer's deregistration delay or connection draining ≥ it when you take a whole host out |
| `[shutdown] long_lived_timeout` | 2 s | WebSockets and SSE get this long to end by themselves, then are closed cleanly | Idle timeouts in front (60–900 s) must be longer than your app's or client's ping interval; Warden's close passes through |
| The app's idle keep-alive timeout | Node 5 s (`server.keepAliveTimeout`), Bun 10 s (`idleTimeout`) | Not Warden's: the app closes idle connections after it | A proxy's idle timeout towards the app must be shorter (nginx: upstream `keepalive_timeout 4s`), or the app's raised above a load balancer's that cannot be lowered |

## nginx

[`contrib/nginx.conf`](../contrib/nginx.conf) is a commented, tested site
file (copy it to `/etc/nginx/conf.d/`). What it does, and why:

- **One upstream address**, the app's port, with `max_fails=0`: it is every
  worker, so a failed connection says nothing about the next one and the
  port is never taken out.
- **The same address again as a `backup`**, so `proxy_next_upstream error
  timeout` has somewhere to go: a GET whose connection a closing worker's
  listener reset is retried over a new connection, which the kernel gives
  to another worker. Measured with the test below (`tcp_migrate_req = 0`,
  upstream keep-alive off so every request opens a connection, ~20,000
  requests per restart of 3 workers): without the backup line the first run
  lost a request (`recv() failed (104: Connection reset by peer) while
  reading response header from upstream`, a 502); with it, 8 runs lost
  none, and nginx retried such a reset in 6 of them. Non-idempotent
  requests are never retried (no `non_idempotent`): `tcp_migrate_req = 1`
  covers them.
- **Keep-alive to the workers** (`keepalive 32`), closed after 4 s idle:
  shorter than Node's 5 s and Bun's 10 s, so nginx never sends a request on
  a connection the app is closing.
- **Client keep-alive 75 s**: longer than an AWS ALB's 60 s idle timeout in
  front. Behind GCP's load balancer use 620 s (below).
- **WebSockets** on any path (the `Upgrade` map), with a 1 h read timeout on
  `/ws`, and `lingering_close always`: when the old worker closes a
  WebSocket, the client answers the close frame, and nginx must read that
  answer before closing, or the kernel resets the connection instead of
  ending it (in 2 of 13 runs of the test without it, none in 20 with it; a
  browser may then report the close as not clean).
- **SSE** on `/events` with `proxy_buffering off` (each event goes out as it
  is written; an app can also send `X-Accel-Buffering: no` on any response)
  and a 1 h read timeout.
- **X-Forwarded-For, -Proto, -Host, -Port** and `X-Real-IP`, set once at the
  server level: a location with a `proxy_set_header` of its own would
  inherit none of them.
- **`/health`** passed to the app (whichever worker the kernel picks), for a
  load balancer's health check.

`tests/integration.rs` (`rolling_restart_through_nginx_drops_nothing`) runs
this file in a real nginx in front of 3 workers and restarts them under
load: requests on new connections, keep-alive GETs and keep-alive POSTs
through nginx, plus a WebSocket and an SSE stream held through the restart.
No request fails, and both streams are closed cleanly by their old worker and
reconnect to a new one.

Reloading nginx itself (`nginx -s reload`) keeps old nginx processes until
their connections end; WebSockets can keep them for hours. Set
`worker_shutdown_timeout 30s;` in nginx's main context to bound it.

## No proxy

Clients connect to the app's port directly. Rolling restarts work the same:
keep-alive clients get `Connection: close` during the drain and reconnect
(browsers, curl and HTTP libraries do), WebSocket clients get 1001 and
should reconnect, EventSource reconnects by itself. With no proxy to retry,
`net.ipv4.tcp_migrate_req = 1` matters more. The app terminates TLS itself
(Bun.serve's `tls`), and a port below 1024 needs `CAP_NET_BIND_SERVICE` for
the workers (`AmbientCapabilities=CAP_NET_BIND_SERVICE` in an override of
`warden@.service`).

## Cloud load balancers

Values are the providers' defaults at the time of writing; check their
documentation for your setup. The rule behind each: a load balancer reuses
idle connections to what is behind it, so what is behind it must keep them
open longer than the load balancer does. With nginx on the host that is
nginx's client `keepalive_timeout`; without it, the app's idle timeout,
which is far shorter by default.

### AWS Application Load Balancer (ALB)

- **Idle timeout** 60 s (1–4000 s), for the client and the target
  connections. Behind it, nginx's 75 s is longer. Without nginx, raise the
  app's above it (Node: `server.keepAliveTimeout = 65_000` and
  `server.headersTimeout = 66_000`; Bun: `idleTimeout: 65`), or the ALB
  sends now and then a request on a connection the app is closing: a 502.
- **Health check**: HTTP on the traffic port, path `/health` (your
  `[health] path`), 200. It reaches whichever worker the kernel picks, so it
  checks the app; it keeps passing through rolling restarts.
- **Deregistration delay** (connection draining) 300 s. A Warden deploy
  needs none. To take an instance out (scale-in, replacement): deregister
  it, wait for the delay, then stop Warden (`systemctl stop warden@api`).
  Stopping Warden first sends requests to a closed port until the health
  check fails. A delay of at least `grace_period` is enough unless
  WebSockets should be given longer.
- **WebSockets and SSE** pass through; the idle timeout applies to them (no
  data for 60 s ends them): ping, or send an SSE comment (`:\n\n`), more
  often than that.

### AWS Network Load Balancer (NLB)

- TCP pass-through to the port; the kernel balances as for local clients.
  With instance targets the app sees the client's address; with proxy
  protocol v2 turned on, nginx must parse it (`listen 80 proxy_protocol;`,
  `real_ip_header proxy_protocol;`).
- **Idle timeout** for TCP flows 350 s (configurable on newer NLBs). A flow
  idle for longer is forgotten without notice, and its next packet gets a
  reset: keep pings or TCP keep-alives shorter.
- **Health check**: TCP (the port is open whenever a worker listens, and
  Warden keeps it open through restarts) or HTTP `/health`, which checks the
  app.
- **Deregistration delay** 300 s, as for the ALB, and the same order to take
  an instance out.

### Google Cloud external Application Load Balancer

- **Keep-alive to the backend** 600 s, and the backend should keep idle
  connections longer: nginx `keepalive_timeout 620s;` in the `server` block
  (or the app's idle timeout above 600 s without nginx).
- **Backend service timeout** 30 s: the longest a response may take, and
  for WebSockets the longest one may stay open, idle or not. Raise it for
  long requests and long-lived connections (`gcloud compute
  backend-services update … --timeout=3600`), or they are cut.
- **Health check**: HTTP on the port, `/health`.
- **Connection draining** on the backend service, for removing instances (as
  AWS's deregistration delay): at least `grace_period`.
- The passthrough Network Load Balancers forward TCP like an NLB.

### Cloudflare

- **Proxy read timeout** 100 s: an origin that has not started its answer
  by then gives the client a 524. Send something on SSE streams and
  WebSockets at least every 100 s.
- **Keep-alive to the origin**: Cloudflare keeps idle connections open up to
  900 s. If occasional 520 errors show up, raise the origin's idle timeout
  (nginx's `keepalive_timeout`) above that.
- **WebSockets**: Cloudflare closes them now and then when it updates its
  servers; clients reconnect anyway, as they do after Warden's 1001.
- **The client's address**: `CF-Connecting-IP`, or X-Forwarded-For trusted
  from Cloudflare's ranges (`set_real_ip_from` for each, in nginx).
- **Health checks** (Load Balancing monitors): `/health`.
