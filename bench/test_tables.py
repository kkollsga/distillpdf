#!/usr/bin/env python3
"""Score pylopdf table detection against synthetic ground truth (gen_tables.py).

Metrics:
  detection recall   — fraction of ground-truth tables found (content overlap >=0.5)
  false-positive rate— tables hallucinated on negative (table-free) PDFs
  column accuracy    — fraction of GT tables whose detected col count is within +-1
  cell-content recall— mean fraction of GT cell tokens recovered in the matched table

Run:  python3 bench/test_tables.py   (report; non-zero exit on threshold miss)
  or: pytest bench/test_tables.py
"""
import json, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
GTDIR = os.path.join(HERE, "corpus_tables")
WORD = re.compile(r"\w+", re.UNICODE)

# Thresholds — set after measuring baseline (see report). Tune in the goal.
MIN_DETECTION_RECALL = 0.80
MAX_FALSE_POSITIVES = 0          # tables on negative PDFs
MIN_COL_ACCURACY = 0.70
MIN_CELL_RECALL = 0.85
MIN_COUNT_ACCURACY = 0.70        # PDFs where detected table count == GT count


def toks(grid):
    out = set()
    for row in grid:
        for cell in row:
            for w in WORD.findall(str(cell).lower()):
                out.add(w)
    return out


def detected_tables(path):
    import pylopdf
    res = []
    for t in pylopdf.Pdf.open(path).extract_tables():
        res.append({"n_cols": t["n_cols"], "n_rows": t["n_rows"], "cells": t["cells"]})
    return res


def run():
    gt = json.load(open(os.path.join(GTDIR, "ground_truth.json")))
    total_gt = 0
    found = 0
    col_ok = 0
    cell_recalls = []
    false_pos = 0
    count_ok = 0
    count_total = 0
    rows = []

    for fname, info in sorted(gt.items()):
        path = os.path.join(GTDIR, fname)
        try:
            det = detected_tables(path)
        except Exception as e:
            rows.append((fname, f"ERROR {type(e).__name__}: {str(e)[:60]}"))
            continue

        if info["is_negative"]:
            false_pos += len(det)
            rows.append((fname, f"NEG: detected {len(det)} (want 0)"))
            continue

        count_total += 1
        if len(det) == len(info["tables"]):
            count_ok += 1
        det_tok = [toks(d["cells"]) for d in det]
        line = []
        for g in info["tables"]:
            total_gt += 1
            gtok = toks(g["cells"])
            # best-matching detected table by token overlap
            best_i, best_r = -1, 0.0
            for i, dt in enumerate(det_tok):
                r = len(gtok & dt) / len(gtok) if gtok else 0.0
                if r > best_r:
                    best_r, best_i = r, i
            cell_recalls.append(best_r)
            if best_r >= 0.5:
                found += 1
                dcols = det[best_i]["n_cols"]
                if abs(dcols - g["cols"]) <= 1:
                    col_ok += 1
                line.append(f"{g['style']} gt{g['rows']}x{g['cols']}->det{det[best_i]['n_rows']}x{dcols} r{best_r:.2f}")
            else:
                line.append(f"{g['style']} gt{g['rows']}x{g['cols']}->MISS")
        rows.append((fname, f"det={len(det)} | " + " ; ".join(line)))

    det_recall = found / total_gt if total_gt else 0.0
    col_acc = col_ok / total_gt if total_gt else 0.0
    mean_cell = sum(cell_recalls) / len(cell_recalls) if cell_recalls else 0.0
    count_acc = count_ok / count_total if count_total else 0.0

    print(f"{'PDF':34s} result")
    for n, r in rows:
        print(f"{n:34s} {r}")
    print("\n--- metrics ---")
    print(f"detection recall : {det_recall:.3f}  (need >={MIN_DETECTION_RECALL}, found {found}/{total_gt})")
    print(f"false positives  : {false_pos}      (need <={MAX_FALSE_POSITIVES})")
    print(f"table-count acc  : {count_acc:.3f}  (need >={MIN_COUNT_ACCURACY}, {count_ok}/{count_total})")
    print(f"column accuracy  : {col_acc:.3f}  (need >={MIN_COL_ACCURACY})")
    print(f"cell recall mean : {mean_cell:.3f}  (need >={MIN_CELL_RECALL})")

    fails = []
    if det_recall < MIN_DETECTION_RECALL: fails.append("detection recall")
    if false_pos > MAX_FALSE_POSITIVES: fails.append("false positives")
    if count_acc < MIN_COUNT_ACCURACY: fails.append("table-count accuracy")
    if col_acc < MIN_COL_ACCURACY: fails.append("column accuracy")
    if mean_cell < MIN_CELL_RECALL: fails.append("cell recall")
    if fails:
        print("\n❌ FAIL:", ", ".join(fails))
        return False
    print("\n✅ PASS")
    return True


def test_tables():
    assert run(), "table-detection thresholds not met (see report)"


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
