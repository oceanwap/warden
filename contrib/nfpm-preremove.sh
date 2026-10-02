#!/bin/sh
# Runs before the warden package is removed (deb: prerm, rpm: %preun). It only
# prints a note and changes nothing: it never stops, disables or deletes
# anything. nfpm.yaml explains why a package does not touch the host.
#
# deb: $1 is "remove" when the package is going away ("upgrade" and others: stay quiet)
# rpm: $1 is 0 when the last copy is removed (1 on an upgrade)
case "${1:-}" in
    remove | 0) ;;
    *) exit 0 ;;
esac

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
    "warden: this host was set up with \`warden startup\`; its systemd units run /usr/bin/warden," \
    "  which is being removed, so apps will not come back after the next boot:$found" \
    "  Nothing was changed here, and running apps keep running. To undo the setup run" \
    "  \`sudo warden unstartup\` (install this package again first if it is already gone)," \
    "  or \`systemctl disable --now wardend.service\`, \`systemctl disable warden@<app>\` for" \
    "  each app, and delete the unit files."
exit 0
