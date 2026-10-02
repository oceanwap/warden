# Platforms

- **Linux x86_64 and ARM64**: supported for production. CI runs the unit
  and integration tests on both (and formatting and clippy on x86_64).
- **macOS**: for development only, to run an app locally with the same
  config. CI builds it and runs clippy and the unit tests (Apple Silicon).
  What it lacks:
  - `SO_REUSEPORT` does not load-balance on macOS: several workers can
    share the port, but connections are not spread across them. Use
    `[workers] count = 1` locally.
  - Node has no `reusePort` on macOS at all (libuv offers it only where the
    kernel balances), so only one Node worker can listen on the port, and a
    reload, which starts the new worker next to the old one, fails: use
    `warden restart --hard`, or `port_strategy = "offset"`. Warden says so
    at start. Bun apps share the port as on Linux.
  - No parent-death signal: workers outlive a supervisor killed with SIGKILL.
  - `warden serve` checks static paths with realpath instead of `openat2`
    (same confinement, slower).
  - No out-of-memory attribution: a worker the kernel killed for memory
    shows as `killed by SIGKILL`, not as an OOM kill (macOS has no cgroup
    events to read).
  - CPU, memory, the user, listening ports, a process's environment and
    the host's numbers come from macOS's own interfaces (libproc, `sysctl`
    and Mach calls) through a small adapter per OS (`src/platform/`), so
    the CPU and mem columns, readiness by port, wardend's `host` events and
    its restart of a supervisor with the environment it was started with
    all work as on Linux. `warden doctor` says what the OS you are on
    cannot do.
- **Windows**: not supported, and not planned for 0.1. Warden is built on
  Unix sockets, signals, process groups and `SO_REUSEPORT`, none of which
  Windows has in the same form. **WSL2 works as Linux** (it is a real Linux
  kernel): follow the Linux instructions inside the distribution, and keep
  the app on the Linux filesystem. [windows.md](windows.md) has the
  tips and the plan for a native build, with the one hard problem (sharing a
  port between workers) spelled out.

## Release archives and what they need

Prebuilt archives are attached to each
[GitHub Release](https://github.com/oceanwap/warden/releases). Each release
has two products for Linux (x86_64, arm64) and macOS (arm64, x86_64):

- **CLI only**: `warden-<version>-<os>-<arch>.tar.gz` with the `warden`
  binary, the README and `contrib/` (systemd units, sysctl file, nginx site). The Linux
  binaries need glibc 2.28 or newer (RHEL 8, Debian 10, Ubuntu 18.10+).
- **GUI + CLI**: `warden-gui-<version>-<os>-<arch>` (`.tar.gz` on Linux, a
  zipped `Warden.app` on macOS) with `warden-gui` next to `warden` (see
  [the GUI](../gui/README.md)). On Linux the GUI links only glibc (2.35+: built on Ubuntu
  22.04) and loads the display libraries at run time, which every desktop
  has: `libxkbcommon`, then on Wayland `libwayland-client` and
  `libwayland-cursor`, on X11 `libX11`, `libX11-xcb`, `libXcursor`, `libXi`
  and `libxkbcommon-x11`. It draws on the CPU
  (tiny-skia): no GPU, Vulkan or OpenGL driver is needed. macOS: the app
  is not notarized by Apple yet, so the first open is refused ("Apple could
  not verify…"); then System Settings → Privacy & Security → Open Anyway
  (macOS 15 dropped the right-click → Open shortcut), or
  `xattr -dr com.apple.quarantine /Applications/Warden.app`. The CLI
  installed by `install.sh` is not affected.

How to download, verify and install them: [install.md](install.md).

## See also

- [`how-it-works.md`](how-it-works.md#limitations): the shim and its limits.
- [`production.md`](production.md): systemd units on Linux, launchd on macOS.
- [`windows.md`](windows.md): Windows status and plan.
