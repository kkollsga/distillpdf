#!/usr/bin/env python3
"""Score page-at-a-time Markdown against the whole-document Markdown, per document category.

    scripts/page_score.py PROBE REFERENCE_DIR OUT.json PDF [PDF ...]

Uses the regression gate's own definitions (`scripts/regression_gate/score.py`): word-set recall
against the frozen PyMuPDF reference, and token order pooled per page. Run after the gate has built
its reference; REFERENCE_DIR is `$GATE_HOME/pymupdf-ref`. PROBE is the `pages_probe` example.

Structure needs no scoring here: `scripts/page_parity.py` shows heading, list and table marks are
the whole-document path's on every document. This is for the one thing that does change, which page
a paragraph's tail is counted on.
"""
import collections
import json
import os
import pathlib
import re
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "regression_gate"))
import score as S  # noqa: E402

MARKER = re.compile(r"^\[(continues on|continued from) PDF p\. \d+\]$", re.M)
TARGET = re.compile(r"\]\([^)]*\)")


def words(markdown):
    return S.norm_words(TARGET.sub("]", MARKER.sub("", markdown)))


def windows(pages, size=8):
    return [[w for page in pages[i : i + size] for w in page] for i in range(0, len(pages), size)]


def category(slug):
    head = slug.split("_")[0]
    return "arxiv" if re.search(r"_\d{4}\.\d+$", slug) else head


def main():
    probe, refs, out_path, pdfs = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4:]
    rows = []
    for pdf in pdfs:
        slug = pathlib.Path(pdf).stem
        path = os.path.join(refs, f"{slug}.json.gz")
        if not os.path.exists(path):
            print(f"{slug}: no reference", flush=True)
            continue
        ref = S.load_reference(path)
        ref_pages = [S.norm_words(p) for p in ref["pages"]]
        ref_set = {w for p in ref_pages for w in p}
        done = subprocess.run([probe, "bothpages", pdf], capture_output=True, timeout=1800)
        if done.returncode:
            print(f"{slug}: CRASH {done.returncode}", flush=True)
            rows.append({"doc": slug, "category": category(slug), "crash": done.returncode})
            continue
        both = json.loads(done.stdout)
        row = {"doc": slug, "category": category(slug), "pages": len(ref_pages)}
        for name in ("whole", "pages"):
            got = [words(p) for p in both[name]]
            row[f"{name}_recall"] = S.overlap(ref_set, {w for p in got for w in p})[0]
            row[f"{name}_order"] = S.pooled_order(ref_pages, got)
            # Per-page order counts a tail moved to the page where its paragraph starts as out of
            # place on both pages. Windows of pages show whether the reading order itself moved.
            row[f"{name}_order_w"] = S.pooled_order(windows(ref_pages), windows(got))
        rows.append(row)
        print(json.dumps(row), flush=True)
    with open(out_path, "w") as out:
        json.dump(rows, out, indent=1)
    groups = collections.defaultdict(list)
    for row in rows:
        groups[row["category"]].append(row)
    mean = lambda items, key: sum(r[key] for r in items if r.get(key) is not None) / max(1, sum(r.get(key) is not None for r in items))
    print("category docs recall whole/page, order per page whole/page, order in 8-page windows whole/page, docs lower on recall, on windowed order")
    for name, items in sorted(groups.items()):
        lower_recall = sum((r.get("pages_recall") or 0) < (r.get("whole_recall") or 0) for r in items)
        lower_order = sum((r.get("pages_order_w") or 0) < (r.get("whole_order_w") or 0) for r in items)
        print(f"{name} {len(items)} {mean(items, 'whole_recall'):.4f}/{mean(items, 'pages_recall'):.4f} {mean(items, 'whole_order'):.4f}/{mean(items, 'pages_order'):.4f} {mean(items, 'whole_order_w'):.4f}/{mean(items, 'pages_order_w'):.4f} {lower_recall} {lower_order}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
