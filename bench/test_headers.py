#!/usr/bin/env python3
"""Acceptance test / oracle for HEADER (heading) extraction in distillpdf.to_html().

Ground truth lives in header_groundtruth.json — the ordered section/subsection
headers of each arXiv paper, extracted by font-aware analysis (size + bold +
numbering) with LLM judgement. This test scores the <h1>/<h2>/<h3> headings that
to_html() actually emits against that oracle.

Headers are paragraph-aware: a bold line (or bold run-in lead) at the top of a
paragraph is a header; its LEVEL comes from numbering depth ("2" → 1, "2.1" → 2,
"2.1.1" → 3) and font size. So we score two things:
  header recall   — fraction of ground-truth headers surfaced as a heading
  level accuracy  — of those captured, fraction emitted at the correct level

Run:  python3 bench/test_headers.py     (report + non-zero exit on failure)
  or: pytest bench/test_headers.py
"""
import json, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
# Two ground-truth sets, judged differently:
#  - "tuned": the 4 papers the detector was developed against. Strict per-file
#    recall (a regression guard) + high aggregate bars.
#  - "held-out": 15 fresh arXiv papers across fields/styles (Roman numerals,
#    "N." vs "N " section numbering, IEEE letter subsections, math run-in heads),
#    fetched after the detector was written. Judged on AGGREGATE means only —
#    per-file minimums are unfair on the hardest unseen papers; the means measure
#    real generalization. Bars are set below demonstrated capability, not at it.
# Each entry: (label, gt_path, pdf_subdir, {per_file_recall|None, recall, level, precision}).
GT_SETS = [
    ("tuned", os.path.join(HERE, "header_groundtruth.json"), "corpus",
     {"per_file_recall": 0.80, "recall": 0.95, "level": 0.85, "precision": 0.55}),
    ("held-out", os.path.join(HERE, "header_groundtruth_heldout.json"), "corpus_headers",
     {"per_file_recall": None, "recall": 0.80, "level": 0.75, "precision": 0.50}),
]

HTAG = re.compile(r"<h([1-6])\b[^>]*>(.*?)</h\1>", re.DOTALL)  # headings now carry an id= anchor
NUM = re.compile(r"^\s*([A-Z]|\d+(?:\.\d+)*)\.?\s+")  # leading "1 ", "2.1 ", "A " ...


def norm(s):
    """Lowercase, drop tags/entities, keep alphanumerics + spaces, collapse."""
    s = re.sub(r"<[^>]+>", " ", s)
    s = s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", '"')
    s = re.sub(r"[^0-9a-z]+", " ", s.lower())
    return re.sub(r"\s+", " ", s).strip()


def title_only(s):
    """Header text with any leading section number stripped (for loose match)."""
    return norm(NUM.sub("", s.strip()))


def emitted_headings(html):
    """(logical_level, normalized_full, normalized_title) for each <h1-6>.
    HTML reserves <h1> for the document title, so a section is <h2>; the logical
    level (matching the ground truth: 1=section, 2=subsection, 3=sub) is the tag
    number minus one."""
    out = []
    for lvl, body in HTAG.findall(html):
        full = norm(body)
        if full:
            out.append((max(1, int(lvl) - 1), full, title_only(body)))
    return out


def captures(gt_text, heads):
    """Return the matched emitted heading (or None). A GT header is captured when
    its title (number-stripped) equals or is contained by an emitted heading's
    text — tolerating whether we merged the leading section number or not."""
    gt_full, gt_title = norm(gt_text), title_only(gt_text)
    if not gt_title:
        gt_title = gt_full
    for h in heads:
        lvl, full, title = h
        if gt_full == full or gt_title == title:
            return h
        # contained either way (run-in heads, trailing punctuation, merged number)
        if gt_title and (f" {gt_title} " in f" {full} " or f" {gt_title} " in f" {title} "):
            return h
        if title and f" {title} " in f" {gt_full} ":
            return h
    return None


def score_set(label, gt_path, subdir, thr):
    """Score one ground-truth set; returns (recalls, level_accs, precisions, failures)."""
    import distillpdf
    gt = json.load(open(gt_path))
    files = [k for k in gt if not k.startswith("_")]
    recalls, level_accs, precisions, failures = [], [], [], []
    print(f"\n=== {label} set ({subdir}/) ===")
    print(f"{'PDF':30s}{'GT':>4s}{'found':>7s}{'recall':>8s}{'lvl_acc':>8s}{'prec':>7s}")
    for name in files:
        path = os.path.join(HERE, subdir, name)
        if not os.path.exists(path):
            failures.append(f"{name}: PDF missing in {subdir}/")
            continue
        heads = emitted_headings(distillpdf.Pdf.open(path).to_html())
        rows = gt[name]
        found = lvl_ok = 0
        matched_heads = set()
        for r in rows:
            m = captures(r["text"], heads)
            if m:
                found += 1
                matched_heads.add(id(m))
                if m[0] == r["level"]:
                    lvl_ok += 1
        recall = found / len(rows) if rows else 1.0
        lvl_acc = lvl_ok / found if found else 0.0
        precision = (sum(1 for h in heads if id(h) in matched_heads) / len(heads)) if heads else 1.0
        recalls.append(recall)
        level_accs.append(lvl_acc)
        precisions.append(precision)
        print(f"{name[:30]:30s}{len(rows):>4d}{found:>7d}{recall:>8.2f}{lvl_acc:>8.2f}{precision:>7.2f}")
        pf = thr.get("per_file_recall")
        if pf is not None and recall < pf:
            failures.append(f"[{label}] {name}: header recall {recall:.2f} < {pf}")

    n = len(recalls) or 1
    mr, ml, mp = sum(recalls) / n, sum(level_accs) / n, sum(precisions) / n
    print(f"  {label} means — recall {mr:.3f} (>={thr['recall']})  "
          f"level {ml:.3f} (>={thr['level']})  precision {mp:.3f} (>={thr['precision']})")
    if mr < thr["recall"]:
        failures.append(f"[{label}] mean recall {mr:.3f} < {thr['recall']}")
    if ml < thr["level"]:
        failures.append(f"[{label}] mean level accuracy {ml:.3f} < {thr['level']}")
    if mp < thr["precision"]:
        failures.append(f"[{label}] mean precision {mp:.3f} < {thr['precision']}")
    return recalls, level_accs, precisions, failures


def run():
    failures = []
    all_r, all_l, all_p = [], [], []
    for label, gt_path, subdir, thr in GT_SETS:
        if not os.path.exists(gt_path):
            continue
        r, l, p, f = score_set(label, gt_path, subdir, thr)
        all_r += r; all_l += l; all_p += p; failures += f

    if all_r:
        print(f"\nOVERALL ({len(all_r)} papers): recall {sum(all_r)/len(all_r):.3f}  "
              f"level {sum(all_l)/len(all_l):.3f}  precision {sum(all_p)/len(all_p):.3f}")

    if failures:
        print(f"\n❌ {len(failures)} FAILURES:")
        for f in failures[:40]:
            print("  -", f)
        return False
    print("\n✅ ALL CHECKS PASSED")
    return True


def test_header_extraction():
    assert run(), "header extraction goalline not met (see report above)"


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
