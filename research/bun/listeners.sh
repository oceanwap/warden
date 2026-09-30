#!/bin/bash
# count LISTEN sockets on port $2 owned by pid $1 (via /proc/net/tcp{,6})
pid=$1; port=$(printf '%04X' $2)
inodes=$(ls -l /proc/$pid/fd 2>/dev/null | grep -o 'socket:\[[0-9]*\]' | grep -o '[0-9]*' | sort -u)
n=0; for f in /proc/net/tcp /proc/net/tcp6; do [ -r $f ] || continue; while read -r sl local rem st q tm rt uid to inode rest; do
  [[ "$st" == "0A" && "${local##*:}" == "$port" ]] && grep -qx "$inode" <<<"$inodes" && n=$((n+1)); done < <(tail -n +2 $f); done; echo $n
