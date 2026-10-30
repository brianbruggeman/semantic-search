#!/usr/bin/env python3
"""Exact search versus raw rg output on the frozen corpus.

usage: exact_eval.py <ss binary> <corpus dir> <exact_queries.tsv>
Per pattern: tokens (ceil(chars/4)) of `ss --exact|-F` at the default budget and at --budget 1000, of
`rg -n --no-heading` run from the corpus root, the matching-line count of each (ss header vs `rg -c` summed), and cuts.
"""
import math
import subprocess
import sys


def tokens(text):
    return math.ceil(len(text) / 4)


def pctl(values, fraction):
    ordered = sorted(values)
    return ordered[max(1, math.ceil(fraction * len(ordered))) - 1]


def run(args, cwd):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout


def main():
    ss, corpus, queries = sys.argv[1:4]
    rows = []
    for line in open(queries):
        if line.startswith("# ") or not line.strip():
            continue
        pattern, kind, size = line.rstrip("\n").split("\t")[:3]
        fixed = ["-F"] if kind == "literal" else []
        ours = run([ss, "-F" if fixed else "--exact", pattern], corpus)
        wide = run([ss, "-F" if fixed else "--exact", "--budget", "1000", pattern], corpus)
        raw = run(["rg", "-n", "--no-heading", *fixed, "-e", pattern], corpus)
        counts = run(["rg", "-c", *fixed, "-e", pattern], corpus)
        rg_lines = sum(int(row.rsplit(":", 1)[1]) for row in counts.splitlines())
        head = ours.splitlines()[0] if ours else "0 matches in 0 files"
        ss_lines = int(head.split()[0])
        cut = next((row for row in ours.splitlines() if row.startswith("cut:")), "")
        rows.append((pattern, size, rg_lines, ss_lines, tokens(raw), tokens(ours), tokens(wide), cut))
    assert len(rows) == 20, len(rows)
    print("pattern | class | rg lines | ss lines | rg tokens | ss tokens | ss tokens (--budget 1000) | cut")
    for row in rows:
        print(" | ".join(str(cell) for cell in row))
    for label, index in (("rg -n", 4), ("ss default", 5), ("ss --budget 1000", 6)):
        values = [row[index] for row in rows]
        print(f"{label}: N={len(values)} median={pctl(values, .5)} p95={pctl(values, .95)} sum={sum(values)}")
    print("line-count parity (ss == rg -c): %d of %d" % (sum(r[2] == r[3] for r in rows), len(rows)))
    flood = [r for r in rows if r[2] > 40]
    print("patterns with more than 40 matching lines: %d; their rg tokens median %d, ss median %d"
          % (len(flood), pctl([r[4] for r in flood], .5), pctl([r[5] for r in flood], .5)))


main()
