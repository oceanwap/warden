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

## Files are not cached

Like nginx (without its optional `open_file_cache`), Warden keeps no copy of
the files it serves: the operating system's page cache already holds what is
used, and a copy of its own would be one more thing to keep in step with the
disk, to size and to lose a big file's worth of memory to. Every request
opens the file (`openat2`, which also keeps it inside the root), looks at it
(`statx`), reads it (`preadv2` for files up to 16 KB, which go out with their
head in one `send`) or hands it to the kernel (`sendfile`, for bigger ones,
after the head), and closes it. An edit, a deletion or a symlink swapped in
shows at once. Nothing here grows with the size of the site, and a file of
any size is served the same way.

That costs little: server CPU for a 1 KB file is 7.8 µs per request on a
kept-alive connection and 14.2 µs on a new one, against 14.3 and 19.2 for
nginx on the same machine (100 KB: 18.2 and 24.0, against 20.1 and 25.5; see
[benchmarks.md](benchmarks.md)).

## The response cache (optional)

`cache_size = "16MB"` turns on a cache of complete responses. Each worker
then keeps the headers and body of recently used files up to
`cache_max_file` (64 KB), in at most `cache_size` (least recently used out
first), so a hit is one system call: a `send(2)` from memory, or, for a body
of 8 KB or more, a `sendfile(2)` from a sealed in-memory file (memfd), where
the kernel takes the pages by reference instead of copying them. It saves
the file's four system calls: 1 KB on a kept-alive connection 6.3 µs of CPU
instead of 7.8, on a new one 12.6 instead of 14.2, so 10 to 20 % on small
files and nothing on big ones. HEAD and 304s come from the same entry;
ranges and anything unusual take the normal path. A cached file is checked
against the disk at most every `cache_valid_ms` (1 s): an edit, a deletion
or a symlink swapped in shows within that time (on NFS, within the
attribute cache time). A file changed in the last 2 s is served but not
cached. A deploy that swaps a `current` symlink needs a rolling restart
anyway (each worker resolves the root once), which starts with an empty
cache. With `access_log = true` each line ends in `cache=hit` or
`cache=miss`, and each worker prints its cache counters when it stops.

## Connections

Requests are read into a buffer that is reused from one connection to the
next, and parsed where they lie. A new connection whose request has already
arrived (Linux accepts a connection only once it has data) is answered from
the accept loop itself, `accept`, `recv`, the file's calls, `send`,
`close`, with no task and no epoll registration; a kept-alive connection then
continues as a task. That holds as long as nothing has to wait: a file that
is not in the kernel's dentry cache (opened on a thread instead), a read
the disk has to serve, a request that is still arriving, or a socket with no
room for the response (the first 256 KB of a file go out at once, the rest as a
task, which sends big files in pieces and lets its other connections run
between them) are handed on as they stand to a task. Where this does not apply
(macOS) nothing else changes.

A connection waiting for its first request may take 10 s, and one waiting
between requests on a kept-alive connection 15 s (a connection whose first
request was answered from the accept loop counts as kept-alive); then it is
closed. One task checks all connections once a second, so the limit holds to
within about a second, and never ends early. A response being sent has no
limit. If the worker itself stalled for a few seconds (a hung disk, a stopped
process), every wait starts over instead of closing connections that were
never idle. The accept loop gives the thread back every 128 connections, so
a backlog that never empties cannot starve the connections already open, the
heartbeat or SIGTERM. When the worker stops it prints how many requests the
accept loop answered (and, with a cache, what the cache did).

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
| `cache_size` | `0` | Per worker: prebuilt responses of small files in memory, e.g. `"16MB"` (`0` = off, the default: files are read from the OS page cache on every request); bodies of 8 KB and up are kept in memfds and sent with `sendfile` (one descriptor each, at most 1/8 of the open-files limit) |
| `cache_max_file` | `"64KB"` | With a cache: larger files are not cached (sent with `sendfile`); at most 16M, since a miss reads the whole file into memory first |
| `cache_valid_ms` | `1000` | With a cache: a cached file is re-checked on disk at most this often |

On macOS, `warden serve` checks static paths with realpath instead of
`openat2` (same confinement, slower); see [`platforms.md`](platforms.md).

## See also

- [`benchmarks.md`](benchmarks.md): `warden serve` against nginx, `pm2 serve` and `serve`.
- [`troubleshooting.md`](troubleshooting.md#static-files-warden-serve): static serving problems.
