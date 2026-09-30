#!/bin/bash
echo "== 2 Nest processes, no preload:"
pids=(); for i in 1 2; do PORT=3400 bun main.ts 2>&1 | head -3 & pids+=($!); sleep 1.5; done; sleep 0.5
bun count2.ts 60 3400; pkill -P $$ bun; kill ${pids[@]} 2>/dev/null; sleep 0.5
echo "== 2 Nest processes, with preload:"
pids=(); for i in 1 2; do PORT=3401 bun --preload ./preload.ts main.ts & pids+=($!); sleep 1.5; done; sleep 0.5
bun count2.ts 60 3401; t0=$(date +%s%N); kill -TERM ${pids[0]}; wait ${pids[0]}; echo "SIGTERM -> exit code $? after $(( ($(date +%s%N)-t0)/1000000 ))ms"; kill ${pids[@]} 2>/dev/null; wait 2>/dev/null
echo "== 1 process, 2 Nest Workers (Bun.serve patched in worker):"
PORT=3402 N=2 bun nhost.ts & H=$!; sleep 3
bun count2.ts 60 3402; echo "RSS: $(grep VmRSS /proc/$H/status)"; kill $H; wait 2>/dev/null
