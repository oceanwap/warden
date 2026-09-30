#!/bin/bash
for R in 0 1; do
  pids=(); for i in 1 2; do REUSE=$R PORT=3300 bun nh.mjs & pids+=($!); sleep 0.4; done; sleep 0.5
  echo "bun node:http, 2 processes, reusePort=$R -> $(bun count2.ts 100 3300)"; kill ${pids[@]} 2>/dev/null; wait 2>/dev/null
done
echo "--- node 22 for reference (reusePort needs node >=22.12):"
pids=(); for i in 1 2; do REUSE=1 PORT=3301 node nh.mjs & pids+=($!); sleep 0.4; done; sleep 0.5; bun count2.ts 50 3301; kill ${pids[@]} 2>/dev/null; wait 2>/dev/null
