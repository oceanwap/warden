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

## Compression

A request whose `Accept-Encoding` takes `br` or `gzip` (a quality of 0 refuses
it, and names may be in any letter case) is answered with a compressed copy of
the file when there is one: sent with `Content-Encoding`, an `ETag` of its own
and `Vary: Accept-Encoding`; brotli is preferred, and the `Content-Type` is the
original file's. When there is none, the request is not an error: the file is
sent as it is. A request with a `Range` always gets the file as it is.

Copies come from two places, looked in this order: the ones **Warden makes in
the background**, and **`file.br` / `file.gz` siblings** that your build made.

### Made in the background

This is on by default (`compress = true`). The first request for a file that
has no copy is answered at once with the file as it is, and queues a job for
it. A few jobs at a time (`compress_jobs`, one per CPU core by default, all
workers of a site together) make the `.br` and `.gz` copies, in a process of
their own at the lowest CPU and disk priority: brotli at its best level for
files up to 1 MB (about 1.5 s of CPU per MB), level 9 above that. The next
request is answered from the copy. A request is never made to wait for a
compression, and nothing is compressed on the fly.

- **Where the copies are.** In `compress_dir`, a private folder (mode 0700,
  yours, never inside `root`: Warden refuses one that is not), by default a
  folder of Warden's state directory named after the app. The folder you serve
  is not touched. It holds at most about `compress_dir_size` (256 MB); the
  oldest copies go first.
- **A copy is never out of date.** The name of a copy says which version of the
  file it was made from: device, inode, size, and modification and change
  times. A request opens the file it is about to send anyway, so it knows its
  version, and asks for the copy of exactly that one. A file that was edited,
  replaced (a new deploy) or touched has no copy until a new one is made, and
  the request in between gets the file as it is: never an old copy, never
  a wrong `ETag`. When the new copy is in place the old ones are removed. There
  is nothing to hash or compare on the request path.
- **What is compressed.** Files between `compress_min_file` (1 KB) and
  `compress_max_file` (8 MB), that are not in `precompressed_skip` (images,
  fonts, audio, video, archives, PDF and office documents), have not changed in
  the last two seconds (a file that is still being uploaded waits), and shrink:
  a copy that is not smaller than the file is not kept. A file with a sibling
  of its own (below) is left alone.
- **Folders that change at runtime** (an uploads folder) work: a file is
  compressed when it is first asked for, whenever that is.
- **What it costs.** The worker stays one thread, and a request that does not
  take a compression costs what it did without this (about 8.3 µs of server CPU for
  a 1 KB file either way). A request answered from a copy opens one more file
  than one answered with the file as it is: for a 20 KB stylesheet 13.3 µs
  against 11.9, and 2.8 KB sent instead of 20 KB. A compression takes tens of MB
  while it runs, in its own process, so the server's memory does not move, and
  the processes ask for nothing a busy CPU needs: with one pinned to the core
  of a worker serving 120 000 requests a second, throughput fell by 3 to 4 %
  and the 99th percentile latency did not move.
- **Off.** `compress = false` turns it off; `precompressed = false` turns off
  every compressed copy, yours included.

With `cache_size` (the response cache), a response is not kept while a copy of
its file is on the way.

### Made by your build

A `file.br` or `file.gz` next to `file` is sent in the same way when the file
has no copy of Warden's making, and Warden then makes none of that file. Use
this for what you want compressed by your own settings (slower, smaller), or
when you want every copy to exist before the first request:

```sh
# every text file, once, next to the original (keeps the original)
find dist -type f \( -name '*.js' -o -name '*.css' -o -name '*.html' -o -name '*.svg' -o -name '*.json' \) \
  -exec gzip -9 -k -f {} \; -exec brotli -f -k {} \;
```

or with your bundler's plugin (`vite-plugin-compression`, webpack's
`compression-webpack-plugin`). A request that takes a compression looks for the
sibling of every file whose extension is not in `precompressed_skip`: two failed
lookups (about 4 µs of CPU) when there is none, which is why the list exists
and a list of your own replaces it.

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
| `precompressed` | `true` | Serve compressed copies of files when the client accepts them: the ones `compress` makes, and `file.br` / `file.gz` siblings you made. A request that accepts them looks for the sibling of every file whose extension is not in `precompressed_skip`: two failed lookups (about 4 µs of CPU) when there is none, so a site without precompressed files, and with `compress = false`, can set `false` |
| `precompressed_skip` | images, fonts, audio, video, archives, PDF and office documents (`png`, `jpg`, `jpeg`, `gif`, `webp`, `avif`, `heic`, `jxl`, `woff`, `woff2`, `mp3`, `m4a`, `aac`, `flac`, `ogg`, `opus`, `mp4`, `m4v`, `mov`, `mkv`, `webm`, `zip`, `gz`, `br`, `zst`, `bz2`, `xz`, `7z`, `rar`, `tgz`, `jar`, `apk`, `pdf`, `docx`, `xlsx`, `pptx`, `odt`, `ods`, `odp`, `epub`) | File extensions (last one of a name, any case, a dot in front is fine) that are never looked up for a `.br` / `.gz` sibling: formats that are compressed already, where a sibling saves nothing. A list **replaces** the default (to add one, write the default and yours); `[]` looks up every file |
| `compress` | `true` | Make `.br` and `.gz` copies of files in the background, from the first request for them: that request gets the file as it is, the next ones the copy (see [Compression](#compression)). Needs `precompressed`. Never for the formats in `precompressed_skip` |
| `compress_jobs` | `0` | Compressions running at once, all workers of the site together, each at the lowest priority; `0`: one per CPU core |
| `compress_dir` | `<state dir>/compress/<app name>` | Where the copies are kept: a private folder (made with mode 0700; refused when it is not yours, is open to others, or lies inside `root`), so `root` stays as it is. A relative path is read from the working directory, else the config file's folder |
| `compress_dir_size` | `"256MB"` | About this much room for copies; the oldest are removed first |
| `compress_min_file` | `"1KB"` | Smaller files are sent as they are |
| `compress_max_file` | `"8MB"` | Larger files are sent as they are (at most 64M) |
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
