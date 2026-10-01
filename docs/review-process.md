# Code review process

Every milestone, and any change to `src/supervisor*`, `src/process.rs` or `shim/`,
gets an **independent review** against this document before it is merged or
pushed. The reviewer has not written or seen the change being made; the author
answers findings but does not grade their own work.

Warden supervises production traffic, so the bar is not "it works in the happy
path" but **"when something breaks, the operator understands what happened and
the system recovers on its own, or tells them exactly what to do."**

## 1. How the reviewer works

1. **Read** `README.md`, `docs/architecture.md`, `docs/troubleshooting.md` and
   the diff (or the whole tree for a milestone review).
2. **Build and test**: `cargo build`, `cargo clippy --all-targets`,
   `cargo test` (integration tests need `bun`). Everything must pass.
3. **Scan** for the patterns in §4 and justify or flag each hit.
4. **Run the scenario matrix** (§3) against a real `warden` with the test
   fixture (`tests/fixtures/app.ts`), reading the log and CLI output as an
   operator would. Use throwaway configs outside the repo.
5. **Try to break it**: interleave events (crash during a reload, stop during a
   restart, scale during a drain), inject faults (`WARDEN_FAULT=<point>:<n>`,
   debug builds only), kill and freeze Warden (`SIGKILL`, `SIGSTOP`).
   `cargo xtask chaos` ([`chaos.md`](chaos.md)) does this to a whole fleet
   under load for minutes, from a seed, and checks the invariants.
6. **Report** in the format of §5. Mark each finding **CONFIRMED** (reproduced
   or traced unambiguously) or **SUSPECTED**. Do not modify the repository.

## 2. Criteria

Each criterion is a question the reviewer answers with evidence (file:line,
log excerpt, or a reproduction).

### A. Logging

- A1. Every state change of a worker (starting, ready, crashed, restarting,
  FAILED, stopped, replaced, recycled) is logged **once**, at the right level:
  `INFO` for normal transitions, `WARN` for degraded-but-handled, `ERROR` for
  failures that need attention or lost functionality.
- A2. Every worker-related line carries `worker=<id>` and `pid=<pid>`; every
  rollout line can be tied to its rollout (kind, and the worker it's on).
- A3. Lines are `message key=value…`: the message is a plain sentence, values
  are fields (no values interpolated into the message that someone would want
  to grep or filter on).
- A4. No secrets: environment values, tokens and full command lines with
  credentials never reach logs, `status` or metrics.
- A5. No log storms: anything that can repeat quickly (health failures,
  heartbeats, IPC noise, retry loops) is rate-limited, deduplicated, or at
  `DEBUG`. One failing worker produces a bounded number of lines per minute.
- A6. Debug detail exists where diagnosis needs it (gate checks, readiness
  probes, IPC events) and can be switched on at runtime without a restart.
- A7. Log output can never block or crash supervision (bounded queues, drops
  counted and reported).
- A8. journald integration: priorities map correctly (`<3>` error, `<4>`
  warning, `<6>` info); no duplicated timestamps under journald.

### B. Error handling in code

- B1. No `unwrap()`, `expect()`, `panic!`, unchecked indexing or unchecked
  arithmetic on external input in non-test code. Each exception is justified in
  a comment.
- B2. Every `let _ =` / ignored `Result` on a fallible operation is either
  harmless by construction (documented in a comment) or logged.
- B3. Errors carry context: which file, socket, worker, command or config key,
  and the OS error. `"No such file or directory"` alone is a defect.
- B4. Every wait that involves a worker or an external process has a timeout
  (readiness, drain, gates, verify/preflight commands, health checks, control
  requests). Nothing waits forever on something that can hang.
- B5. Every task Warden spawns has a defined panic policy (see
  `src/guard.rs`): worker-owning → kill and restart that worker; essential
  loop → exit for systemd to restart; per-request → drop the request.
- B6. Resources are released on every error path: child processes, pipes,
  sockets, files in the runtime directory, timers.
- B7. Queues and buffers are bounded; inputs from workers (IPC, output lines)
  and from the control socket have size and rate limits.

### C. Error messages for end users

Every error an operator can see (log line, CLI output, `status` field) must
answer four questions:

1. **What happened** ("worker 2 crashed", "reload failed at worker 1").
2. **Why**, as far as Warden knows ("exit code 1", "HTTP 503 from /health",
   "no heartbeat for 60 s", "config: reload.timeout out of range").
3. **What Warden did about it** ("restarting in 200 ms", "rolled back: every
   worker still runs the previous version", "holding replacements").
4. **What the operator can do**, when action is needed: a `hint=` naming the
   exact config key, command or sysctl ("set [control] socket to a shorter
   path", "run `warden restart 2` after fixing the cause").

Plus:

- C5. Config errors name the file, the key (`section.key`) and the allowed
  range or values.
- C6. CLI exit codes are consistent and documented: 0 success, 1 the operation
  failed, 2 Warden unreachable or bad usage.
- C7. Internal identifiers (instance ids, rollout sequence numbers) are never
  the only way to identify something; use worker id and pid.
- C8. Every distinct `ERROR`/`WARN` message appears in
  `docs/troubleshooting.md` with its meaning and fix.
- C9. Wording is plain and consistent (the same thing has the same name
  everywhere: "worker", "host", "rollout", "gate").

### D. Error recovery

- D1. Every failure class has a defined recovery and it is exercised by a
  test: worker crash, crash loop, hang, leak, failed start, not ready in time,
  failed health, failed rollout gate, Warden crash, Warden hang.
- D2. **No stuck states**: there is no reachable state where a slot has no
  worker and nothing is scheduled to fix it (except STOPPED on request, or
  FAILED with a cooldown/explicit restart path), and no rollout that can wait
  forever.
- D3. Recovery does not amplify the problem: backoff, restart caps, the
  dependency-outage guard, one rollout at a time, no replacement storms.
- D4. Rollback leaves the system in the previous known-good state, including
  config and release, and says so; when rollback is impossible, it says that.
- D5. Operations are idempotent and safe to repeat: a second SIGTERM, a
  repeated `reload`, `stop` while stopping, `restart` while restarting.
- D6. Fail-fast vs. fail-safe choices are deliberate and documented (e.g.
  Warden exits on an essential task panic; workers die with Warden).
- D7. After any recovery, `status` reflects reality (pids alive, states
  correct, counters incremented) — verified against `/proc`.

### E. Process hygiene

- E1. No orphans and no zombies after worker exit, Warden exit, SIGKILL of
  Warden, or failed spawns (check `ps` after each scenario).
- E2. Signals go to the right target (process group for TERM/KILL), exactly
  once, and never to a pid that may have been reused.
- E3. File descriptors are CLOEXEC except those deliberately inherited (fd 3).
- E4. The runtime directory is private (owner, not a symlink, not
  group/world-writable) and cleaned up on exit.

### F. State machine invariants

- F1. Handlers tolerate stale or unknown ids (events for exited instances,
  timers for superseded restarts, gate results for finished rollouts).
- F2. At most N+1 worker processes per slot at any time (one replacement).
- F3. Every rollout terminates (success, failure, or abort) and records an
  outcome.
- F4. Timers are invalidated by tokens when the state they were set for is gone.

### G. Security

- G1. Control socket 0600 in a private directory; no network control plane.
- G2. Commands run only from the config file (trusted), never from worker
  input or the control socket.
- G3. Secrets redacted in any output that shows environment or config.
- G4. Size limits on all parsed input (IPC lines, control requests, HTTP
  health responses).

### H. Configuration

- H1. `warden check` catches everything detectable before start (ranges,
  paths, combinations), and `safe-reload` preflight runs the same checks.
- H2. Every numeric value has an upper bound that keeps time arithmetic safe.
- H3. Defaults are documented in `warden.example.toml` and are safe for
  production.

### I. Debuggability

- I1. `warden status` explains *why* for anything not RUNNING (last exit,
  next retry, rollout phase, health, outage guard).
- I2. `warden doctor` detects the environment problems we know about and
  prints a fix for each.
- I3. Metrics exist for every failure class (restarts, crashes, health,
  rollouts, dropped log lines, held replacements).
- I4. An operator can follow one worker's story (`warden logs --worker N`)
  and Warden's own decisions (`warden logs --events`).

### J. Tests

- J1. Each fixed defect has a regression test that fails without the fix.
- J2. Failure paths are tested, not just happy paths (fault injection,
  crash/hang/leak fixtures, bad configs).
- J3. Tests are deterministic (no reliance on lucky timing; generous
  timeouts; poll for conditions instead of sleeping) and leave no processes.

### K. Documentation

- K1. README, architecture and troubleshooting match the code; examples are
  real output.
- K2. Every limitation the reviewer finds is either fixed or documented.

### L. Resource budgets

- L1. Warden stays < 8 MB RSS and ~0 % CPU idle; nothing blocks the event
  loop (no synchronous network I/O, no unbounded synchronous file reads on hot
  paths).

## 3. Scenario matrix

For each scenario, the reviewer records what the operator sees (log lines,
`warden status`, CLI exit code) and judges it against §2 C and D.

| # | Scenario | Expected outcome |
|---|---|---|
| S1 | Config with a typo'd key / out-of-range value | `warden check` exit 1, names file + key + range |
| S2 | `app.command` not found / entry file missing | Clear start error with the path; workers retried with backoff, then FAILED with a hint |
| S3 | Port already used by another process | Workers not ready in time; message suggests checking the port |
| S4 | App crashes at start (exit 1) | Backoff, restart cap, FAILED, cooldown retry; each step logged |
| S5 | App crashes at runtime (`/exit`, `/throw`, `kill -9`) | Restarted; other workers untouched; `last_exit` explains |
| S6 | App hangs (`/hang`) | Watchdog kills and restarts it; "hung" in `last_exit` |
| S7 | App leaks memory (`/leak`, `max_memory`) | Graceful replacement, reason logged |
| S8 | Dependency outage (all workers fail health) | Replacements held, one ERROR line, recovery logged |
| S9 | Bad release via `safe-reload` (503, crash in soak, failing verify) | Rolled back, CLI exit 1 with the reason, previous config and release kept |
| S10 | Broken config during `safe-reload` | Refused in preflight, nothing touched, exit 1 |
| S11 | `SIGKILL` / `SIGSTOP` of Warden | Workers drain and exit / keep serving; watchdog pings stop |
| S12 | Stalled stdout (log consumer stops reading) | Supervision unaffected; dropped-line counter reported |
| S13 | Runtime directory world-writable or a symlink | Refuses to start with the reason and the fix |
| S14 | Control socket unreachable (Warden down) | CLI exit 2 with "Is it running?" |
| S15 | Injected panics (`WARDEN_FAULT=waiter:1`, `stdout:1`, `control:1`, `tick:3`) | Per §2 B5; no stuck slot; clear ERROR line |

## 4. Mechanical checks

```sh
cargo clippy --all-targets -- -D warnings
rg -n '\.unwrap\(\)|\.expect\(|panic!|unreachable!' src --glob '!**/tests/**'
rg -n 'let _ =' src            # each hit: harmless by construction, or logged?
rg -n 'spawn_local' src        # each task: which panic policy applies?
rg -n 'Instant::now\(\) \+|now \+ ' src   # must go through restart::later
systemd-analyze verify contrib/warden@.service   # with ExecStart pointed at /bin/true
```

### Valgrind for the `unsafe` code

Miri can't run the system calls in `src/sys.rs`, so its tests also run under Valgrind's memcheck: invalid reads and writes,
uninitialised bytes handed to a system call, definite leaks.

```sh
# 1. Build the unit-test binary (a debug build) without running it, and take
#    its path from cargo's "Executable unittests src/main.rs (…)" line.
bin=$(cargo test --bin warden --no-run 2>&1 | sed -n 's/.*Executable unittests src\/main.rs (\(.*\))$/\1/p')
echo "$bin"    # target/debug/deps/warden-<hash>
# 2. The sys:: tests under memcheck, one at a time (~30 s).
valgrind --error-exitcode=1 --leak-check=full --show-leak-kinds=definite \
  --errors-for-leak-kinds=definite --track-origins=yes --quiet \
  "$bin" sys:: --test-threads=1 --skip sys::tests::nofile_limit_matches_proc
echo $?        # 0: every test passed and memcheck found nothing
```

- Expected: `test result: ok. 52 passed` (the count grows with the tests),
  no `==PID==` lines, exit 0. Exit 1 is a memcheck error (`Invalid read`,
  `Syscall param … points to uninitialised byte(s)`, `definitely lost`):
  a finding. Exit 101 is a failed test.
- `nofile_limit_matches_proc` is skipped: Valgrind keeps 12 descriptors for
  itself and lowers `RLIMIT_NOFILE` by them, so `getrlimit` and
  `/proc/self/limits` disagree under it, by design.

## 5. Report format

Findings ordered by severity, each with:

- **Severity**: Blocker (data loss, outage, stuck state, security), High
  (wrong recovery, misleading message on a common path), Medium (degraded
  diagnosis, missing hint, rare-path defect), Low (wording, consistency).
- **Criterion** (e.g. C3, D2) and **file:line**.
- **What's wrong**, a **concrete scenario** (input → what the operator sees →
  what they should see), and a **suggested fix**.
- **CONFIRMED** or **SUSPECTED**.

End with: a table of the scenario matrix results (pass / fail / partial with a
one-line note), and anything checked and found fine that the author might
otherwise re-check.
