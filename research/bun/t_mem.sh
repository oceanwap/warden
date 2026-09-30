#!/bin/bash
AC=./node_modules/.bin/autocannon
pids=(); for i in 1 2 3 4; do PORT=3200 bun wsrv.ts >/dev/null 2>&1 & pids+=($!); done; sleep 1.5
tot=0; for p in ${pids[@]}; do r=$(awk '/VmRSS/{print $2}' /proc/$p/status); tot=$((tot+r)); done; echo "4 processes idle RSS total: $((tot/1024)) MB"
$AC -c 8 -d 5 --json http://127.0.0.1:3200/cpu 2>/dev/null | bun -e 'const j=JSON.parse(await Bun.stdin.text()); console.log("4 procs /cpu rps=",j.requests.average, "p99=",j.latency.p99)'
kill ${pids[@]}; wait 2>/dev/null
N=4 PORT=3200 bun host.ts >/dev/null 2>&1 & H=$!; sleep 1.5
echo "1 process x 4 workers idle RSS: $(( $(awk '/VmRSS/{print $2}' /proc/$H/status)/1024 )) MB"
$AC -c 8 -d 5 --json http://127.0.0.1:3200/cpu 2>/dev/null | bun -e 'const j=JSON.parse(await Bun.stdin.text()); console.log("4 workers /cpu rps=",j.requests.average, "p99=",j.latency.p99)'
kill $H; wait 2>/dev/null
N=1 PORT=3200 bun host.ts >/dev/null 2>&1 & H=$!; sleep 1.5
$AC -c 8 -d 5 --json http://127.0.0.1:3200/cpu 2>/dev/null | bun -e 'const j=JSON.parse(await Bun.stdin.text()); console.log("1 worker /cpu rps=",j.requests.average, "(baseline, 20ms busy => max 50 rps/core)")'
kill $H; wait 2>/dev/null
