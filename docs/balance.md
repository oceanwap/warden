# Balancing one hostname over workers on their own ports

Some workers cannot share a port: a Python or Go program that binds its own
port, a runtime without `SO_REUSEPORT`, or an app you want to reach on each
worker's port as well. `[balance]` lets one hostname still have one public
port: the kernel sends each new connection to one worker, with nothing in
between.

```toml
# /etc/warden/api.toml
[app]
name = "api"
command = "bun"
args = ["run", "server.ts"]   # Bun.serve({ port: Number(process.env.PORT), tls: {...}, http2: true })
port = 9000                   # worker 1 gets 9000, worker 2 gets 9001, ...
address = "2001:db8::10"      # the address api.example.com's AAAA record points at

[workers]
count = 4
port_strategy = "offset"      # each worker its own port

[balance]
port = 443                    # visitors connect to [2001:db8::10]:443
```

Workers never need `reusePort`. A visitor's connection to `address:443` is
put in one worker's accept queue by the kernel; the worker sees the visitor's
own address and no byte is copied or parsed.

## How it works

A small eBPF `sk_lookup` program (Linux 5.9+) is attached to the network
namespace. For a connection to the app's address and `balance.port` it picks
a worker's listening socket by a hash of the visitor's address and port and
assigns the connection to it (`bpf_sk_assign`); every other connection is left
alone. Warden keeps the program's list of sockets: a worker is added when it
is ready and removed when Warden starts to stop it, so a draining worker gets
no new connections. A worker that dies leaves the list by itself (the kernel
drops a socket when it closes) and its share goes to the next worker. Warden
loads the program with raw `bpf(2)`: no libbpf and no compiler.

## What it needs

- Linux 5.9+ and root (or `CAP_BPF` and `CAP_NET_ADMIN`; Warden also needs to
  read the workers' sockets, which root can). If the program cannot be
  attached, Warden says why in its log (`[balance] is off`) and the workers keep
  running on their own ports, unbalanced: nothing else changes.
- `app.address` (one address per hostname, so the kernel can tell hostnames
  apart: it cannot read a hostname) and `workers.port_strategy = "offset"`.
  Sharing one IPv4 address between hostnames is the job of the
  [hostname router](routing.md).
- At most 256 workers per app. Each `[balance]` app attaches its own program,
  which matches only its own address and port.

## Behind the hostname router

With [the router](routing.md) in front (`hosts = { "api.example.com" = "api" }`,
for visitors on a shared IPv4 address), the router connects to the workers'
ports on `app.address` from the visitor's own address (`client_ip`, on by
default on Linux with root), so the workers see the visitor's IP. The routing
rules cover `app.port`, `app.port + 1`, ...: those ports answer only through
the router. The workers may listen on `app.address` alone. Not with a loopback
`app.address` or a visitor of the other IP family: those connections come from
the router's address.

## What it does not do

- No health-based routing beyond what Warden already does (a worker that fails
  its checks is replaced, and leaves the list while it stops).
- No weights, no least-connections: the choice is a hash of the visitor.
- Not on macOS (the kernel has no equivalent; use the router there).
- If Warden's supervisor for the app crashes, the program goes with it until
  its keeper starts a new supervisor (the workers keep serving on their own
  ports meanwhile). It is attached again as the workers are adopted.

Measured against nginx 1.30.6 and 1.31.8 on the same two cores: see
[the benchmark notes](../bench/README.md#balance).
