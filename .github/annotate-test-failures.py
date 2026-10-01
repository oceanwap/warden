#!/usr/bin/env python3
"""Turn failed `cargo test` output into GitHub annotations.

Job logs need a login to read; annotations don't. One annotation per failed
test, carrying its captured output (newlines escaped, as workflow commands
require), so a failure can be diagnosed from the public API alone.
Usage: annotate-test-failures.py LOG...
"""
import re
import sys

MAX_TESTS = 8
MAX_CHARS = 3500


def escape(s: str) -> str:
    return s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def escape_prop(s: str) -> str:
    """Workflow command properties also escape ':' and ','."""
    return escape(s).replace(":", "%3A").replace(",", "%2C")


def blocks(text: str):
    """(test name, captured output) for every `---- name stdout ----` block."""
    current, lines = None, []
    for line in text.splitlines():
        m = re.match(r"^---- (\S+) stdout ----$", line)
        if m or line.startswith("failures:") or line.startswith("test result:"):
            if current:
                yield current, "\n".join(lines).strip()
            current, lines = (m.group(1) if m else None), []
            continue
        if current:
            lines.append(line)
    if current:
        yield current, "\n".join(lines).strip()


def main() -> int:
    shown = 0
    for path in sys.argv[1:]:
        try:
            text = open(path, encoding="utf-8", errors="replace").read()
        except OSError:
            continue
        failed = re.findall(r"^test (\S+) \.\.\. FAILED$", text, re.M)
        out = dict(blocks(text))
        for name in failed:
            if shown >= MAX_TESTS:
                return 0
            body = out.get(name, "(no captured output)")
            if len(body) > MAX_CHARS:
                # The panic message is usually near the top; the end shows the last state.
                body = body[: MAX_CHARS // 2] + "\n…\n" + body[-MAX_CHARS // 2 :]
            print(f"::error title={escape_prop(path + ' ' + name)}::{escape(body)}")
            shown += 1
        for m in re.finditer(r"^error(\[\w+\])?: .*$", text, re.M):
            if shown >= MAX_TESTS:
                return 0
            print(f"::error title={escape_prop(path + ' build')}::{escape(m.group(0))}")
            shown += 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
