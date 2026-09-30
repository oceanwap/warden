#!/bin/bash
export PORT=3200
bun host.ts > host.log 2>&1 & H=$!; sleep 1.5
echo "listeners before: $(./listeners.sh $H 3200) rss=$(grep VmRSS /proc/$H/status)"; bun count2.ts 200 3200
curl -s -m 1 localhost:3200/throw; echo; sleep 0.5; echo "after uncaught throw:"; cat host.log | grep -v listening; bun count2.ts 200 3200
for i in $(seq 1 20); do curl -s -m 1 localhost:3200/exit >/dev/null; sleep 0.3; grep -q 'close code' host.log && break; done
grep close host.log; echo "listeners after one worker exited: $(./listeners.sh $H 3200)"; bun count2.ts 200 3200
kill $H; wait 2>/dev/null
