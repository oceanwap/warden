# Automatic HTTPS: `[tls]`

An app with a `[tls]` section gets a Let's Encrypt certificate that Warden orders and renews. The shim hands it to `Bun.serve`, so the app has no TLS code, no certbot and no proxy in front. Requests never pass through Warden: the app terminates TLS itself, as it does when you write `tls:` by hand.

```toml
[app]
name = "api"
command = "bun"
args = ["run", "server.ts"]   # Bun.serve({ port: Number(process.env.PORT), fetch })  (no tls: option)
port = 443

[tls]
hosts = ["api.example.com", "www.example.com"]   # one certificate for all
email = "you@example.com"                          # the CA's contact address
```

`warden start` then:

1. starts the workers at once with a short-lived self-signed placeholder, so the app listens and passes health checks;
2. answers the CA's HTTP-01 challenge on port 80 and orders the certificate (a second or two once DNS and port 80 are right);
3. stores it and runs the same gated rolling reload as `warden reload`: new workers read the certificate and old ones drain, exactly as in any `warden reload`.

By using `[tls]` you agree to the CA's terms of service (Let's Encrypt's subscriber agreement).

## Renewal

A certificate is renewed when a third of its lifetime is left (read from the certificate, so shorter lifetimes need no change). It is looked at every 12 hours. A failed order is tried again after 1, 5 and 15 minutes, then every hour doubling to 12 hours, which stays inside Let's Encrypt's limits. When a renewal keeps failing and the certificate has less than 14 days left, the log line is an ERROR. Every renewal is one rolling reload, with the drain behaviour of `warden reload` (keep-alive connections move to new workers; under a storm of brand-new connections a handful can still be reset as an old worker stops, as in any reload).

`warden tls` shows every certificate Warden holds: hostnames, placeholder or issued, days left. `warden tls renew [app]` orders a new certificate now, though the current one is not due (after a key leak, or to try the setup); the workers roll when it arrives, and a failed order is retried with the usual backoff until one succeeds.

Changing `hosts`, `email`, `staging`, `directory`, `dns_hook` or `dns_wait` takes effect with `warden reload`: a changed name list orders a new certificate (and rolls the workers again when it arrives), a changed first hostname starts with a placeholder in its new directory. `http_port` and `redirect`, and adding or removing the whole section, still need `warden restart`; a reload says so.

## What the app must do

- Use `Bun.serve` with the port from `PORT` (Warden sets it from `app.port`), and do not pass `tls`. An app that passes its own `tls` keeps it, and Warden's certificate is not used.
- Servers on other ports (admin, metrics) are left plain. Unix-socket servers too.
- Node apps are not supported yet (`https.createServer` is the app's own); use `cert`/`key` files there.
- A `[static]` site takes `[tls]` too: its workers read the same files (do not set `static.cert`/`static.key`).

## Port 80

The CA connects to port 80 of each name (IPv6 first when the name has an AAAA record). Every supervisor with `[tls]` listens there (`SO_REUSEPORT`, IPv6 and IPv4), serves the challenge, and answers everything else with a `308` to `https://<host><path>` (`redirect = false` makes that a 404). Nothing listened on port 80 before, so no existing traffic changes.

If something else (nginx) holds port 80, Warden says so in the log and retries every minute; the app keeps serving, but no certificate can be ordered. Either free port 80, forward it to `http_port`, or supply `cert`/`key` yourself. Binding 80 needs root, or `sysctl net.ipv4.ip_unprivileged_port_start=80`; macOS lets any user bind it.

## Wildcards and servers without port 80: `dns_hook`

Names like `*.example.com` need the DNS challenge, which proves control of the domain with a TXT record instead of port 80. Warden does not talk to any DNS provider; you give it a command:

```toml
[tls]
hosts = ["*.example.com", "example.com"]   # a wildcard does not cover the bare domain
email = "you@example.com"
dns_hook = ["/usr/local/bin/acme-dns"]     # argv; run as `<command> present` and `<command> cleanup`
dns_wait = 30                              # seconds to wait after `present` for the record to spread
```

The command runs as the user of Warden with `WARDEN_ACME_DOMAIN` (`example.com`), `WARDEN_ACME_RECORD` (`_acme-challenge.example.com`) and `WARDEN_ACME_VALUE` (the TXT value). `present` must add the record (the record can hold several values: `*.example.com` and `example.com` both use the same name); `cleanup` removes it after the order, also after a failed one. A non-zero exit or more than 120 seconds fails the order, which is retried later. With `dns_hook` every name is proved by DNS, so port 80 is only used for the redirect.

## Where the files are

```text
<state dir>/tls/account-<ca>.json        ACME account key (0600); account-<ca>.bak keeps the one it replaced
<state dir>/tls/<first host>/v<time>/    fullchain.pem, privkey.pem (0600), meta.json: one directory per certificate
<state dir>/tls/<first host>/live        symlink to the current directory, swapped atomically
```

The state directory is `/var/lib/warden` for root, `~/.local/state/warden` otherwise (`$WARDEN_HOME/state`). The newest three certificates are kept. A renewal that fails, or a chain that does not parse, changes nothing. `warden delete` and `warden update` never touch these files. Copy `live/fullchain.pem` and `live/privkey.pem` if you want to leave.

## With the router and per-app addresses

[`[route]`](routing.md) only reads the ClientHello and passes the connection on, so it needs no certificate; each routed app has its own `[tls]` with its own hosts. An app with an `address` and port 443 serves the certificate for its names on that address; the challenge arrives on port 80 of the server, not on the app's address. Workers of one app (also with `[balance]`) read the same `live` directory.

## Keys

| Key | Default | Meaning |
|---|---|---|
| `hosts` | required | Hostnames of the certificate, lowercase; wildcards (`*.example.com`) only with `dns_hook` |
| `email` | required | Contact address for the CA |
| `staging` | `false` | Let's Encrypt's staging CA: certificates no browser trusts, higher limits. Try it first |
| `directory` | Let's Encrypt | Another ACME directory URL (ZeroSSL, a private CA); not with `staging` |
| `redirect` | `true` | Port 80 redirects to https |
| `http_port` | `80` | Port of the challenge listener; the CA always asks port 80, so change it only behind a port forward |
| `dns_hook` | none | Command (argv) that adds and removes the DNS challenge record; proves the names by DNS and allows wildcards |
| `dns_wait` | `30` | Seconds to wait after `present` before the CA is asked to check |

`[tls]` needs `app.port`, process mode, and not `[route]`, `[expose] cert/acme` or `shim = false`. `WARDEN_ACME_ROOT=<file.pem>` names a root certificate for a private CA (Pebble in the tests).

## Not built yet

Node apps, and swapping the certificate without a reload (Bun's `server.reload()` has no `tls`). Real Let's Encrypt has only been tried against a local test CA (Pebble): start with `staging = true`.
