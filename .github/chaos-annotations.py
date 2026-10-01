#!/usr/bin/env python3
"""Turn a `cargo xtask chaos` JSON report into GitHub annotations.

Job logs and artifacts need a GitHub login to read; annotations don't. Three
notices (the verdict with the run's settings, recovery per fault kind,
requests and CLI latency) and one error per invariant violation (at most 7:
GitHub keeps 10 of each kind per step).
Usage: chaos-annotations.py LABEL REPORT.json
"""
import json
import sys

MAX_CHARS = 3800
MAX_ERRORS = 7


def escape(s: str) -> str:
    return s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def escape_prop(s: str) -> str:
    return escape(s).replace(":", "%3A").replace(",", "%2C")


def emit(kind: str, title: str, text: str) -> None:
    if len(text) > MAX_CHARS:
        text = text[: MAX_CHARS - 20] + "\n(cut)"
    print(f"::{kind} title={escape_prop(title)}::{escape(text)}")


def ms(v) -> str:
    return "-" if v is None else f"{v:.0f}"


def main() -> int:
    label, path = sys.argv[1], sys.argv[2]
    try:
        r = json.load(open(path, encoding="utf-8"))
    except (OSError, ValueError) as e:
        emit("error", f"{label}: no report", f"{path}: {e} (the run failed before writing it; see the log)")
        return 0
    inv = r.get("invariants", {})
    failed = [k for k, v in inv.items() if not v.get("pass")]
    verdict = "PASS" if not failed and not r.get("violations") else f"FAIL ({len(r.get('violations', []))} violations)"
    faults = r.get("faults", [])
    lines = [
        f"{verdict}: seed {r.get('seed')}, {r.get('minutes')} min of faults ({r.get('duration_s', 0):.0f} s), "
        f"{r.get('build')} build, tcp_migrate_req = {r.get('tcp_migrate_req')}, "
        f"{'own pid namespace' if r.get('pid_namespace') else 'no pid namespace'}",
        f"apps: {', '.join(r.get('apps', []))}",
        f"faults: {len(faults)} injected, {sum(1 for f in faults if f.get('skipped'))} skipped",
    ]
    lines += [f"left out: {n}" for n in r.get("left_out", [])]
    lines.append("invariants: " + "; ".join(f"{k} {'ok' if v.get('pass') else 'FAIL ' + str(v.get('violations'))}" for k, v in inv.items()))
    alerts = r.get("alerts", {})
    if alerts:
        lines.append("alerts delivered: " + ", ".join(f"{k} {v}" for k, v in sorted(alerts.items())))
    emit("notice", f"{label}: {verdict.split(' ')[0]}", "\n".join(lines))

    rows = ["fault|n|skipped|recovered|p50 ms|p99 ms|max ms|req allowed|req violations"]
    for k, v in sorted(r.get("fault_kinds", {}).items()):
        rows.append(
            f"{k}|{v.get('injected')}|{v.get('skipped')}|{v.get('recovered')}/{v.get('injected')}|"
            f"{ms(v.get('recovery_ms_p50'))}|{ms(v.get('recovery_ms_p99'))}|{ms(v.get('recovery_ms_max'))}|"
            f"{v.get('requests_allowed')}|{v.get('requests_violations')}"
        )
    emit("notice", f"{label}: recovery per fault (from the end of the injection to all ready)", "\n".join(rows))

    reqs = r.get("requests", [])
    ok = sum(x.get("ok", 0) for x in reqs)
    allowed = sum(sum(x.get("allowed", {}).values()) for x in reqs)
    bad = sum(x.get("violations", 0) for x in reqs)
    lines = [f"requests: {ok} answered, {allowed} lost within the allowances, {bad} violations"]
    for x in reqs:
        if x.get("allowed") or x.get("violations"):
            why = "; ".join(f"{n} {w}" for w, n in x.get("allowed", {}).items())
            lines.append(f"  {x['app']} {x['client']}: ok {x['ok']}, allowed {sum(x.get('allowed', {}).values())} ({why}), violations {x.get('violations')}")
    ll = r.get("long_lived", [])
    if ll:
        lines.append(
            f"websocket/sse: {sum(x['sessions'] for x in ll)} sessions, {sum(x['clean'] for x in ll)} clean, "
            f"{sum(x['unplanned'] for x in ll)} with their worker killed, {sum(x['violations'] for x in ll)} violations"
        )
    for cmd, v in r.get("cli_latency", {}).items():
        lines.append(f"warden {cmd}: p50 {v.get('p50_ms') or 0:.1f} ms, p99 {v.get('p99_ms') or 0:.1f} ms ({v.get('samples')} samples, {v.get('excluded')} excluded)")
    emit("notice", f"{label}: requests, connections, CLI latency", "\n".join(lines))

    for v in r.get("violations", [])[:MAX_ERRORS]:
        t = "" if v.get("t") is None else f" at {v['t']:.1f}s"
        emit("error", f"{label}: {v.get('invariant')}{t}", f"{v.get('app') or ''} {v.get('what')}".strip()[:1500])
    return 0


if __name__ == "__main__":
    sys.exit(main())
