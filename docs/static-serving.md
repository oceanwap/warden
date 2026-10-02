# Static file serving

```sh
warden serve dist 8080                       # like `pm2 serve dist 8080`
warden serve dist 8080 --name site --spa -i 4
```

`warden serve dist 8080` (or a `[static]` section, see
[`configuration.md`](configuration.md#static) and `warden.example.toml`) runs Warden's own file server as the app's workers:
supervised, health-checked and reloaded like any app. It speaks HTTP/1.1
with keep-alive: ETag / Last-Modified and 304s, single ranges,
precompressed `.br` / `.gz` siblings, SPA fallback, `404.html`, Basic auth.
Paths can't leave the root (`..`, symlinks out, NUL), and files are opened
with `openat2(RESOLVE_BENEATH)` on Linux.

`warden serve [dir] [port]` serves `.` on 8080 by default. Its options:
`--name`, `--spa`, `--listing`, `-i N` (workers) and `--basic-auth user:pass`
(or `--basic-auth-username` / `--basic-auth-password`). The command writes
the `[static]` config for you; put the same keys in a config file to keep
them.

## The response cache

Small files are served from memory. Each worker keeps complete responses
(headers and body) of recently used files up to `cache_max_file` (64 KB), in
at most `cache_size` (16 MB, least recently used out first; `0` turns it
off), so a hit is one system call: a `send(2)` from memory, or, for a body
of 8 KB or more, a `sendfile(2)` from a sealed in-memory file (memfd), where
the kernel takes the pages by reference instead of copying them (48 KB
script: 28 % less CPU per request, ahead of nginx; see
[benchmarks.md](benchmarks.md)). HEAD and 304s come from the same entry;
ranges and anything unusual take the normal path. A cached file is checked
against the disk at most every `cache_valid_ms` (1 s): an edit, a deletion
or a symlink swapped in shows within that time (on NFS, within the
attribute cache time). A file changed in the last 2 s is served but not
cached. A deploy that swaps a `current` symlink needs a rolling restart
anyway (each worker resolves the root once), which starts with an empty
cache. With `access_log = true` each line ends in `cache=hit` or
`cache=miss`, and each worker prints its cache counters when it stops.

## Keys

The `[static]` section replaces the app's command: no `command` is needed,
process mode. Defaults are in the table; every key is optional except `root`.

| Key | Default | Meaning |
|---|---|---|
| `root` | | The directory to serve (a `current` symlink is resolved per worker start) |
| `host` | `"0.0.0.0"` | Address to listen on (the port is `[app] port`) |
| `spa` | `false` | Unknown paths get `index.html` (single-page apps) |
| `index` | `"index.html"` | The file served for a directory |
| `cache_max_age` | `3600` | Seconds. Fingerprinted names are cached a year |
| `html_max_age` | unset | Seconds browsers may reuse HTML pages (`warden serve --html-max-age N`). Unset or `0`: HTML is revalidated on every load with its ETag, a 304 when unchanged. 0 to 31536000; `public`, or `private` with `basic_auth` |
| `listing` | `false` | HTML listing for directories without an index |
| `dotfiles` | `false` | Serve dotfiles (`.well-known` is always served) |
| `precompressed` | `true` | Serve `file.br` / `file.gz` when the client accepts them |
| `basic_auth` | none | `"user:password"`; keep the config file private (0600) |
| `headers` | `{}` | Extra response headers, e.g. `{ "X-Frame-Options" = "DENY" }` |
| `access_log` | `false` | One stdout line per request (method, path, status, bytes, ms) |
| `cache_size` | `"16MB"` | Per worker: prebuilt responses of small files in memory (`0` = off); bodies of 8 KB and up are kept in memfds and sent with `sendfile` (one descriptor each, at most 1/8 of the open-files limit) |
| `cache_max_file` | `"64KB"` | Larger files are not cached (sent with `sendfile`); at most 16M, since a miss reads the whole file into memory first |
| `cache_valid_ms` | `1000` | A cached file is re-checked on disk at most this often |

On macOS, `warden serve` checks static paths with realpath instead of
`openat2` (same confinement, slower); see [`platforms.md`](platforms.md).

## See also

- [`benchmarks.md`](benchmarks.md): `warden serve` against nginx, `pm2 serve` and `serve`.
- [`troubleshooting.md`](troubleshooting.md#static-files-warden-serve): static serving problems.
