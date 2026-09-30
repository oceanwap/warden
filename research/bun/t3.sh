#!/bin/bash
bun h3.ts 3>fd3.out > h3.log 2>&1 & H=$!; sleep 1.2
echo "fd3: $(cat fd3.out)"; bun count2.ts 40 3210
for i in $(seq 1 10); do curl -s -m 1 localhost:3210/throw > /dev/null; sleep 0.3; grep -q closed h3.log && break; done
grep closed h3.log; echo "after crash with exit-hook stop(): $(bun count2.ts 100 3210)"
kill $H; wait 2>/dev/null
