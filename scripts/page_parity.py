#!/usr/bin/env python3
"""Compare page-at-a-time Markdown with the whole-document Markdown, document by document.

    scripts/page_parity.py PROBE OUT_DIR PDF [PDF ...]

PROBE is `pages_probe` from `cargo build --release -p distillpdf --example pages_probe`. For each
PDF it runs the `wholemd` and `pagemd` modes, removes the differences the page API names in its
module docs, and classifies whatever remains. `OUT_DIR/parity.json` holds every document's result
and `OUT_DIR/<doc>.diff.txt` the word-level hunks of any document that still differs.

What is normalised away before comparing, each one stated in `distillpdf/src/pages.rs`:
  * the `[continues on PDF p. N]` and `[continued from PDF p. N]` marker lines;
  * figure anchors, which are page-scoped there and document-numbered here;
  * blank lines, which a concatenation cannot reproduce exactly, and line structure generally.

What is then classified rather than removed, so it stays visible:
  * `joined`: a word split across a paragraph break that the page path rejoined;
  * `heading`: the same words with different `#` marks, a heading demotion that differs;
  * `other`: anything else, which is a defect until explained.
"""
import difflib
import json
import pathlib
import re
import subprocess
import sys

MARKER = re.compile(r"^\[(continues on|continued from) PDF p\. \d+\]$", re.M)
ANCHOR = re.compile(r"\(#(?:p\d+-)?fig-[\w.-]+\)")
TOKEN = re.compile(r"\S+")


def run(probe, mode, pdf):
    done = subprocess.run([probe, mode, pdf], capture_output=True, timeout=1800)
    return done.returncode, done.stdout.decode("utf-8", "replace")


def tokens(markdown):
    text = MARKER.sub("", markdown)
    text = ANCHOR.sub("(#fig)", text)
    return TOKEN.findall(text)


def classify(a, b):
    """Word-level hunks between whole-document tokens `a` and page tokens `b`."""
    counts = {"joined": 0, "heading": 0, "other": 0}
    hunks = []
    matcher = difflib.SequenceMatcher(None, a, b, autojunk=False)
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "equal":
            continue
        left, right = a[i1:i2], b[j1:j2]
        if "".join(left).replace("-", "").replace("­", "") == "".join(right).replace("-", "").replace("­", ""):
            kind = "joined"
        elif [t for t in left if not re.fullmatch(r"#{1,6}", t)] == [t for t in right if not re.fullmatch(r"#{1,6}", t)]:
            kind = "heading"
        else:
            kind = "other"
        counts[kind] += 1
        hunks.append((kind, i1, " ".join(left)[:400], " ".join(right)[:400]))
    return counts, hunks


def fast_equal_runs(a, b):
    """Trim the common head and tail: difflib is quadratic on the middle only."""
    head = 0
    while head < len(a) and head < len(b) and a[head] == b[head]:
        head += 1
    tail = 0
    while tail < len(a) - head and tail < len(b) - head and a[-1 - tail] == b[-1 - tail]:
        tail += 1
    return head, tail


def main():
    probe, out_dir, pdfs = sys.argv[1], pathlib.Path(sys.argv[2]), sys.argv[3:]
    out_dir.mkdir(parents=True, exist_ok=True)
    results = []
    for pdf in pdfs:
        name = pathlib.Path(pdf).name
        whole_rc, whole = run(probe, "wholemd", pdf)
        page_rc, page = run(probe, "pagemd", pdf)
        if whole_rc or page_rc:
            results.append({"doc": name, "status": "crash", "whole_rc": whole_rc, "page_rc": page_rc})
            print(f"{name}: CRASH whole={whole_rc} page={page_rc}", flush=True)
            continue
        a, b = tokens(whole), tokens(page)
        if a == b:
            results.append({"doc": name, "status": "identical", "tokens": len(a)})
            print(f"{name}: identical ({len(a)} tokens)", flush=True)
            continue
        head, tail = fast_equal_runs(a, b)
        counts, hunks = classify(a[head : len(a) - tail], b[head : len(b) - tail])
        results.append({"doc": name, "status": "differs", "tokens": len(a), **counts})
        with open(out_dir / f"{name}.diff.txt", "w") as out:
            for kind, position, left, right in hunks:
                out.write(f"[{kind}] at {head + position}\n  - {left}\n  + {right}\n")
        print(f"{name}: differs {counts} of {len(a)} tokens", flush=True)
    with open(out_dir / "parity.json", "w") as out:
        json.dump(results, out, indent=1)
    identical = sum(r["status"] == "identical" for r in results)
    totals = {k: sum(r.get(k, 0) for r in results) for k in ("joined", "heading", "other")}
    print(f"{identical}/{len(results)} identical; hunks elsewhere: {totals}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
