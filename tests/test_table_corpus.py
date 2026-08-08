#!/usr/bin/env python3
"""Score the table torture corpus — the metrics that close bench100's and `test_tables.py`'s
shared blind spot.

Both existing table metrics are **blind to cell placement**:

  * bench100's table accuracy is `0.5*count-agreement + 0.5*rows x cols-agreement`, which
    scores **1.0 when the grid is right and every value sits under the wrong header**;
  * `tests/test_tables.py`'s cell recall is a token **set** overlap, so the same content
    scores 1.0 wherever the tokens land.

This suite is the fix, and it is only possible because the corpus is generated: every cell's
text and (r, c) is KNOWN, not judged. The load-bearing metric is #2 below.

Metrics (spec §8)
-----------------
1. **grid** — `n_rows`/`n_cols` exact, after trimming fully-empty edge rows/cols of the
   detection.
2. **positional cell accuracy** — a detected (r, c) is correct **iff its normalised text
   equals the ground truth at (r, c)**. Strict: no re-alignment, because on generated PDFs we
   control the content and an alignment heuristic would only launder misplacement. Span text
   is credited at the ANCHOR; a detector that repeats span text into covered slots is
   accepted (`covered in {"", anchor_text}`).
3. **header attribution** — for every non-blank body cell, the concatenated header path above
   its detected column must equal the ground-truth header path. This is the direct probe of
   the proven failure the count/dims metrics cannot see.
4. **token recall** — DIAGNOSTIC ONLY, never gated. Kept because the pair localises a
   failure: high token recall + low positional accuracy = misassignment; both low = lost text.
6. **normalisation** — NFC, collapse internal whitespace, strip soft hyphens. Numbers compare
   **literally**: `1,234.56` != `1234.56`.

The existing frozen floors continue to score legacy `extract_tables()` exactly as before.
The additive report-only analysis metrics use `analyze_tables()` and bbox-first matching;
they do not silently replace or lower that older gate.
"""
from __future__ import annotations

import hashlib
import json
import html as html_module
import math
import os
import re
import subprocess
import sys
import unicodedata

import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "table_corpus")
WORD = re.compile(r"\w+", re.UNICODE)

#: The closed `expect` vocabulary (§7). A key outside this set is a truth-file bug, not a
#: silently-ignored expectation.
EXPECT_KEYS = {"table_count", "table_count_any", "cols_any", "must_not_merge", "exactly_once"}

# The legacy extraction payload has cell text and dimensions, but not table/cell geometry or
# span topology. Keep the capability declaration executable so reports cannot silently claim
# metrics that this API cannot support. Phase 3 can flip these only with the corresponding
# additive fields and attack tests.
SCORER_CAPABILITIES = {
    "bbox_iou": True,
    "span_topology": True,
    "cell_localization": True,
    "semantic_header_depth": True,
}


def test_parity_adjudicates_each_file_before_family_summary():
    """A perfect/easy sibling cannot be cancelled by a deliberately hard sibling."""
    from gen_table_corpus import _family_parity_summary, adjudicate_parity

    cases = [
        {"file": "easy.pdf", "family": "siblings", "source": "real p1", "tier": 2,
         "C_gen": 0.4, "C_real": 0.2, "invented": False, "accepted_reason": None},
        {"file": "hard.pdf", "family": "siblings", "source": "real p1", "tier": 2,
         "C_gen": 0.0, "C_real": 0.2, "invented": False, "accepted_reason": None},
    ]

    rows, over, accepted = adjudicate_parity(cases)
    summary = _family_parity_summary(rows)[0]

    assert {r["file"]: r["verdict"] for r in rows} == {
        "easy.pdf": "OVERSIMPLIFIED", "hard.pdf": "ok"}
    assert [r["file"] for r in over] == ["easy.pdf"]
    assert accepted == []
    assert summary["mean_C_gen"] == 0.2       # old implementation passed this mean
    assert summary["max_gap"] == 0.2
    assert summary["verdict"] == "OVERSIMPLIFIED — unexplained"


def test_parity_preserves_exact_accepted_exclusion_without_exempting_siblings():
    """Existing evidence accepts only the named case; the family remains a truthful mix."""
    from gen_table_corpus import OVERSIMPLIFIED, _family_parity_summary, adjudicate_parity

    accepted_file = "t1_borderless_small.pdf"
    cases = [
        {"file": accepted_file, "family": "borderless", "source": "real p35", "tier": 1,
         "C_gen": 1.0, "C_real": 0.7786, "invented": False,
         "accepted_reason": OVERSIMPLIFIED[accepted_file]},
        {"file": "unflagged_sibling.pdf", "family": "borderless", "source": "real p35",
         "tier": 1, "C_gen": 0.7, "C_real": 0.7786, "invented": False,
         "accepted_reason": None},
    ]

    rows, over, accepted = adjudicate_parity(cases)
    summary = _family_parity_summary(rows)[0]

    assert over == []
    assert [r["file"] for r in accepted] == [accepted_file]
    assert {r["file"]: r["verdict"] for r in rows}["unflagged_sibling.pdf"] == "ok"
    assert summary["verdict"] == "partial — accepted exclusion(s)"


# ------------------------------------------------------------------------- normalisation

def norm(s) -> str:
    s = unicodedata.normalize("NFC", str(s)).replace("­", "")
    return " ".join(s.split())


def toks(s) -> set:
    return set(WORD.findall(norm(s).lower()))


# ------------------------------------------------------------------------------ detection

def detect(path):
    """Detected tables as `{page, rows: [[str]]}`, edge-trimmed. `page` is 0-based here;
    the API reports it 1-based."""
    import distillpdf

    out = []
    for t in distillpdf.Pdf.open(path).extract_tables():
        rows = [[norm(c) for c in row] for row in t["cells"]]
        out.append({"page": int(t["page"]) - 1, "rows": trim(rows),
                    "raw_rows": rows, "n_rows": t["n_rows"], "n_cols": t["n_cols"]})
    return out


def detect_analysis(path):
    """The additive raw-analysis payload, normalized but otherwise unmodified."""
    import distillpdf

    out = []
    for table in distillpdf.Pdf.open(path).analyze_tables():
        cells = []
        for cell in table["cells"]:
            cells.append({
                "r": int(cell["row"]), "c": int(cell["col"]),
                "text": norm(cell["text"]),
                "rowspan": int(cell["rowspan"]), "colspan": int(cell["colspan"]),
                "bbox_norm": cell["bbox_norm"], "role": cell["role"],
                "header_path": [list(anchor) for anchor in cell["header_path"]],
            })
        out.append({
            "page": int(table["page"]) - 1,
            "bbox_norm": table["bbox_norm"],
            "n_rows": int(table["n_rows"]), "n_cols": int(table["n_cols"]),
            "header_rows": int(table["header_rows"]),
            "cells": cells, "evidence": list(table["evidence"]),
        })
    return out


def detect_html_semantics(path):
    """Detected HTML tables with their real leading all-``th`` depth.

    ``extract_tables()`` deliberately exposes cells only and cannot observe either semantic
    tags or declared L0 tables. This companion reads the shipped HTML surface, so a header
    gate cannot accidentally pass while checking the geometric API again.
    """
    import distillpdf

    body = distillpdf.Pdf.open(path).to_html(return_string=True)
    out = []
    for table in re.findall(r"<table\b.*?</table>", body, re.DOTALL):
        rows = []
        header_rows = 0
        still_leading = True
        for tr in re.findall(r"<tr\b.*?</tr>", table, re.DOTALL):
            cells = re.findall(r"<(t[dh])\b([^>]*)>(.*?)</t[dh]>", tr, re.DOTALL)
            if not cells:
                continue
            row = []
            for _, attrs, inner in cells:
                text = html_module.unescape(re.sub(r"<[^>]+>", "", inner))
                span = re.search(r'\bcolspan="(\d+)"', attrs)
                row.extend([norm(text)] * (int(span.group(1)) if span else 1))
            all_th = all(tag == "th" for tag, _, _ in cells)
            if still_leading and all_th:
                header_rows += 1
            else:
                still_leading = False
            rows.append(row)
        out.append({"page": 0, "rows": trim(rows), "header_rows": header_rows})
    return out


def trim(rows):
    """Drop fully-empty edge rows and columns (§8.1) — leading/trailing only."""
    if not rows:
        return rows
    w = max(len(r) for r in rows)
    rows = [list(r) + [""] * (w - len(r)) for r in rows]
    while rows and not any(rows[0]):
        rows.pop(0)
    while rows and not any(rows[-1]):
        rows.pop()
    while rows and rows[0] and not any(r[0] for r in rows):
        rows = [r[1:] for r in rows]
    while rows and rows[0] and not any(r[-1] for r in rows):
        rows = [r[:-1] for r in rows]
    return rows


# --------------------------------------------------------------------------- ground truth

def gt_matrix(t):
    """(matrix, covered, header_rows) — the ground-truth table as a dense grid.

    `covered` is the set of slots a span covers; a detector may leave them empty or repeat the
    anchor text, and both are accepted (§8.2)."""
    n_r, n_c = t["n_rows"], t["n_cols"]
    m = [["" for _ in range(n_c)] for _ in range(n_r)]
    covered = {}
    for cell in t["cells"]:
        r, c = cell["r"], cell["c"]
        txt = norm(cell.get("text", ""))
        if r < n_r and c < n_c:
            m[r][c] = txt
        for dr in range(cell.get("rowspan", 1)):
            for dc in range(cell.get("colspan", 1)):
                if (dr, dc) != (0, 0) and r + dr < n_r and c + dc < n_c:
                    covered[(r + dr, c + dc)] = txt
    return m, covered, t.get("header_rows", 1)


def gt_tokens(t):
    return set().union(*[toks(c.get("text", "")) for c in t["cells"]]) if t["cells"] else set()


# -------------------------------------------------------------------------------- matching

def align(det, gts):
    """Order-preserving maximum-weight matching of detections to GT tables on the same page.

    Both sides are in reading order, so a matching that crossed would claim a table was
    emitted before one it physically follows."""
    n, m = len(det), len(gts)
    w = [[overlap(det[i], gts[j]) for j in range(m)] for i in range(n)]
    f = [[0.0] * (m + 1) for _ in range(n + 1)]
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            f[i][j] = max(f[i - 1][j - 1] + w[i - 1][j - 1], f[i - 1][j], f[i][j - 1])
    pairs, i, j = [], n, m
    while i > 0 and j > 0:
        d = w[i - 1][j - 1]
        if d > 0 and abs(f[i][j] - (f[i - 1][j - 1] + d)) < 1e-12:
            pairs.append((i - 1, j - 1))
            i, j = i - 1, j - 1
        elif abs(f[i][j] - f[i - 1][j]) < 1e-12:
            i -= 1
        else:
            j -= 1
    pairs.reverse()
    return pairs


def overlap(d, g):
    gt = gt_tokens(g)
    if not gt:
        return 0.0
    dt = set().union(*[toks(c) for row in d["rows"] for c in row]) if d["rows"] else set()
    return len(gt & dt) / len(gt)


# --------------------------------------------------------------------------------- scoring

def score_pair(d, g):
    m, covered, hdr = gt_matrix(g)
    rows = d["rows"]
    n_r, n_c = g["n_rows"], g["n_cols"]
    det_r, det_c = len(rows), (max(len(r) for r in rows) if rows else 0)
    grid_exact = (det_r == n_r and det_c == n_c)

    # --- 2. positional cell accuracy (the anti-"1.0 under the wrong header" metric) ------
    total = ok = 0
    wrong = []
    for r in range(n_r):
        for c in range(n_c):
            got = rows[r][c] if r < det_r and c < len(rows[r]) else ""
            if (r, c) in covered:
                # a detector may leave a covered slot empty OR repeat the anchor text
                total += 1
                if got in ("", covered[(r, c)]):
                    ok += 1
                else:
                    wrong.append((r, c, m[r][c], got))
                continue
            total += 1
            if got == m[r][c]:
                ok += 1
            else:
                wrong.append((r, c, m[r][c], got))
    cell_acc = ok / total if total else 1.0

    # --- 3. header attribution -----------------------------------------------------------
    h_tot = h_ok = 0
    if hdr:
        for r in range(hdr, n_r):
            for c in range(n_c):
                if not m[r][c] or (r, c) in covered:
                    continue
                want = "|".join(m[k][c] for k in range(hdr))
                got = "|".join(rows[k][c] if k < det_r and c < len(rows[k]) else ""
                               for k in range(hdr))
                h_tot += 1
                h_ok += (want == got)
    header_acc = h_ok / h_tot if h_tot else None

    # --- 4. token recall (diagnostic only) ------------------------------------------------
    gt_t = gt_tokens(g)
    det_t = set().union(*[toks(c) for row in rows for c in row]) if rows else set()
    tok_recall = len(gt_t & det_t) / len(gt_t) if gt_t else 1.0

    return {"grid_exact": grid_exact, "cell_acc": cell_acc, "header_acc": header_acc,
            "tok_recall": tok_recall, "n_cells": total,
            "det_dims": [det_r, det_c], "gt_dims": [n_r, n_c], "wrong": wrong[:6]}


def score_is_perfect(res, truth_table_count):
    """One fail-closed predicate for scorer attack tests and local measurement reports."""
    if res.get("detected") != truth_table_count:
        return False
    if res.get("false_positives", 0) or res.get("spurious", 0):
        return False
    if len(res.get("tables", [])) != truth_table_count:
        return False
    return all(t["grid_exact"] and t["cell_acc"] == 1.0
               and t.get("header_acc") in (None, 1.0) for t in res["tables"])


def score_semantic_header_depth(detected, truth):
    """Exact semantic header-depth accuracy for already matched synthetic tables."""
    n = max(len(detected), len(truth))
    if not n:
        return 1.0
    ok = sum(
        detected[i].get("header_rows") == truth[i].get("header_rows", 1)
        for i in range(min(len(detected), len(truth)))
    )
    return ok / n


def bbox_iou_if_available(detected, truth):
    """Return IoU only when both payloads carry geometry; legacy detections return None."""
    a, b = detected.get("bbox_norm"), truth.get("bbox_norm")
    if a is None or b is None:
        return None
    valid = lambda box: (
        isinstance(box, (list, tuple)) and len(box) == 4
        and all(isinstance(v, (int, float)) and math.isfinite(v) for v in box)
        and box[2] > box[0] and box[3] > box[1]
    )
    if not valid(a) or not valid(b):
        return 0.0
    ax0, ay0, ax1, ay1 = a
    bx0, by0, bx1, by1 = b
    inter = max(0.0, min(ax1, bx1) - max(ax0, bx0)) * max(
        0.0, min(ay1, by1) - max(ay0, by0)
    )
    union = ((ax1 - ax0) * (ay1 - ay0) + (bx1 - bx0) * (by1 - by0) - inter)
    return inter / union if union > 0 else 0.0


def span_topology_if_available(detected, truth):
    """Exact span signature only when a future analysis payload supplies explicit spans."""
    if "spans" not in detected or "spans" not in truth:
        return None
    key = lambda s: (s["r"], s["c"], s.get("rowspan", 1), s.get("colspan", 1))
    return sorted(map(key, detected["spans"])) == sorted(map(key, truth["spans"]))


def bbox_first_pairs(detected, truth, threshold=0.5):
    """Maximum-cardinality bbox assignment with deterministic high-IoU tie ordering."""
    adjacency = []
    for di, det in enumerate(detected):
        edges = []
        for gi, gt in enumerate(truth):
            iou = bbox_iou_if_available(det, gt)
            if iou is not None and iou >= threshold:
                edges.append((gi, iou))
        adjacency.append(sorted(edges, key=lambda edge: (-edge[1], edge[0])))
    return _maximum_cardinality(adjacency)


def _maximum_cardinality(adjacency):
    """Dependency-free augmenting-path matcher; edge scores order ties, not total weight."""
    match_truth = {}

    def augment(di, seen):
        for gi, score in adjacency[di]:
            if gi in seen:
                continue
            seen.add(gi)
            if gi not in match_truth or augment(match_truth[gi][0], seen):
                match_truth[gi] = (di, score)
                return True
        return False

    order = sorted(range(len(adjacency)),
                   key=lambda di: (-(adjacency[di][0][1] if adjacency[di] else -1), di))
    for di in order:
        augment(di, set())
    return sorted((di, gi, score) for gi, (di, score) in match_truth.items())


def _analysis_rows(table):
    rows = [["" for _ in range(table["n_cols"])] for _ in range(table["n_rows"])]
    for cell in table["cells"]:
        if cell["r"] < len(rows) and cell["c"] < len(rows[cell["r"]]):
            rows[cell["r"]][cell["c"]] = norm(cell.get("text", ""))
    return {"rows": rows}


def _logical_rect(cell):
    return [cell["c"], cell["r"],
            cell["c"] + cell.get("colspan", 1),
            cell["r"] + cell.get("rowspan", 1)]


def _truth_header_path(table, cell):
    if "header_path" in cell:
        return cell["header_path"]
    if cell.get("header") or cell.get("role") == "header":
        return []
    out = []
    for header in table["cells"]:
        if not (header.get("header") or header.get("role") == "header"):
            continue
        if header["c"] <= cell["c"] < header["c"] + header.get("colspan", 1):
            out.append([header["r"], header["c"]])
    return sorted(out)


def _truth_cell_bbox(table, cell):
    if cell.get("bbox_norm") is not None:
        return cell["bbox_norm"]
    xs, ys = table.get("col_edges_norm"), table.get("row_edges_norm")
    r, c = cell["r"], cell["c"]
    rs, cs = cell.get("rowspan", 1), cell.get("colspan", 1)
    if not xs or not ys or c + cs >= len(xs) or r + rs >= len(ys):
        return None
    return [xs[c], ys[r], xs[c + cs], ys[r + rs]]


def score_analysis_pair(detected, truth):
    """Report-only geometry/topology/semantic metrics on one bbox-matched table.

    The topology/localization scores are anchor-key precision/recall/F1 proxies. They are
    deliberately not named GriTS: this owned scorer does no inserted-row/column alignment.
    """
    pred = {(c["r"], c["c"]): c for c in detected["cells"]}
    gt = {(c["r"], c["c"]): c for c in truth["cells"]}

    top_reward = 0.0
    span_ok = 0
    for key, cell in gt.items():
        got = pred.get(key)
        if got is None:
            continue
        top_reward += bbox_iou_if_available(
            {"bbox_norm": _logical_rect(got)},
            {"bbox_norm": _logical_rect(cell)},
        ) or 0.0
        span_ok += (
            got.get("rowspan", 1), got.get("colspan", 1)
        ) == (
            cell.get("rowspan", 1), cell.get("colspan", 1)
        )

    loc_truth_n = loc_reward = loc_covered = 0
    for key, cell in gt.items():
        truth_bbox = _truth_cell_bbox(truth, cell)
        if truth_bbox is None:
            continue
        loc_truth_n += 1
        got = pred.get(key)
        if got and got.get("bbox_norm") is not None:
            loc_covered += 1
            loc_reward += bbox_iou_if_available(got, {"bbox_norm": truth_bbox}) or 0.0
    loc_pred_n = sum(cell.get("bbox_norm") is not None for cell in pred.values())

    def prf(reward, predicted, expected):
        precision = reward / predicted if predicted else (1.0 if not expected else 0.0)
        recall = reward / expected if expected else (1.0 if not predicted else 0.0)
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        return precision, recall, f1

    top_precision, top_recall, top_f1 = prf(top_reward, len(pred), len(gt))
    span_precision, span_recall, span_f1 = prf(span_ok, len(pred), len(gt))
    loc_precision, loc_recall, loc_f1 = prf(loc_reward, loc_pred_n, loc_truth_n)

    header_total = header_ok = 0
    for key, cell in gt.items():
        if cell.get("header") or cell.get("role") == "header":
            continue
        got = pred.get(key)
        header_total += 1
        header_ok += bool(got and got.get("header_path", []) == _truth_header_path(truth, cell))

    numeric = {trait: [0, 0] for trait in ("numeric", "signed", "decimal", "percent", "unit")}
    for key, cell in gt.items():
        got = pred.get(key)
        for trait in cell.get("value_traits", []):
            numeric[trait][1] += 1
            numeric[trait][0] += bool(got and norm(got["text"]) == norm(cell["text"]))

    gt_blank = {key for key, cell in gt.items() if cell.get("blank")}
    pred_blank = {key for key, cell in pred.items() if not norm(cell.get("text", ""))}
    blank_tp = len(gt_blank & pred_blank)
    blank_precision, blank_recall, blank_f1 = prf(blank_tp, len(pred_blank), len(gt_blank))

    return {
        "bbox_iou": bbox_iou_if_available(detected, truth),
        "anchor_topology_f1": top_f1, "anchor_topology_precision": top_precision,
        "anchor_topology_recall": top_recall,
        "top_reward": top_reward, "top_pred_n": len(pred), "top_truth_n": len(gt),
        "span_exact": span_f1, "span_precision": span_precision,
        "span_recall": span_recall, "span_reward": span_ok,
        "anchor_localization_f1": loc_f1 if loc_truth_n or loc_pred_n else None,
        "anchor_localization_precision": loc_precision if loc_truth_n or loc_pred_n else None,
        "anchor_localization_recall": loc_recall if loc_truth_n or loc_pred_n else None,
        "loc_reward": loc_reward, "loc_pred_n": loc_pred_n, "loc_truth_n": loc_truth_n,
        "loc_coverage": loc_covered / loc_truth_n if loc_truth_n else None,
        "header_depth_exact": detected["header_rows"] == truth.get("header_rows", 1),
        "header_path_acc": header_ok / header_total if header_total else None,
        "header_path_ok": header_ok, "header_path_n": header_total,
        "numeric": {trait: (ok / total if total else None)
                    for trait, (ok, total) in numeric.items()},
        "numeric_counts": numeric,
        "blank_precision": blank_precision, "blank_recall": blank_recall,
        "blank_f1": blank_f1, "blank_tp": blank_tp,
        "blank_pred_n": len(pred_blank), "blank_truth_n": len(gt_blank),
    }


def segmentation_counts(detected, truth, pairs):
    """Counts duplicate/split/fusion/phantom shapes from authored bbox relations."""
    matched_d = {pair[0] for pair in pairs}
    matched_g = {pair[1] for pair in pairs}
    related_d = {di: [] for di in range(len(detected))}
    related_g = {gi: [] for gi in range(len(truth))}
    for di, det in enumerate(detected):
        for gi, gt in enumerate(truth):
            iou = bbox_iou_if_available(det, gt) or 0.0
            if iou >= 0.1:
                related_d[di].append(gi)
                related_g[gi].append(di)
    duplicates = sum(max(0, sum((bbox_iou_if_available(det, gt) or 0) >= 0.5
                                for det in detected) - 1) for gt in truth)
    fusions = sum(len(gs) >= 2 for gs in related_d.values())
    splits = sum(len(ds) >= 2 for ds in related_g.values())
    phantom = sum(di not in matched_d and not related_d[di] for di in range(len(detected)))
    return {"duplicates": duplicates, "fusions": fusions, "splits": splits,
            "phantoms": phantom, "false_positives": len(detected) - len(matched_d),
            "misses": len(truth) - len(matched_g)}


def score_analysis_file(path, rec, detector=detect_analysis):
    detected = detector(path)
    truth = rec["tables"]
    all_pairs, tables = [], []
    for page in sorted({t["page"] for t in detected + truth}):
        d_page = [(i, table) for i, table in enumerate(detected) if table["page"] == page]
        g_page = [(i, table) for i, table in enumerate(truth) if table["page"] == page]
        d_idx, ds = zip(*d_page) if d_page else ((), ())
        g_idx, gs = zip(*g_page) if g_page else ((), ())
        bbox_pairs = bbox_first_pairs(ds, gs)
        used_d = {di for di, _, _ in bbox_pairs}
        used_g = {gi for _, gi, _ in bbox_pairs}
        page_pairs = [(di, gi, iou, "bbox") for di, gi, iou in bbox_pairs]
        rem_d = [(di, table) for di, table in enumerate(ds) if di not in used_d]
        rem_g = [(gi, table) for gi, table in enumerate(gs) if gi not in used_g]
        if rem_d and rem_g:
            token_pairs = align([_analysis_rows(table) for _, table in rem_d],
                                [table for _, table in rem_g])
            page_pairs.extend((rem_d[di][0], rem_g[gi][0], None, "token")
                              for di, gi in token_pairs)
        for di, gi, iou, source in page_pairs:
            all_pairs.append((d_idx[di], g_idx[gi], iou, source))
            scored = score_analysis_pair(ds[di], gs[gi])
            scored["match_source"] = source
            if source == "token":
                scored.update({"bbox_iou": None, "anchor_localization_f1": None,
                               "anchor_localization_precision": None,
                               "anchor_localization_recall": None, "loc_coverage": None,
                               "loc_reward": 0.0, "loc_pred_n": 0, "loc_truth_n": 0})
            tables.append(scored)
    match_sources = {source: sum(pair[3] == source for pair in all_pairs)
                     for source in ("bbox", "token")}
    pred_cells = sum(len(table["cells"]) for table in detected)
    truth_cells = sum(len(table["cells"]) for table in truth)
    pred_loc_cells = sum(cell.get("bbox_norm") is not None
                         for table in detected for cell in table["cells"])
    truth_loc_cells = sum(_truth_cell_bbox(table, cell) is not None
                          for table in truth for cell in table["cells"])
    header_reward = sum(table["header_path_ok"] for table in tables)
    header_depth_reward = sum(table["header_depth_exact"] for table in tables)
    truth_header_cells = sum(
        not (cell.get("header") or cell.get("role") == "header")
        for table in truth for cell in table["cells"])
    numeric_counts = {}
    for trait in ("numeric", "signed", "decimal", "percent", "unit"):
        reward = sum(table["numeric_counts"][trait][0] for table in tables)
        total = sum(trait in cell.get("value_traits", ())
                    for table in truth for cell in table["cells"])
        numeric_counts[trait] = [reward, total]
    blank_reward = sum(table["blank_tp"] for table in tables)
    pred_blank_cells = sum(not norm(cell.get("text", ""))
                           for table in detected for cell in table["cells"])
    truth_blank_cells = sum(cell.get("blank", False)
                            for table in truth for cell in table["cells"])

    def summary(reward, predicted, expected):
        precision = reward / predicted if predicted else (1.0 if not expected else 0.0)
        recall = reward / expected if expected else (1.0 if not predicted else 0.0)
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        return {"precision": precision, "recall": recall, "f1": f1,
                "reward": reward, "pred_n": predicted, "truth_n": expected}

    return {"detected": len(detected), "truth": len(truth), "matches": len(all_pairs),
            "match_sources": match_sources,
            "table_detection": summary(len(all_pairs), len(detected), len(truth)),
            "anchor_topology": summary(sum(table["top_reward"] for table in tables),
                                       pred_cells, truth_cells),
            "anchor_localization": summary(sum(table["loc_reward"] for table in tables),
                                           pred_loc_cells, truth_loc_cells),
            "span": summary(sum(table["span_reward"] for table in tables),
                            pred_cells, truth_cells),
            "header_depth_exact": header_depth_reward / len(truth) if truth else None,
            "header_depth_reward": header_depth_reward,
            "header_path_acc": header_reward / truth_header_cells
            if truth_header_cells else None,
            "header_path_reward": header_reward, "truth_header_cells": truth_header_cells,
            "numeric_counts": numeric_counts,
            "blank": summary(blank_reward, pred_blank_cells, truth_blank_cells),
            "pred_cells": pred_cells, "truth_cells": truth_cells,
            "pred_loc_cells": pred_loc_cells, "truth_loc_cells": truth_loc_cells,
            "tables": tables,
            **segmentation_counts(detected, truth, all_pairs)}


def score_analysis_corpus(outdir=CORPUS):
    truth = json.load(open(os.path.join(outdir, "truth.json")))
    files = {}
    tables = []
    seg = {k: 0 for k in ("duplicates", "fusions", "splits", "phantoms",
                          "false_positives", "misses")}
    totals = {key: 0 for key in ("detected", "truth", "matches", "pred_cells",
                                 "truth_cells", "pred_loc_cells", "truth_loc_cells")}
    match_sources = {"bbox": 0, "token": 0}
    for fname, rec in sorted(truth["files"].items()):
        result = score_analysis_file(os.path.join(outdir, fname), rec)
        files[fname] = result
        tables.extend(result["tables"])
        for key in totals:
            totals[key] += result[key]
        for source in match_sources:
            match_sources[source] += result["match_sources"][source]
        for key in seg:
            seg[key] += result[key]
    def mean(key):
        vals = [table[key] for table in tables if table[key] is not None]
        return sum(vals) / len(vals) if vals else None
    def micro(prefix):
        reward = sum(table[f"{prefix}_reward"] for table in tables)
        pred_n = sum(table[f"{prefix}_pred_n"] for table in tables)
        truth_n = sum(table[f"{prefix}_truth_n"] for table in tables)
        precision = reward / pred_n if pred_n else (1.0 if not truth_n else 0.0)
        recall = reward / truth_n if truth_n else (1.0 if not pred_n else 0.0)
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        return {"precision": precision, "recall": recall, "f1": f1,
                "reward": reward, "pred_n": pred_n, "truth_n": truth_n}

    def prf(reward, predicted, expected):
        precision = reward / predicted if predicted else (1.0 if not expected else 0.0)
        recall = reward / expected if expected else (1.0 if not predicted else 0.0)
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        return {"precision": precision, "recall": recall, "f1": f1,
                "reward": reward, "pred_n": predicted, "truth_n": expected}

    top_reward = sum(table["top_reward"] for table in tables)
    loc_reward = sum(table["loc_reward"] for table in tables)
    aggregate = {"bbox_iou": mean("bbox_iou"),
                 "table_detection": prf(totals["matches"], totals["detected"], totals["truth"]),
                 "match_sources": match_sources,
                 "anchor_topology": prf(top_reward, totals["pred_cells"], totals["truth_cells"]),
                 "matched_anchor_topology": micro("top"),
                 "anchor_localization": prf(loc_reward, totals["pred_loc_cells"],
                                             totals["truth_loc_cells"]),
                 "matched_anchor_localization": micro("loc"),
                 "loc_coverage": mean("loc_coverage"),
                 "matched_header_depth_exact": mean("header_depth_exact")}
    span_reward = sum(table["span_reward"] for table in tables)
    span_pred_n = sum(table["top_pred_n"] for table in tables)
    span_truth_n = sum(table["top_truth_n"] for table in tables)
    span_p = span_reward / span_pred_n if span_pred_n else (1.0 if not span_truth_n else 0.0)
    span_r = span_reward / span_truth_n if span_truth_n else (1.0 if not span_pred_n else 0.0)
    aggregate["matched_span"] = {"precision": span_p, "recall": span_r,
                                 "f1": 2 * span_p * span_r / (span_p + span_r)
                                 if span_p + span_r else 0.0,
                                 "reward": span_reward, "pred_n": span_pred_n,
                                 "truth_n": span_truth_n}
    aggregate["span"] = prf(
        sum(result["span"]["reward"] for result in files.values()),
        totals["pred_cells"], totals["truth_cells"])
    header_depth_ok = sum(result["header_depth_reward"] for result in files.values())
    aggregate["header_depth_exact"] = (
        header_depth_ok / totals["truth"] if totals["truth"] else None)
    hdr_ok = sum(table["header_path_ok"] for table in tables)
    hdr_n = sum(table["header_path_n"] for table in tables)
    aggregate["matched_header_path_acc"] = hdr_ok / hdr_n if hdr_n else None
    e2e_hdr_ok = sum(result["header_path_reward"] for result in files.values())
    e2e_hdr_n = sum(result["truth_header_cells"] for result in files.values())
    aggregate["header_path_acc"] = e2e_hdr_ok / e2e_hdr_n if e2e_hdr_n else None
    blank_tp = sum(table["blank_tp"] for table in tables)
    blank_pred = sum(table["blank_pred_n"] for table in tables)
    blank_truth = sum(table["blank_truth_n"] for table in tables)
    blank_p = blank_tp / blank_pred if blank_pred else (1.0 if not blank_truth else 0.0)
    blank_r = blank_tp / blank_truth if blank_truth else (1.0 if not blank_pred else 0.0)
    aggregate["matched_blank"] = {"precision": blank_p, "recall": blank_r,
                                  "f1": 2 * blank_p * blank_r / (blank_p + blank_r)
                                  if blank_p + blank_r else 0.0,
                                  "tp": blank_tp, "pred_n": blank_pred,
                                  "truth_n": blank_truth}
    e2e_blank_tp = sum(result["blank"]["reward"] for result in files.values())
    e2e_blank_pred = sum(result["blank"]["pred_n"] for result in files.values())
    e2e_blank_truth = sum(result["blank"]["truth_n"] for result in files.values())
    aggregate["blank"] = prf(e2e_blank_tp, e2e_blank_pred, e2e_blank_truth)
    aggregate.update(seg)
    for trait in ("numeric", "signed", "decimal", "percent", "unit"):
        matched_ok = sum(table["numeric_counts"][trait][0] for table in tables)
        matched_total = sum(table["numeric_counts"][trait][1] for table in tables)
        aggregate[f"matched_{trait}_exact"] = matched_ok / matched_total if matched_total else None
        ok = sum(result["numeric_counts"][trait][0] for result in files.values())
        total = sum(result["numeric_counts"][trait][1] for result in files.values())
        aggregate[f"{trait}_exact"] = ok / total if total else None
        aggregate[f"{trait}_n"] = total
    return {"files": files, "aggregate": aggregate}


def score_file(path, rec):
    det = detect(path)
    tables = rec["tables"]
    if not tables:
        return {"false_positives": len(det), "detected": len(det), "tables": []}
    out, used = [], set()
    for pg in sorted({t["page"] for t in tables}):
        gts = [t for t in tables if t["page"] == pg]
        page_det = [(i, d) for i, d in enumerate(det) if d["page"] == pg]
        idx = [i for i, _ in page_det]
        ds = [d for _, d in page_det]
        pairs = align(ds, gts)
        for i, j in pairs:
            used.add(idx[i])
            out.append({"gt": gts[j], **score_pair(ds[i], gts[j])})
        for j in range(len(gts)):
            if not any(jj == j for _, jj in pairs):
                out.append({"gt": gts[j], "grid_exact": False, "cell_acc": 0.0,
                            "header_acc": 0.0, "tok_recall": 0.0,
                            "n_cells": gts[j]["n_rows"] * gts[j]["n_cols"],
                            "det_dims": None, "gt_dims": [gts[j]["n_rows"], gts[j]["n_cols"]],
                            "wrong": [], "unmatched": True})
    return {"false_positives": 0, "detected": len(det), "spurious": len(det) - len(used),
            "tables": out}


def check_expect(path, rec, res):
    """The closed `expect` vocabulary (§6.5): a case whose expectation cannot be checked
    mechanically does not enter the corpus, so every key here is enforced."""
    bad = set(rec["expect"]) - EXPECT_KEYS
    assert not bad, f"{os.path.basename(path)}: unknown expect keys {bad}"
    fails = []
    e, n = rec["expect"], res["detected"]
    if "table_count" in e and n != e["table_count"]:
        fails.append(f"table_count {n} != {e['table_count']}")
    if "table_count_any" in e and n not in e["table_count_any"]:
        fails.append(f"table_count {n} not in {e['table_count_any']}")
    if "cols_any" in e:
        cols = {t["det_dims"][1] for t in res["tables"] if t.get("det_dims")}
        if not (cols & set(e["cols_any"])):
            fails.append(f"cols {sorted(cols)} not in {e['cols_any']}")
    if e.get("must_not_merge"):
        # no detected table may carry tokens exclusive to two different GT tables
        det = detect(path)
        gts = [set().union(*[toks(c["text"]) for c in t["cells"]]) for t in rec["tables"]]
        excl = [g - set().union(*[o for k, o in enumerate(gts) if k != i])
                for i, g in enumerate(gts)]
        for d in det:
            dt = set().union(*[toks(c) for row in d["rows"] for c in row]) if d["rows"] else set()
            hit = [i for i, x in enumerate(excl) if len(dt & x) >= max(1, len(x) // 4)]
            if len(hit) > 1:
                fails.append(f"one detection carries cells from GT tables {hit}")
    if e.get("exactly_once"):
        import distillpdf
        html = distillpdf.Pdf.open(path).to_html(return_string=True)
        for t in rec["tables"]:
            for c in t["cells"]:
                txt = c["text"]
                if not txt or len(txt) <= 4:
                    continue
                # Word-bounded: `Benchmark` is a SUBSTRING of the panel's own heading
                # ("Benchmarks for evaluating groundwater quality"), and a plain `count`
                # read that as a duplicate emission. The panel's own text is exactly what
                # must survive, so the check has to be able to tell them apart.
                n = len(re.findall(r"(?<!\w)" + re.escape(txt) + r"(?!\w)", html))
                if n > 1:
                    fails.append(f"cell {txt!r} appears {n}x in the output")
    return fails


# ---------------------------------------------------------------------------- the harness

def score_corpus(outdir=CORPUS):
    truth = json.load(open(os.path.join(outdir, "truth.json")))
    files, agg = {}, []
    for fname, rec in sorted(truth["files"].items()):
        path = os.path.join(outdir, fname)
        res = score_file(path, rec)
        res["expect_fails"] = check_expect(path, rec, res)
        files[fname] = {"rec": rec, "res": res}
        agg += res["tables"]
    n = len(agg) or 1
    aggregate = {
        "cell_acc": sum(t["cell_acc"] for t in agg) / n,
        "grid_exact": sum(bool(t["grid_exact"]) for t in agg) / n,
        "header_acc": (sum(t["header_acc"] or 0.0 for t in agg)
                       / max(1, sum(1 for t in agg if t["header_acc"] is not None))),
        "tok_recall": sum(t["tok_recall"] for t in agg) / n,
        "n_tables": len(agg),
    }
    return {"files": files, "aggregate": aggregate}


def group_key(rec):
    """T1 gates per structural type x declaration variant; T2 gates per family (§8)."""
    if rec["tier"] == 1:
        return f"{rec['family']}|{'tagged' if rec['tagged'] else 'untagged'}"
    return rec["family"]


def group_scores(scored, tier):
    out = {}
    for fname, d in scored["files"].items():
        if d["rec"]["tier"] != tier:
            continue
        k = group_key(d["rec"])
        g = out.setdefault(k, {"cell_ok": 0, "cell_n": 0, "grid_ok": 0, "grid_n": 0,
                               "hdr_ok": 0.0, "hdr_n": 0, "files": []})
        g["files"].append(fname)
        for t in d["res"]["tables"]:
            g["cell_ok"] += t["cell_acc"] * t["n_cells"]
            g["cell_n"] += t["n_cells"]
            g["grid_ok"] += bool(t["grid_exact"])
            g["grid_n"] += 1
            if t["header_acc"] is not None:
                g["hdr_ok"] += t["header_acc"]
                g["hdr_n"] += 1
    for g in out.values():
        g["cell_acc"] = g["cell_ok"] / g["cell_n"] if g["cell_n"] else 1.0
        g["grid_exact"] = g["grid_ok"] / g["grid_n"] if g["grid_n"] else 1.0
        g["header_acc"] = g["hdr_ok"] / g["hdr_n"] if g["hdr_n"] else None
    return out


# -------------------------------------------------------------- scorer attack locks

def _attack_truth(rows, *, header_rows=1):
    cells = [
        {"r": r, "c": c, "text": value}
        for r, row in enumerate(rows)
        for c, value in enumerate(row)
    ]
    return {
        "page": 0,
        "n_rows": len(rows),
        "n_cols": len(rows[0]),
        "header_rows": header_rows,
        "bbox_norm": [0.1, 0.1, 0.9, 0.9],
        "cells": cells,
    }


def _attack_detection(rows, *, page=0):
    return {"page": page, "rows": rows, "raw_rows": rows,
            "n_rows": len(rows), "n_cols": len(rows[0]) if rows else 0}


def test_scorer_rejects_column_permutation_and_header_reassignment():
    truth = _attack_truth([["Region", "Revenue"], ["North", "-12.50"],
                           ["South", "9.75"]])
    perfect = _attack_detection([["Region", "Revenue"], ["North", "-12.50"],
                                 ["South", "9.75"]])
    swapped = _attack_detection([["Revenue", "Region"], ["-12.50", "North"],
                                 ["9.75", "South"]])
    assert score_pair(perfect, truth)["cell_acc"] == 1.0
    attacked = score_pair(swapped, truth)
    assert attacked["cell_acc"] < 1.0
    assert attacked["header_acc"] < 1.0
    assert attacked["tok_recall"] == 1.0, "attack must preserve every token"


@pytest.mark.parametrize("value", ["12.50", "-1250"])
def test_scorer_rejects_lost_sign_or_decimal(value):
    truth = _attack_truth([["Amount"], ["-12.50"]])
    attacked = score_pair(_attack_detection([["Amount"], [value]]), truth)
    assert attacked["cell_acc"] < 1.0


def test_scorer_rejects_duplicate_fused_split_and_phantom(monkeypatch):
    a = _attack_truth([["A"], ["alpha"]])
    b = _attack_truth([["B"], ["bravo"]])
    rec = {"tables": [a, b], "expect": {}}
    clean = [_attack_detection([["A"], ["alpha"]]),
             _attack_detection([["B"], ["bravo"]])]

    def measure(detections, tables=rec["tables"]):
        monkeypatch.setattr(__import__(__name__), "detect", lambda _path: detections)
        return score_file("synthetic.pdf", {"tables": tables, "expect": {}})

    assert score_is_perfect(measure(clean), 2)
    duplicated = measure(clean + [clean[0]])
    assert not score_is_perfect(duplicated, 2), "duplicate survived"
    assert duplicated["spurious"] == 1, "equal dicts must retain distinct source indices"
    fused = [_attack_detection([["A", "B"], ["alpha", "bravo"]])]
    assert not score_is_perfect(measure(fused), 2), "fusion survived"
    split = [_attack_detection([["A"], ["alpha"]]), _attack_detection([["B"]]),
             _attack_detection([["bravo"]])]
    assert not score_is_perfect(measure(split), 2), "split survived"
    phantom = _attack_detection([["PHANTOM"], ["noise"]])
    assert not score_is_perfect(measure(clean + [phantom]), 2), "phantom survived"
    assert not score_is_perfect(measure([phantom], tables=[]), 0), "negative phantom survived"


def test_scorer_rejects_invented_zero_header():
    truth = [{"header_rows": 0}]
    assert score_semantic_header_depth([{"header_rows": 0}], truth) == 1.0
    assert score_semantic_header_depth([{"header_rows": 1}], truth) == 0.0


def test_optional_geometry_and_span_attacks_fail_closed():
    truth = {"bbox_norm": [0.1, 0.1, 0.9, 0.9],
             "spans": [{"r": 0, "c": 0, "colspan": 2}]}
    assert bbox_iou_if_available({}, truth) is None
    assert span_topology_if_available({}, truth) is None
    assert bbox_iou_if_available({"bbox_norm": truth["bbox_norm"]}, truth) == 1.0
    assert bbox_iou_if_available({"bbox_norm": [0.5, 0.5, 1.0, 1.0]}, truth) < 0.5
    assert span_topology_if_available({"spans": truth["spans"]}, truth) is True
    corrupt = {"spans": [{"r": 0, "c": 0, "rowspan": 2}]}
    assert span_topology_if_available(corrupt, truth) is False
    assert SCORER_CAPABILITIES == {
        "bbox_iou": True, "span_topology": True, "cell_localization": True,
        "semantic_header_depth": True}


def _rich_attack_table(bbox, *, span=(1, 1), header_path=None, value="-12.50",
                       blank=False, cell_bbox=None):
    cells = [
        {"r": 0, "c": 0, "text": "Amount", "rowspan": 1, "colspan": 1,
         "role": "header", "header_path": [], "bbox_norm": [bbox[0], bbox[1], bbox[2], .3]},
        {"r": 1, "c": 0, "text": "" if blank else value,
         "rowspan": span[0], "colspan": span[1], "role": "data",
         "header_path": [[0, 0]] if header_path is None else header_path,
         "bbox_norm": cell_bbox or [bbox[0], .3, bbox[2], bbox[3]]},
    ]
    return {"page": 0, "bbox_norm": bbox, "n_rows": 1 + span[0],
            "n_cols": span[1], "header_rows": 1, "cells": cells,
            "evidence": ["ruled"]}


def test_rich_scorer_rejects_geometry_topology_header_numeric_and_blank_attacks():
    truth = _rich_attack_table([.1, .1, .9, .9])
    truth["cells"][1]["value_traits"] = ["numeric", "signed", "decimal"]
    perfect = _rich_attack_table([.1, .1, .9, .9])
    clean = score_analysis_pair(perfect, truth)
    assert clean["bbox_iou"] == clean["anchor_topology_f1"] == clean["anchor_localization_f1"] == 1.0
    assert clean["header_path_acc"] == clean["numeric"]["signed"] == 1.0

    shifted = _rich_attack_table([.5, .5, 1.0, 1.0], cell_bbox=[.5, .5, 1.0, 1.0])
    assert bbox_first_pairs([shifted], [truth]) == []
    assert score_analysis_pair(shifted, truth)["anchor_localization_f1"] < 1.0

    corrupt_span = _rich_attack_table([.1, .1, .9, .9], span=(2, 1))
    top = score_analysis_pair(corrupt_span, truth)
    assert top["span_exact"] < 1.0 and top["anchor_topology_f1"] < 1.0

    extra = _rich_attack_table([.1, .1, .9, .9])
    extra["cells"].append({"r": 1, "c": 1, "text": "extra", "rowspan": 1,
                           "colspan": 1, "role": "data", "header_path": [],
                           "bbox_norm": [.9, .3, 1.0, .9]})
    extra_score = score_analysis_pair(extra, truth)
    assert extra_score["anchor_topology_precision"] < 1.0
    assert extra_score["anchor_topology_recall"] == 1.0
    assert extra_score["anchor_topology_f1"] < 1.0

    wrong_header = _rich_attack_table([.1, .1, .9, .9], header_path=[[0, 1]])
    assert score_analysis_pair(wrong_header, truth)["header_path_acc"] < 1.0

    lost_sign = _rich_attack_table([.1, .1, .9, .9], value="12.50")
    assert score_analysis_pair(lost_sign, truth)["numeric"]["signed"] < 1.0

    blank_truth = _rich_attack_table([.1, .1, .9, .9], blank=True)
    blank_truth["cells"][1]["blank"] = True
    invented = _rich_attack_table([.1, .1, .9, .9], value="0")
    assert score_analysis_pair(invented, blank_truth)["blank_recall"] == 0.0


def test_bbox_segmentation_rejects_duplicate_fusion_split_and_phantom():
    left = _rich_attack_table([.05, .1, .45, .9])
    right = _rich_attack_table([.55, .1, .95, .9])
    clean = bbox_first_pairs([left, right], [left, right])
    assert not any(segmentation_counts([left, right], [left, right], clean).values())

    dup = bbox_first_pairs([left, left, right], [left, right])
    assert segmentation_counts([left, left, right], [left, right], dup)["duplicates"] == 1
    indexed = score_analysis_file(
        "synthetic.pdf", {"tables": [left, right]},
        detector=lambda _path: [left.copy(), left.copy(), right.copy()])
    assert indexed["duplicates"] == 1 and indexed["false_positives"] == 1
    fused = _rich_attack_table([.05, .1, .95, .9])
    assert segmentation_counts([fused], [left, right], bbox_first_pairs([fused], [left, right]))["fusions"] == 1
    upper = _rich_attack_table([.05, .1, .45, .5])
    lower = _rich_attack_table([.05, .5, .45, .9])
    assert segmentation_counts([upper, lower], [left], bbox_first_pairs([upper, lower], [left]))["splits"] == 1
    phantom = _rich_attack_table([.05, .92, .45, .99])
    assert segmentation_counts([left, right, phantom], [left, right],
                               bbox_first_pairs([left, right, phantom], [left, right]))["phantoms"] == 1

    invalid = _rich_attack_table([.9, .1, .1, .9])
    assert bbox_iou_if_available(invalid, left) == 0.0
    invalid["bbox_norm"] = [0.0, 0.0, float("nan"), 1.0]
    assert bbox_iou_if_available(invalid, left) == 0.0
    assert bbox_first_pairs([invalid], [left]) == []


def test_bbox_matching_maximizes_cardinality_before_iou_ties():
    adjacency = [[(0, .9), (1, .8)], [(0, .7)]]
    assert _maximum_cardinality(adjacency) == [(0, 1, .8), (1, 0, .7)]


def test_rich_matching_uses_token_fallback_without_crediting_geometry():
    truth = _rich_attack_table([.1, .1, .4, .9])
    shifted = _rich_attack_table([.6, .1, .9, .9])
    result = score_analysis_file("synthetic.pdf", {"tables": [truth]},
                                 detector=lambda _path: [shifted])
    assert result["matches"] == 1 and result["match_sources"] == {"bbox": 0, "token": 1}
    assert result["tables"][0]["bbox_iou"] is None
    assert result["tables"][0]["anchor_localization_f1"] is None

    unrelated = _rich_attack_table([.6, .1, .9, .9], value="unrelated")
    unrelated["cells"][0]["text"] = "Different"
    missed = score_analysis_file("synthetic.pdf", {"tables": [truth]},
                                 detector=lambda _path: [unrelated])
    assert missed["matches"] == 0 and missed["misses"] == 1


def test_end_to_end_topology_counts_misses_and_phantoms_outside_matched_diagnostic():
    left = _rich_attack_table([.05, .1, .45, .9])
    right = _rich_attack_table([.55, .1, .95, .9])
    right["cells"][1]["value_traits"] = ["numeric", "signed", "decimal"]
    phantom = _rich_attack_table([.05, .92, .45, .99], value="noise")
    phantom["cells"][0]["text"] = "Phantom"
    phantom["cells"][1]["text"] = ""
    result = score_analysis_file("synthetic.pdf", {"tables": [left, right]},
                                 detector=lambda _path: [left, phantom])
    assert result["tables"][0]["anchor_topology_f1"] == 1.0
    assert result["table_detection"]["f1"] == .5
    assert result["anchor_topology"]["f1"] == .5
    assert result["span"]["f1"] == .5
    assert result["header_depth_exact"] == .5
    assert result["header_path_acc"] == .5
    assert result["numeric_counts"]["signed"] == [0, 1]
    assert result["blank"]["f1"] == 0.0


def test_authored_numeric_traits_distinguish_grouping_from_decimals():
    from gen_table_corpus import _authored_value_traits

    assert "decimal" not in _authored_value_traits("1,234")
    assert "decimal" in _authored_value_traits("1,234.05")
    assert "decimal" in _authored_value_traits("1.234,05")
    assert "signed" in _authored_value_traits("($1,234.05)")


# ------------------------------------------------------------------------------- the gates

def load_floors(outdir=CORPUS):
    return json.load(open(os.path.join(outdir, "floors.json")))


def freeze_floors(outdir=CORPUS):
    """(Re)initialise `floors.json` at **measured minus slack**, bench100's `1/n + 0.005`
    rule (n = cells in the family), and record the red ledger for T3.

    Floors never move DOWN; only the owner revises a floor, promotes a tier, or accepts an
    oversimplification — in a PR-visible diff (§8)."""
    scored = score_corpus(outdir)
    old = {}
    fp = os.path.join(outdir, "floors.json")
    if os.path.exists(fp):
        old = json.load(open(fp))

    def slack(n):
        """bench100's `1/n + 0.005`, with **n = cells in the family** as the spec names it
        (§8). Keyed on cells rather than tables on purpose: a per-table denominator on a
        family of 1-3 tables gives a slack > 1, i.e. a floor of zero, which is not a gate."""
        return 1.0 / max(n, 1) + 0.005

    #: §6.2 — "Initial floor 0.98 for grid/booktabs/borderless". Applied where the measured
    #: value can carry it; a group that measures BELOW 0.98 records `below_spec_class` with
    #: the measurement, rather than being given a floor it fails on the day it is written.
    SPEC_098 = {"full_grid", "booktabs", "borderless"}

    out = {
        "note": ("Frozen floors for the table torture corpus. Policy (spec §8): floors never "
                 "move DOWN; a T3 `known_fail` runs as a strict xfail so a landed fix makes "
                 "the run fail in the GOOD direction and promotion is a one-line diff. "
                 "Standing rule (§10.1.2): a change that raises these scores while dropping "
                 "any bench100 floor is a FAILED change."),
        "note_t0": ("SPEC EXTENSION, stated rather than hidden: §6.1 gates T0 at a hard "
                    "`detected == 0` per file, but two of the seven negatives are red TODAY "
                    "and the spec itself names one of them a live corpus failure (the "
                    "three-column-newsletter phantom grid). Deleting them would remove the "
                    "evidence; passing them is not in this phase's scope. They therefore "
                    "carry the same RED LEDGER T3 uses: `known_fail` runs as a strict xfail, "
                    "so the day the false positive stops the run fails in the good direction "
                    "and promotion is a one-line diff. A negative is NEVER downgraded to "
                    "`watch` to make it green — `watch` is reserved for the one case (§6.1 "
                    "neg_form_grid) whose correct answer is an open policy question."),
        "t0": {}, "t1": {}, "t2": {}, "t3": {},
    }
    for fname, d in sorted(scored["files"].items()):
        if d["rec"]["tier"] != 0 or d["rec"]["variant"] == "watch":
            continue
        prev = (old.get("t0") or {}).get(fname, {})
        n_fp = d["res"]["false_positives"]
        out["t0"][fname] = {
            "expected": prev.get("expected") if prev.get("expected") == "pass" else
                        ("pass" if n_fp == 0 else "known_fail"),
            "measured": {"false_positives": n_fp},
        }
    for tier, key in ((1, "t1"), (2, "t2")):
        for k, g in sorted(group_scores(scored, tier).items()):
            prev = (old.get(key) or {}).get(k, {})
            sl = slack(g["cell_n"])
            cell_floor = max(0.0, g["cell_acc"] - sl)
            grid_floor = max(0.0, g["grid_exact"] - sl)
            fam = k.split("|")[0]
            floor = {
                "cell_acc": round(max(prev.get("cell_acc", 0.0), cell_floor), 4),
                "grid_exact": round(max(prev.get("grid_exact", 0.0), grid_floor), 4),
                "measured": {"cell_acc": round(g["cell_acc"], 4),
                             "grid_exact": round(g["grid_exact"], 4),
                             "header_acc": (round(g["header_acc"], 4)
                                            if g["header_acc"] is not None else None),
                             "n_cells": g["cell_n"], "n_tables": g["grid_n"]},
            }
            if tier == 1 and fam in SPEC_098:
                if g["cell_acc"] >= 0.98:
                    floor["cell_acc"] = round(max(floor["cell_acc"], 0.98), 4)
                else:
                    floor["below_spec_class"] = (
                        f"§6.2 asks 0.98 for {fam}; measured {g['cell_acc']:.4f}. Frozen at "
                        f"measured - slack instead of at a floor it fails on day one. See the "
                        f"phase report for the named defect this is attributable to.")
            out[key][k] = floor
    for fname, d in sorted(scored["files"].items()):
        if d["rec"]["tier"] != 3:
            continue
        res = d["res"]
        green = (not res["expect_fails"]
                 and all(t["grid_exact"] and t["cell_acc"] >= 0.98 for t in res["tables"]))
        prev = (old.get("t3") or {}).get(fname, {})
        out["t3"][fname] = {
            "expected": prev.get("expected") if prev.get("expected") == "pass" else
                        ("pass" if green else "known_fail"),
            "measured": {"cell_acc": round(sum(t["cell_acc"] for t in res["tables"])
                                           / max(1, len(res["tables"])), 4),
                         "grid_exact": all(t["grid_exact"] for t in res["tables"]),
                         "expect_fails": res["expect_fails"]},
        }
    with open(fp, "w") as f:
        json.dump(out, f, indent=1, sort_keys=True)
        f.write("\n")
    print(f"froze floors -> {fp}")
    return out


# ------------------------------------------------------------------------------ pytest API

_SCORED = None


def scored():
    global _SCORED
    if _SCORED is None:
        _SCORED = score_corpus()
    return _SCORED


TRUTH = json.load(open(os.path.join(CORPUS, "truth.json")))
FLOORS = json.load(open(os.path.join(CORPUS, "floors.json"))) \
    if os.path.exists(os.path.join(CORPUS, "floors.json")) else {"t1": {}, "t2": {}, "t3": {}}
T0 = [f for f, r in sorted(TRUTH["files"].items()) if r["tier"] == 0]
T3 = [f for f, r in sorted(TRUTH["files"].items()) if r["tier"] == 3]
COMPLEX_SEMANTIC_HEADER_FAMILIES = {
    "merged_colspan",
    "multitier_header",
    "no_header",
    "t3_kitchen_sink",
}
SEMANTIC_HEADER_LOCKS = [
    pytest.param(
        f,
        marks=pytest.mark.xfail(
            strict=True,
            reason=("G7 red ledger: zero-header inference needs structural table "
                    "segmentation/ownership evidence that separates real headers"),
        ),
    ) if r["family"] == "no_header" else f
    for f, r in sorted(TRUTH["files"].items())
    if (r["tier"] == 1
        or r["family"] == "tagged_only_signal"
        or r["family"] in COMPLEX_SEMANTIC_HEADER_FAMILIES)
]


def test_truth_schema():
    """Every case names a real source or is marked invented (§4.1), and carries a checkable
    `expect` block (§6.5). A case that cannot say where it came from is a case we made up."""
    assert TRUTH["schema"] == 3
    for fname, rec in TRUTH["files"].items():
        assert os.path.exists(os.path.join(CORPUS, fname)), fname
        assert ("source" in rec) ^ bool(rec.get("invented")), f"{fname}: source XOR invented"
        assert set(rec["expect"]) <= EXPECT_KEYS, f"{fname}: {set(rec['expect']) - EXPECT_KEYS}"
        assert isinstance(rec["tagged"], bool)
        for t in rec["tables"]:
            assert len(t["bbox_norm"]) == 4
            assert t["cells"], f"{fname}: a table with no cells is not ground truth"
            anchors = {(cell["r"], cell["c"]) for cell in t["cells"]}
            if "col_edges_norm" in t or "row_edges_norm" in t:
                assert len(t["col_edges_norm"]) == t["n_cols"] + 1
                assert len(t["row_edges_norm"]) == t["n_rows"] + 1
            for cell in t["cells"]:
                assert cell["role"] in {"header", "data"}, (fname, cell)
                if cell["role"] == "header":
                    assert cell.get("header") is True
                    assert cell["header_path"] == []
                else:
                    assert all(tuple(anchor) in anchors for anchor in cell["header_path"])
                assert set(cell.get("value_traits", ())) <= {
                    "numeric", "signed", "decimal", "percent", "unit"}
                assert (cell["r"], cell["c"]) in anchors


def _truth_rows(table):
    rows = [["" for _ in range(table["n_cols"])] for _ in range(table["n_rows"])]
    for cell in table["cells"]:
        rows[cell["r"]][cell["c"]] = cell["text"]
    return rows


def test_phase7_crosspage_fixture_truth_and_provenance_are_exact():
    """Freeze the four pre-candidate continuation controls, not just their filenames."""
    import distillpdf

    names = {
        "t3_crosspage_independent_geometry.pdf":
            "169a02c11f55a40e54cac8ea94633997da015d4e0bd356d09fd106725cf65e92",
        "t3_crosspage_independent_caption.pdf":
            "848398731a3bdbc8c83e639b7501cf5e4405d918a5745dd55dc0c4b3acc626ec",
        "t3_crosspage_aligned_prose.pdf":
            "2e1a9c759d4fce9442ac2b91eca209910a77c27a5af70d25862e4672f6a8fead",
        "t3_crosspage_header_text_data.pdf":
            "a4f13f5b619249fcec01989761a612151aabb585cf742b21360201920fcbea18",
    }
    for fname, expected_sha in names.items():
        path = os.path.join(CORPUS, fname)
        rec = TRUTH["files"][fname]
        assert hashlib.sha256(open(path, "rb").read()).hexdigest() == expected_sha
        assert distillpdf.Pdf.open(path).page_count() == 2
        assert rec["tier"] == 3 and rec["pages"] == 2 and rec.get("invented") is True
        assert rec["why"].startswith("Phase 7 preregistered")
        assert all(table["n_cols"] == 4 for table in rec["tables"])

    geometry = TRUTH["files"]["t3_crosspage_independent_geometry.pdf"]["tables"]
    assert [(table["page"], table["n_rows"]) for table in geometry] == [(0, 6), (1, 6)]
    assert geometry[0]["col_edges_norm"] != geometry[1]["col_edges_norm"]

    caption = TRUTH["files"]["t3_crosspage_independent_caption.pdf"]["tables"]
    assert [(table["page"], table["n_rows"]) for table in caption] == [(0, 6), (1, 6)]
    assert caption[0]["col_edges_norm"] == caption[1]["col_edges_norm"]
    assert _truth_rows(caption[0])[0] == _truth_rows(caption[1])[0]

    prose = TRUTH["files"]["t3_crosspage_aligned_prose.pdf"]["tables"]
    assert [(table["page"], table["n_rows"]) for table in prose] == [(0, 6)]

    continuation = TRUTH["files"]["t3_crosspage_header_text_data.pdf"]["tables"]
    assert [(table["page"], table["n_rows"]) for table in continuation] == [(0, 10)]
    table = continuation[0]
    assert _truth_rows(table)[5] == _truth_rows(table)[0]
    assert all(cell["role"] == "data" for cell in table["cells"] if cell["r"] == 5)
    assert [cell["header_path"] for cell in table["cells"] if cell["r"] == 5] == [
        [[0, 0]], [[0, 1]], [[0, 2]], [[0, 3]],
    ]


# The `<table ...>` open tags each negative emits, in document order. A table whose caption is
# anchored to it carries that caption's `id="tab-N"`; a table with no anchored caption is a bare
# `<table>`. Locked as a list rather than counted, because WHICH of them anchors is exactly the
# accepted behaviour this test exists to pin: before the heading rules stopped publishing a
# prominent caption as the page's document `<h1>`, "Table 7."/"Table 11." were consumed as a
# title and their tables carried no anchor at all.
CROSSPAGE_NEGATIVE_TABLE_TAGS = {
    "t3_crosspage_independent_geometry.pdf": ["<table>", "<table>"],
    "t3_crosspage_independent_caption.pdf": ['<table id="tab-7">', "<table>"],
    "t3_crosspage_aligned_prose.pdf": ['<table id="tab-11">'],
}


@pytest.mark.parametrize(
    "fname,expected_shapes",
    [
        ("t3_crosspage_independent_geometry.pdf", [(1, 6, 4), (2, 6, 4)]),
        ("t3_crosspage_independent_caption.pdf", [(1, 6, 4), (2, 6, 4)]),
        ("t3_crosspage_aligned_prose.pdf", [(1, 6, 4)]),
    ],
)
def test_phase7_crosspage_negatives_lock_accepted_parent_behavior(fname, expected_shapes):
    """The three hard negatives are exact accepted behavior before stitching exists."""
    import distillpdf

    path = os.path.join(CORPUS, fname)
    rec = TRUTH["files"][fname]
    pdf = distillpdf.Pdf.open(path)
    assert pdf.page_count() == 2
    expected = [
        {"page": table["page"] + 1, "n_rows": table["n_rows"],
         "n_cols": table["n_cols"], "cells": _truth_rows(table)}
        for table in rec["tables"]
    ]
    assert pdf.extract_tables() == expected
    analyzed = pdf.analyze_tables()
    assert [(table["page"], table["n_rows"], table["n_cols"])
            for table in analyzed] == expected_shapes

    html = pdf.to_html(return_string=True, image_mode="drop")
    markdown = pdf.to_markdown(return_string=True, image_mode="drop")
    assert re.findall(r"<table[^>]*>", html) == CROSSPAGE_NEGATIVE_TABLE_TAGS[fname]
    assert html.count("<table") == len(expected)
    assert markdown.count("| --- | --- | --- | --- |") == len(expected)

    if fname == "t3_crosspage_independent_caption.pdf":
        captions = ["Table 7. Permit register", "Table 8. Audit outcomes"]
        assert [table["caption"] for table in analyzed] == [None, None]
        # Once each. "Table 7." used to appear TWICE: the page's most prominent line was its
        # caption, so `find_document_title` published it as the document `<h1>` as well — the
        # same words emitted as a title and as the table's caption. It is now only the
        # table's `<caption>`, which is also why table 7 carries the `tab-7` anchor above.
        for caption in captions:
            assert html.count(caption) == 1
            assert markdown.count(caption) == 1
        assert '<caption style="caption-side:bottom">Table 7. Permit register</caption>' in html
        spans = [pdf._dbg_spans_xy(page) for page in (1, 2)]
        assert [
            hashlib.sha256(json.dumps(page, sort_keys=True, separators=(",", ":")).encode())
            .hexdigest()
            for page in spans
        ] == [
            "11d53c20c27733f0b37b05b2b793de3f3b20c31cd03aea48f54a3a5a88c0474d",
            "926ddef7c571ffe35c4bcc66669b2a181f26daf3b939927c0b86dff9b8852d6e",
        ]
        assert spans[0][-4:] == [
            ("Table ", 78.0, 188.39999389648438, 20.007999420166016, 8.0),
            ("7. ", 100.23199462890625, 188.39999389648438, 6.672000885009766, 8.0),
            ("Permit ", 109.12799835205078, 188.39999389648438, 23.11199951171875, 8.0),
            ("register", 134.4639892578125, 188.39999389648438, 26.6719970703125, 8.0),
        ]
        assert spans[1][:4] == [
            ("Table ", 78.0, 720.4000244140625, 20.007999420166016, 8.0),
            ("8. ", 100.23199462890625, 720.4000244140625, 6.672000885009766, 8.0),
            ("Audit ", 109.12799835205078, 720.4000244140625, 18.23199462890625, 8.0),
            ("outcomes", 129.583984375, 720.4000244140625, 34.67999267578125, 8.0),
        ]
    elif fname == "t3_crosspage_aligned_prose.pdf":
        prose = [
            "The field team continued its regional survey after dawn.",
            "These four aligned fragments form one sentence, not a continuation row.",
            "The narrative then resumes across the full text width on the second page.",
            "No word on this page belongs to the station table on the previous page.",
        ]
        for sentence in prose:
            assert html.count(sentence) == markdown.count(sentence) == 1


def test_phase7_header_text_data_desired_continuation_contract():
    """Text equality alone must not suppress the first page-2 row: it is authored data."""
    import distillpdf

    fname = "t3_crosspage_header_text_data.pdf"
    table = TRUTH["files"][fname]["tables"][0]
    rows = _truth_rows(table)
    pdf = distillpdf.Pdf.open(os.path.join(CORPUS, fname))
    assert pdf.extract_tables() == [
        {"page": 1, "n_rows": 10, "n_cols": 4, "cells": rows}
    ]
    analyzed = pdf.analyze_tables()
    assert len(analyzed) == 1
    assert (analyzed[0]["page"], analyzed[0]["n_rows"], analyzed[0]["n_cols"],
            analyzed[0]["header_rows"]) == (1, 10, 4, 1)
    row_five = [cell for cell in analyzed[0]["cells"] if cell["row"] == 5]
    assert [cell["text"] for cell in row_five] == rows[5]
    assert [cell["role"] for cell in row_five] == ["data"] * 4


@pytest.mark.parametrize(
    ("fname", "shape"),
    [
        ("t3_page_break.pdf", (40, 4)),
        ("t3_page_break_repeat.pdf", (40, 4)),
        ("t3_long_multipage.pdf", (60, 6)),
        ("t3_crosspage_header_text_data.pdf", (10, 4)),
    ],
)
def test_phase7_ruled_continuations_share_raw_rich_render_and_model_proof(
        fname, shape, tmp_path):
    import distillpdf

    pdf = distillpdf.Pdf.open(os.path.join(CORPUS, fname))
    extracted = pdf.extract_tables()
    assert [(table["page"], table["n_rows"], table["n_cols"])
            for table in extracted] == [(1, *shape)]
    analyzed = pdf.analyze_tables()
    assert [(table["page"], table["n_rows"], table["n_cols"])
            for table in analyzed] == [(1, *shape)]

    html = pdf.to_html(
        return_string=True, mode="page", toc=False, image_mode="drop",
    )
    markdown = pdf.to_markdown(
        return_string=True, mode="page", toc=False, image_mode="drop",
    )
    dpdf = pdf.distill(str(tmp_path / f"{fname}.dpdf"), assets="none")
    assert distillpdf.render_html(dpdf, mode="page", toc=False) == html
    assert distillpdf.render_markdown(
        dpdf, mode="page", toc=False, image_mode="drop",
    ) == markdown
    model = json.loads(distillpdf.load_model(dpdf))
    blocks = [block for block in model["blocks"] if block["kind"] == "table"]
    assert len(blocks) == 1
    assert (blocks[0]["page"], blocks[0]["cells"]) == (1, extracted[0]["cells"])


_PHASE7_OWNER_DIAGNOSTICS = {
    "t3_page_break.pdf": [(1, 40, 40, 161, "17ef928b8338e401")],
    "t3_page_break_repeat.pdf": [(1, 40, 40, 166, "30961bb22d6c7916")],
    "t3_long_multipage.pdf": [(1, 60, 60, 362, "5a31b64a7a796987")],
    "t3_crosspage_header_text_data.pdf": [(1, 10, 10, 40, "5d6993f37e3563ab")],
    "t3_crosspage_independent_geometry.pdf": [
        (1, 6, 6, 24, "c9c749902847616f"),
        (2, 6, 6, 24, "52931512b1d967a9"),
    ],
    "t3_crosspage_independent_caption.pdf": [
        (1, 6, 6, 29, "ab7f940ce626e56f"),
        (2, 6, 6, 29, "61b482c9cefb5042"),
    ],
}


@pytest.mark.parametrize("surface", ["raw", "rich", "render"])
@pytest.mark.parametrize("fname", _PHASE7_OWNER_DIAGNOSTICS)
def test_phase7_grouped_owner_diagnostic_is_exact_and_emitted_once(fname, surface):
    code = r"""
import distillpdf, sys
pdf = distillpdf.Pdf.open(sys.argv[1])
action = {
    "raw": pdf.extract_tables,
    "rich": pdf.analyze_tables,
    "render": lambda: pdf.to_html(return_string=True, image_mode="drop"),
}[sys.argv[2]]
action()
"""
    env = os.environ.copy()
    env["DPDF_TABLE_OWNERS"] = "1"
    run = subprocess.run(
        [sys.executable, "-c", code, os.path.join(CORPUS, fname), surface],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )
    lines = [line for line in run.stderr.splitlines()
             if line.startswith("DPDF_TABLE_OWNERS ")]
    parsed = []
    for line in lines:
        match = re.fullmatch(
            r"DPDF_TABLE_OWNERS page=(\d+) scope=detected candidate=frame:\d+ "
            r"bbox=[0-9a-f,]+ evidence=ruled(?:\+aligned)? rows=(\d+) "
            r"claim_rows=(\d+) slices=(\d+) hash=([0-9a-f]{16})",
            line,
        )
        assert match, line
        parsed.append((int(match[1]), int(match[2]), int(match[3]),
                       int(match[4]), match[5]))
    assert parsed == _PHASE7_OWNER_DIAGNOSTICS[fname]


def _rounded_analysis_report(value):
    """Give report-only floats a stable representation without hiding key drift."""
    if isinstance(value, float):
        return round(value, 10)
    if isinstance(value, dict):
        return {key: _rounded_analysis_report(item) for key, item in value.items()}
    if isinstance(value, list):
        return [_rounded_analysis_report(item) for item in value]
    return value


def test_analysis_report_baseline_is_complete_and_frozen():
    """Make every rich aggregate change review-visible; legacy floors remain the gate."""
    baseline_path = os.path.join(CORPUS, "analysis_baseline.json")
    expected = json.load(open(baseline_path))["metrics"]
    actual = _rounded_analysis_report(score_analysis_corpus()["aggregate"])
    missing = sorted(set(expected) - set(actual))
    extra = sorted(set(actual) - set(expected))
    assert not missing and not extra, (
        f"analysis baseline key drift: missing={missing}, extra={extra}")
    assert actual == expected, (
        "analysis report changed (report-only; do not lower legacy floors):\n"
        f"expected={json.dumps(expected, indent=2, sort_keys=True)}\n"
        f"actual={json.dumps(actual, indent=2, sort_keys=True)}")


def test_every_structural_type_exists_tagged_and_untagged():
    """Owner-mandated axis (§6.2): L0 reads declarations exactly and inference handles the
    rest — two entirely different code paths. A type that existed only tagged would leave
    inference untested for that shape; only untagged would leave L0 untested for it."""
    tagged = {r["family"] for r in TRUTH["files"].values() if r["tier"] == 1 and r["tagged"]}
    untagged = {r["family"] for r in TRUTH["files"].values()
                if r["tier"] == 1 and not r["tagged"]}
    assert tagged == untagged and len(tagged) == 5, (sorted(tagged), sorted(untagged))


@pytest.mark.parametrize("fname", SEMANTIC_HEADER_LOCKS)
def test_html_semantic_header_depth(fname):
    """Actual ``th`` depth, through both inferred HTML and declared L0, is exact.

    G5's original ``header_acc`` compares top-row *content* through ``extract_tables()``;
    it neither sees HTML tags nor reaches L0. Keep that positional metric, but lock semantics
    independently on every clean T1 shape, the two tagged-only L0 cases, and the T2/T3
    complex-header families whose committed per-file truth declares zero or multiple tiers.
    """
    rec = TRUTH["files"][fname]
    assert all(t["page"] == 0 for t in rec["tables"]), "semantic helper is page-local"
    det = detect_html_semantics(os.path.join(CORPUS, fname))
    truth_depths = {t.get("header_rows", 1) for t in rec["tables"]}
    if truth_depths == {0}:
        # Count/segmentation is gated independently (and these stacked T2 cases carry an
        # existing known-fail ledger).  Semantically, a fused emission made exclusively from
        # zero-header owners is still zero-header: never turn its first data row into `<th>`.
        assert det, f"{fname}: no semantic table emitted"
        assert all(t["header_rows"] == 0 for t in det), (
            f"{fname}: zero-header truth emitted depths "
            f"{[t['header_rows'] for t in det]}")
        return
    pairs = align(det, rec["tables"])
    assert len(pairs) == len(rec["tables"]), (
        f"{fname}: matched {len(pairs)} of {len(rec['tables'])} semantic tables")
    for di, gi in pairs:
        want = rec["tables"][gi].get("header_rows", 1)
        assert det[di]["header_rows"] == want, (
            f"{fname} table {gi}: {det[di]['header_rows']} leading all-th rows != {want}")


def test_booktabs_has_zero_vertical_rules():
    """§4.2.4 measured: arXiv booktabs has **exactly zero** vertical rules — not "few", none.
    Asserted on the committed BYTES, so a style command that draws more than it claims fails
    here as well as at generation time."""
    import _rawpdf
    for fname, rec in TRUTH["files"].items():
        if rec["family"] != "booktabs":
            continue
        n = _rawpdf.stroke_census(os.path.join(CORPUS, fname))
        assert n["v"] == 0, f"{fname}: {n['v']} vertical strokes; booktabs must have none"


@pytest.mark.parametrize("fname", T0)
def test_t0_negatives(fname, request):
    """T0: pages with ZERO tables that present tabular temptations. Hard `detected == 0`,
    with `neg_form_grid` the one designated WATCH exception (§6.1) — an IRS field grid at the
    measured shape, reported but not gated, because whether a field grid is a table is an
    open policy question the 38%-header statistic will decide.

    A negative that is red TODAY carries `known_fail` in floors.json and runs as a strict
    xfail (see `note_t0` there): the evidence stays in the corpus, and the fix flips the run
    red in the good direction."""
    rec = TRUTH["files"][fname]
    res = scored()["files"][fname]["res"]
    if rec["variant"] == "watch":
        assert not res["expect_fails"], res["expect_fails"]
        return
    if FLOORS.get("t0", {}).get(fname, {}).get("expected") == "known_fail":
        request.node.add_marker(pytest.mark.xfail(strict=True, reason="known_fail ledger"))
    assert res["false_positives"] == 0, (
        f"{fname}: {res['false_positives']} phantom table(s) on a page with none. "
        f"Every recall improvement in this project has threatened exactly this.")


@pytest.mark.parametrize("key", sorted(FLOORS.get("t1", {})))
def test_t1_floors(key):
    g = group_scores(scored(), 1)[key]
    fl = FLOORS["t1"][key]
    assert g["cell_acc"] >= fl["cell_acc"] - 1e-9, (
        f"T1 {key}: positional cell accuracy {g['cell_acc']:.4f} < floor {fl['cell_acc']}")
    assert g["grid_exact"] >= fl["grid_exact"] - 1e-9, (
        f"T1 {key}: exact-grid rate {g['grid_exact']:.4f} < floor {fl['grid_exact']}")


@pytest.mark.parametrize("key", sorted(FLOORS.get("t2", {})))
def test_t2_floors(key):
    g = group_scores(scored(), 2)[key]
    fl = FLOORS["t2"][key]
    assert g["cell_acc"] >= fl["cell_acc"] - 1e-9, (
        f"T2 {key}: positional cell accuracy {g['cell_acc']:.4f} < floor {fl['cell_acc']} "
        f"(files: {', '.join(g['files'])})")
    assert g["grid_exact"] >= fl["grid_exact"] - 1e-9, (
        f"T2 {key}: exact-grid rate {g['grid_exact']:.4f} < floor {fl['grid_exact']}")


@pytest.mark.parametrize("fname", T3)
def test_t3_red_ledger(fname, request):
    """T3 is a RED LEDGER, not a floor: each case declares `pass` or `known_fail`, and a
    `known_fail` runs as a **strict xfail** — so a landed fix turns it XPASS, the run fails in
    the good direction, and promotion is a one-line PR-visible diff. Without this, "floors
    never negotiate downward" would forbid committing the most valuable cases in the corpus."""
    entry = FLOORS.get("t3", {}).get(fname, {"expected": "pass"})
    if entry["expected"] == "known_fail":
        request.node.add_marker(pytest.mark.xfail(strict=True, reason="known_fail ledger"))
    res = scored()["files"][fname]["res"]
    assert not res["expect_fails"], f"{fname}: {res['expect_fails']}"
    for t in res["tables"]:
        assert t["grid_exact"], (f"{fname}: dims {t['det_dims']} != {t['gt_dims']}")
        assert t["cell_acc"] >= 0.98, (
            f"{fname}: positional cell accuracy {t['cell_acc']:.3f}; first misses "
            f"{t['wrong']}")


def report(outdir=CORPUS):
    s = score_corpus(outdir)
    lines = [f"{'case':44s} {'dims':>12s} {'cell':>6s} {'hdr':>6s} {'tok':>6s}  notes"]
    for fname, d in sorted(s["files"].items()):
        if not d["res"]["tables"]:
            lines.append(f"{fname:44s} {'NEG':>12s} {'':>6s} {'':>6s} {'':>6s}  "
                         f"detected={d['res']['detected']}")
            continue
        for t in d["res"]["tables"]:
            dims = f"{t['det_dims']}->{t['gt_dims']}" if t["det_dims"] else "MISS"
            lines.append(f"{fname:44s} {dims:>12s} {t['cell_acc']:6.3f} "
                         f"{(t['header_acc'] if t['header_acc'] is not None else -1):6.3f} "
                         f"{t['tok_recall']:6.3f}  "
                         f"{'; '.join(d['res']['expect_fails'])}")
    a = s["aggregate"]
    lines.append(f"\naggregate over {a['n_tables']} tables: cell {a['cell_acc']:.4f}  "
                 f"grid-exact {a['grid_exact']:.4f}  header {a['header_acc']:.4f}  "
                 f"token {a['tok_recall']:.4f}")
    return "\n".join(lines)


if __name__ == "__main__":
    import sys
    if "--freeze" in sys.argv:
        freeze_floors()
    print(report())
