#!/usr/bin/env python3
"""Turn one libtest log into TSV rows: suite, test, status, reason.

libtest's human output is the only format available on stable (`--format json`
is nightly-only), so this parses the human output. With `--test-threads=1` the
`test NAME ... ok|FAILED` lines are emitted in order and the panic text is
buffered into the trailing `---- NAME stdout ----` blocks, so the two can be
joined without ambiguity.
"""
import re
import sys

STATUS_LINE = re.compile(r"^test ([A-Za-z0-9_:]+) \.\.\. (ok|FAILED|ignored)\b")
BLOCK = re.compile(r"^---- ([A-Za-z0-9_:]+) stdout ----\s*$")
PANIC = re.compile(r"^thread '.*' panicked at [^\n]*:\s*$")


def main() -> int:
    suite, path = sys.argv[1], sys.argv[2]
    with open(path, "r", errors="replace") as fh:
        lines = fh.read().splitlines()

    order, status = [], {}
    for line in lines:
        m = STATUS_LINE.match(line)
        if m:
            name = m.group(1)
            if name not in status:
                order.append(name)
            status[name] = {"ok": "PASS", "FAILED": "FAIL", "ignored": "SKIP"}[m.group(2)]

    # Reasons, from the buffered stdout blocks at the end of the run.
    reasons, cur, buf = {}, None, []

    def flush():
        if cur is None:
            return
        text = [ln.rstrip() for ln in buf]
        # Prefer the line(s) right after the panic header; fall back to any
        # non-empty line that is not libtest noise.
        pick = []
        for i, ln in enumerate(text):
            if PANIC.match(ln):
                pick = [x for x in text[i + 1 : i + 5] if x.strip()]
                break
        if not pick:
            pick = [
                x
                for x in text
                if x.strip() and not x.startswith("note:") and not x.startswith("thread '")
            ][:3]
        reasons[cur] = " | ".join(x.strip() for x in pick)[:400]

    for line in lines:
        m = BLOCK.match(line)
        if m:
            flush()
            cur, buf = m.group(1), []
            continue
        if cur is not None:
            if line.startswith("failures:") or line.startswith("test result:"):
                flush()
                cur, buf = None, []
                continue
            buf.append(line)
    flush()

    # A suite whose process died (timeout, panic in main) leaves started-but-
    # unreported tests. libtest prints the status line only on completion, so a
    # test that never reported is recorded as TIMEOUT rather than silently lost.
    started = re.compile(r"^test ([A-Za-z0-9_:]+) \.\.\. $")
    for line in lines:
        m = started.match(line)
        if m and m.group(1) not in status:
            order.append(m.group(1))
            status[m.group(1)] = "TIMEOUT"

    if not order:
        print(f"{suite}\t<suite-did-not-run>\tERROR\tno test lines in log")
        return 0

    for name in order:
        print(f"{suite}\t{name}\t{status[name]}\t{reasons.get(name, '')}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
