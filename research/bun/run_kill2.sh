#!/bin/bash
pids=()
for i in 1 2 3 4; do WID=$i GRACEFUL=$2 SLOW=5 bun srv2.ts & pids+=($!); done
sleep 1
( sleep 2; kill -$1 ${pids[1]} ) &
bun load.ts 50 5000 2>&1 | tail -1
kill ${pids[@]} 2>/dev/null; wait 2>/dev/null
