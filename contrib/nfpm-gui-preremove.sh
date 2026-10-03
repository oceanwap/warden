#!/bin/sh
# Runs before the warden-gui package is removed (deb: prerm, rpm: %preun). The
# package holds the CLI too, so /usr/bin/warden goes with it. Like the warden
# package's note (contrib/nfpm-preremove.sh) it only prints, and changes
# nothing: it never stops, disables or deletes anything.
#
# deb: $1 is "remove" when the package is going away ("upgrade" and others:
#      stay quiet); "remove in-favour PKG" when a package that has
#      /usr/bin/warden too takes its place: quiet
# rpm: $1 is 0 when the last copy is removed (1 on an upgrade)
case "${1:-}" in
    remove | 0) ;;
    *) exit 0 ;;
esac
[ "${2:-}" != in-favour ] || exit 0

# What `sudo warden startup` writes for the system manager.
found=
for unit in /etc/systemd/system/warden@.service /etc/systemd/system/wardend.service; do
    if [ -e "$unit" ]; then
        found="$found
    $unit"
    fi
done
[ -n "$found" ] || exit 0

printf '%s\n' \
    "warden-gui: this host was set up with \`warden startup\`; its systemd units run /usr/bin/warden," \
    "  which goes with this package, so unless the warden package is installed in its place," \
    "  apps will not come back after the next boot:$found" \
    "  Nothing was changed here, and running apps keep running. To undo the setup run" \
    "  \`sudo warden unstartup\` (install warden or warden-gui again first if it is already gone)," \
    "  or \`systemctl disable --now wardend.service\`, \`systemctl disable warden@<app>\` for" \
    "  each app, and delete the unit files."
exit 0
