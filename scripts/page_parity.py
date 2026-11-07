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
  * `moved`: the same words at another position, a paragraph's tail taken past a footnote;
  * `heading`: the same words with different `#` or `**` marks;
  * `other`: anything else, which is a defect until explained.

Whatever the classes say, `text_preserved` is the check that matters most: every document's
letters and digits, counted, must come out the same.
"""
import collections
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


def bare(tokens):
    return "".join(c for c in "".join(tokens) if c.isalnum())


def strip_marks(tokens):
    return [t for t in (re.sub(r"^#{1,6}$|\*\*", "", t) for t in tokens) if t]


def classify(a, b):
    """Word-level hunks between whole-document tokens `a` and page tokens `b`."""
    counts = {"joined": 0, "moved": 0, "heading": 0, "other": 0}
    hunks = []
    matcher = difflib.SequenceMatcher(None, a, b, autojunk=False)
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag != "equal":
            hunks.append([None, i1, a[i1:i2], b[j1:j2]])
    for hunk in hunks:
        if bare(hunk[2]) == bare(hunk[3]):
            hunk[0] = "heading" if strip_marks(hunk[2]) == strip_marks(hunk[3]) else "joined"
    # A deletion and an insertion of the same letters are one move. Whatever is left may be moves
    # whose seam rejoined a hyphenated word, so it is judged as a whole by its letters.
    gone = [h for h in hunks if h[0] is None and not h[3]]
    came = [h for h in hunks if h[0] is None and not h[2]]
    for out in gone:
        for into in came:
            if into[0] is None and out[0] is None and bare(out[2]) == bare(into[3]):
                out[0] = into[0] = "moved"
    rest = [h for h in hunks if h[0] is None]
    if rest and collections.Counter(bare(t for h in rest for t in h[2])) == collections.Counter(bare(t for h in rest for t in h[3])):
        for hunk in rest:
            hunk[0] = "moved"
    for hunk in hunks:
        hunk[0] = hunk[0] or "other"
        counts[hunk[0]] += 1
    return counts, [(kind, position, " ".join(left)[:400], " ".join(right)[:400]) for kind, position, left, right in hunks]


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
        preserved = collections.Counter(bare(a)) == collections.Counter(bare(b))
        if a == b:
            results.append({"doc": name, "status": "identical", "tokens": len(a), "text_preserved": True})
            print(f"{name}: identical ({len(a)} tokens)", flush=True)
            continue
        head, tail = fast_equal_runs(a, b)
        counts, hunks = classify(a[head : len(a) - tail], b[head : len(b) - tail])
        results.append({"doc": name, "status": "differs", "tokens": len(a), "text_preserved": preserved, **counts})
        with open(out_dir / f"{name}.diff.txt", "w") as out:
            for kind, position, left, right in hunks:
                out.write(f"[{kind}] at {head + position}\n  - {left}\n  + {right}\n")
        print(f"{name}: differs {counts} of {len(a)} tokens, text preserved: {preserved}", flush=True)
    with open(out_dir / "parity.json", "w") as out:
        json.dump(results, out, indent=1)
    identical = sum(r["status"] == "identical" for r in results)
    totals = {k: sum(r.get(k, 0) for r in results) for k in ("joined", "moved", "heading", "other")}
    lost = [r["doc"] for r in results if not r.get("text_preserved")]
    print(f"{identical}/{len(results)} identical; hunks elsewhere: {totals}; text not preserved in: {lost}")
    return 1 if lost else 0


if __name__ == "__main__":
    sys.exit(main())
