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

Known deviation from the spec, stated rather than hidden
--------------------------------------------------------
§8 opens matching with "bbox IoU >= 0.5 first, token-overlap fallback". The public API's
`extract_tables()` exposes `page`/`n_rows`/`n_cols`/`cells` and **no bbox**, so IoU matching
is not implementable today. Matching is therefore page-scoped, order-preserving, maximum-
weight on token overlap — the same monotone alignment bench100 uses, for the same reason
(both sides are in reading order). `bbox_norm` is still recorded on every ground-truth table,
so the day the API exposes table geometry this becomes a one-function change. The cases where
IoU would matter most (`t3_interleaved`, `t3_adjacent_no_fuse`) are additionally gated by
`table_count` / `must_not_merge`, which do not need geometry.
"""
from __future__ import annotations

import json
import html as html_module
import os
import re
import unicodedata

import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "table_corpus")
WORD = re.compile(r"\w+", re.UNICODE)

#: The closed `expect` vocabulary (§7). A key outside this set is a truth-file bug, not a
#: silently-ignored expectation.
EXPECT_KEYS = {"table_count", "table_count_any", "cols_any", "must_not_merge", "exactly_once"}


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


def score_file(path, rec):
    det = detect(path)
    tables = rec["tables"]
    if not tables:
        return {"false_positives": len(det), "detected": len(det), "tables": []}
    out, used = [], set()
    for pg in sorted({t["page"] for t in tables}):
        gts = [t for t in tables if t["page"] == pg]
        ds = [d for d in det if d["page"] == pg]
        idx = [det.index(d) for d in ds]
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
    "t3_kitchen_sink",
}
SEMANTIC_HEADER_LOCKS = [
    f for f, r in sorted(TRUTH["files"].items())
    if (r["tier"] == 1
        or r["family"] == "tagged_only_signal"
        or r["family"] in COMPLEX_SEMANTIC_HEADER_FAMILIES)
]


def test_truth_schema():
    """Every case names a real source or is marked invented (§4.1), and carries a checkable
    `expect` block (§6.5). A case that cannot say where it came from is a case we made up."""
    assert TRUTH["schema"] == 2
    for fname, rec in TRUTH["files"].items():
        assert os.path.exists(os.path.join(CORPUS, fname)), fname
        assert ("source" in rec) ^ bool(rec.get("invented")), f"{fname}: source XOR invented"
        assert set(rec["expect"]) <= EXPECT_KEYS, f"{fname}: {set(rec['expect']) - EXPECT_KEYS}"
        assert isinstance(rec["tagged"], bool)
        for t in rec["tables"]:
            assert len(t["bbox_norm"]) == 4
            assert t["cells"], f"{fname}: a table with no cells is not ground truth"


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
    complex-header families whose committed per-file truth declares more than one tier.
    """
    rec = TRUTH["files"][fname]
    assert all(t["page"] == 0 for t in rec["tables"]), "semantic helper is page-local"
    det = detect_html_semantics(os.path.join(CORPUS, fname))
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
