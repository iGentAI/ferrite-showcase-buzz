#!/usr/bin/env python3
"""Compare a run's results against the checked-in expectations and gate on it.

expectations.tsv columns:
    suite   test   expected   class   note

`expected` is PASS, FAIL or SKIP. `class` says *why* a FAIL or SKIP is
tolerated, and is the column a human reads:

    conform          the test passes; our relay implements the obligation
    nonconformance   our relay is genuinely wrong; `note` names the finding
    missing-shim     the test needs something we cannot supply without editing
                     it (Postgres seeding, upstream's X-Pubkey dev auth)
    deployment       an assertion about upstream's own deployment constants
                     (limits, wording) rather than about the protocol
    untranslatable   the test cannot mean anything against a one-community app
    infra            needs infrastructure we do not run (S3, an LLM endpoint)

Exit: 0 clean, 1 regression (expected PASS, got FAIL), 2 unexpected pass.
"""
import argparse
import sys


def read_tsv(path, width):
    rows = []
    with open(path, "r", errors="replace") as fh:
        for line in fh:
            line = line.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            parts = line.split("\t")
            parts += [""] * (width - len(parts))
            rows.append(parts[:width])
    return rows


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True)
    ap.add_argument("--expect", required=True)
    ap.add_argument("--report")
    ap.add_argument("--update", action="store_true")
    args = ap.parse_args()

    results = read_tsv(args.results, 4)
    expect_rows = read_tsv(args.expect, 5)
    expect = {(s, t): (e, c, n) for s, t, e, c, n in expect_rows}

    if args.update:
        seen = {(s, t): st for s, t, st, _ in results}
        merged = []
        for (s, t), st in seen.items():
            _, c, n = expect.get((s, t), ("", "", ""))
            if st == "PASS":
                c = "conform"
            merged.append((s, t, st, c or "unclassified", n))
        # keep rows we did not run this time (skipped suites, untranslatable)
        for (s, t), (e, c, n) in expect.items():
            if (s, t) not in seen:
                merged.append((s, t, e, c, n))
        merged.sort()
        # Keep the file's leading comment block: it carries the note-key legend,
        # which is the only place the reasons are written out in full.
        header = []
        with open(args.expect, "r", errors="replace") as fh:
            for line in fh:
                if line.lstrip().startswith("#") or not line.strip():
                    header.append(line)
                else:
                    break
        with open(args.expect, "w") as fh:
            fh.writelines(header or ["# suite\ttest\texpected\tclass\tnote\n"])
            for row in merged:
                fh.write("\t".join(row) + "\n")
        return 0

    lines, counts = [], {}
    regressions, unexpected_pass, unknown = [], [], []
    for suite, test, status, reason in results:
        exp, cls, note = expect.get((suite, test), (None, None, None))
        if exp is None:
            unknown.append((suite, test, status, reason))
            verdict = "NEW"
        elif status == exp:
            verdict = "ok"
        elif exp == "PASS" and status in ("FAIL", "TIMEOUT", "ERROR"):
            verdict = "REGRESSION"
            regressions.append((suite, test, reason))
        elif exp in ("FAIL", "SKIP") and status == "PASS":
            verdict = "UNEXPECTED-PASS"
            unexpected_pass.append((suite, test, cls))
        else:
            verdict = f"CHANGED({exp}->{status})"
            regressions.append((suite, test, f"expected {exp}, got {status}: {reason}"))
        key = cls or ("conform" if status == "PASS" else "unclassified")
        counts[key] = counts.get(key, 0) + 1
        lines.append(f"{verdict:16s} {status:8s} {suite}::{test}\t{cls or ''}\t{note or ''}")

    total = len(results)
    passed = sum(1 for r in results if r[2] == "PASS")
    out = []
    out.append(f"tests reported : {total}")
    out.append(f"passed         : {passed}")
    out.append(f"failed         : {sum(1 for r in results if r[2] in ('FAIL','TIMEOUT','ERROR'))}")
    out.append(f"skipped        : {sum(1 for r in results if r[2] == 'SKIP')}")
    out.append("")
    out.append("by class:")
    for k in sorted(counts):
        out.append(f"  {k:16s} {counts[k]}")
    out.append("")
    out.extend(lines)
    if unknown:
        out.append("")
        out.append("NOT IN EXPECTATIONS (upstream added tests, or a suite was renamed):")
        for s, t, st, r in unknown:
            out.append(f"  {st:8s} {s}::{t}  {r}")
    text = "\n".join(out)
    if args.report:
        with open(args.report, "w") as fh:
            fh.write(text + "\n")
    print(text)

    if regressions:
        print(f"\nGATE FAIL: {len(regressions)} test(s) that must pass did not.", file=sys.stderr)
        for s, t, r in regressions:
            print(f"  {s}::{t}  {r}", file=sys.stderr)
        return 1
    if unexpected_pass:
        print(
            f"\nGATE: {len(unexpected_pass)} test(s) now pass that the baseline says fail. "
            "Re-run with --update-expect once you have confirmed why.",
            file=sys.stderr,
        )
        for s, t, c in unexpected_pass:
            print(f"  {s}::{t}  (was {c})", file=sys.stderr)
        return 2
    if unknown:
        print(f"\nGATE: {len(unknown)} test(s) are not in the baseline.", file=sys.stderr)
        return 2
    print("\nGATE OK: every test matched its expectation.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
