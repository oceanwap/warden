#!/usr/bin/env python3
"""Turn bench/results/latest.md (`cargo xtask bench`) into GitHub annotations.

Job logs and artifacts need a GitHub login to read; annotations don't. One
`::notice` for the machine, then one per suite: its table, compacted (no
padding, no separator row) and cut at a row boundary to fit in ~3.8 KB.
GitHub keeps at most 10 notices per step, so: the machine + 8 suites.
Usage: bench-annotations.py LABEL [FILE]
"""
import sys

MAX_CHARS = 3800


def escape(s: str) -> str:
    return s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def escape_prop(s: str) -> str:
    return escape(s).replace(":", "%3A").replace(",", "%2C")


def compact(line: str) -> str:
    """A Markdown table row without its padding: `|a|b|c|`."""
    cells = [c.strip() for c in line.strip().strip("|").split("|")]
    return "|" + "|".join(cells) + "|"


def section_body(lines):
    """Text lines as they are, table rows compacted, separator rows dropped."""
    out = []
    for l in lines:
        s = l.strip()
        if not s:
            continue
        if s.startswith("|") and set(s) <= set("|-: "):
            continue
        out.append(compact(s) if s.startswith("|") else s)
    return out


def fit(lines, limit=MAX_CHARS):
    text, kept = "", 0
    for l in lines:
        if len(text) + len(l) + 1 > limit:
            break
        text += l + "\n"
        kept += 1
    if kept < len(lines):
        text += f"(cut: {len(lines) - kept} more lines in the job summary and the artifact)"
    return text.rstrip("\n")


def main() -> int:
    label = sys.argv[1] if len(sys.argv) > 1 else "bench"
    path = sys.argv[2] if len(sys.argv) > 2 else "bench/results/latest.md"
    try:
        text = open(path, encoding="utf-8").read()
    except OSError as e:
        print(f"::error title={escape_prop(label + ': no results')}::{escape(str(e))}")
        return 0
    head, *sections = text.split("\n### ")
    intro = [l for l in head.splitlines() if l.strip() and not l.startswith("<!--")]
    print(f"::notice title={escape_prop(label + ': machine')}::{escape(fit(intro))}")
    for sec in sections:
        title, _, rest = sec.partition("\n")
        lines = rest.splitlines()
        # The suite's one-paragraph description is in the job summary; keep the numbers.
        if lines and not lines[0].strip():
            lines = lines[1:]
        if lines and not lines[0].lstrip().startswith("|"):
            lines = lines[1:]
        body = fit(section_body(lines))
        print(f"::notice title={escape_prop(label + ': ' + title.strip())}::{escape(body)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
