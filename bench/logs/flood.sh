#!/bin/sh
# Flood worker for bench/logs.ts: LOG_MB megabytes of 101-byte lines to
# stdout with plain blocking writes, so it goes exactly as fast as the
# process manager reads the pipe. Then records the time and idles.
line="w${WARDEN_WORKER_ID:-${NODE_APP_INSTANCE:-0}} xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx flood"
bytes=$(( ${LOG_MB:-200} * 1048576 ))
start=$(date +%s%N)
yes "$line" | head -c "$bytes"
end=$(date +%s%N)
echo "{\"lines\":$(( bytes / (${#line} + 1) )),\"bytes\":$bytes,\"ms\":$(( (end - start) / 1000000 ))}" > "$LOG_DONE_DIR/done-$$"
exec sleep 100000
