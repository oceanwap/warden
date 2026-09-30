#!/bin/bash
pids=(); for i in 1 2; do REUSE=0 PORT=3302 bun --preload ./preload.ts nh.mjs & pids+=($!); sleep 0.4; done; sleep 0.5
echo "node:http with Bun.serve-wrapping preload -> $(bun count2.ts 100 3302)"; kill ${pids[@]} 2>/dev/null; wait 2>/dev/null
