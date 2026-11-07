#!/usr/bin/env python3
"""Compare one sweep with the frozen baseline. Pure data in, verdict out: no PDFs, no PyMuPDF,
no wheel, so tests/test_regression_gate.py can pin the verdict logic on a fresh clone.

The verdict, in the order it is decided:

  FAIL (exit 1)     a document or page lost, or any score below its baseline value on any
                    document, with no entry naming that exact drop.
  UNNAMED (exit 3)  output changed and nothing got worse by any score, but nobody has said why.
                    Scores are coarse: a word-order change between two equally-recalled words
                    moves none of them, so "no drop" is not "no change".
  PASS (exit 0)     byte-identical, or every change is named.

There is no score tolerance. A name is matched by the exact old and new hash, so a later
different change to the same document surfaces again instead of inheriting the name. A lost
document or page cannot be named at all: that takes a new baseline, signed.

Footprint and time never decide the verdict. They are reported beyond the tolerances below, with
the measured noise next to them, because on this machine other agents' builds run alongside.
"""
from __future__ import annotations

import statistics

#: Scores where higher is better and any decrease on a document is a drop.
SCORE_KEYS = (
    "recall", "jaccard", "order", "pages_recall", "md_recall", "html_recall",
    "uri_recall", "internal_recall", "internal_resolved",
    "gt_headings", "gt_tables", "gt_paragraphs", "gt_lists", "gt_images", "gt_fulldoc_headings",
)
#: Baseline-relative scores (headings/tables the baseline had that this build still has).
KEPT_KEYS = ("headings_kept", "tables_kept")

#: A document-operation is reported when the median grows by BOTH the ratio and the absolute
#: amount, and clears the baseline's own repeat range. Ratios alone would flag a 2 MiB form.
FOOTPRINT_RATIO, FOOTPRINT_ABS_MIB = 1.10, 8.0
INSTRUCTIONS_RATIO, INSTRUCTIONS_ABS_M = 1.10, 500
CPU_RATIO, CPU_ABS_S = 1.25, 0.5


def _ok(op: dict | None) -> bool:
    return bool(op) and "error" not in op and "crash" not in op


def lost_in(base_doc: dict, now_doc: dict) -> list[str]:
    """What this build no longer produces, in words a reader can act on."""
    out = []
    for engine, base_ops in base_doc.get("ops", {}).items():
        now_ops = now_doc.get("ops", {}).get(engine, {})
        for op, b in base_ops.items():
            n = now_ops.get(op)
            if n is None:
                continue  # not measured in this run (fast mode, engine filter)
            if _ok(b) and not _ok(n):
                why = (n or {}).get("error") or f"crash {(n or {}).get('crash')}"
                out.append(f"{engine}/{op} now fails: {why}")
                continue
            if not _ok(b):
                continue
            if b.get("page_count", 0) > n.get("page_count", 0):
                out.append(f"{engine}/{op} page count {b['page_count']} -> {n['page_count']}")
            if b.get("chars", 0) > 0 and n.get("chars", 0) == 0:
                out.append(f"{engine}/{op} output is now empty ({b['chars']} chars before)")
            if op == "pages":
                gone = sorted(set(n.get("empty_pages", [])) - set(b.get("empty_pages", [])))
                if gone:
                    out.append(f"{engine}/pages text lost on page(s) {gone[:12]}"
                               + (f" and {len(gone) - 12} more" if len(gone) > 12 else ""))
    return out


def changed_ops(base_doc: dict, now_doc: dict) -> list[tuple]:
    """(engine, op, base_sha, now_sha) for every measured output whose bytes moved."""
    out = []
    for engine, base_ops in base_doc.get("ops", {}).items():
        for op, b in base_ops.items():
            n = now_doc.get("ops", {}).get(engine, {}).get(op)
            if n is None:
                continue
            if _ok(b) and _ok(n):
                if b["sha"] != n["sha"]:
                    out.append((engine, op, b["sha"], n["sha"]))
            elif _ok(b) != _ok(n):
                out.append((engine, op, b.get("sha", "error"), n.get("sha", "error")))
    return out


def changed_pages(base_doc: dict, now_doc: dict, engine: str) -> list[int]:
    b = (base_doc.get("ops", {}).get(engine, {}).get("pages") or {}).get("page_hashes") or []
    n = (now_doc.get("ops", {}).get(engine, {}).get("pages") or {}).get("page_hashes") or []
    return [i + 1 for i in range(max(len(b), len(n)))
            if (b[i] if i < len(b) else None) != (n[i] if i < len(n) else None)]


def kept_scores(base: dict, now: dict, table_score) -> dict:
    """Of what the baseline emitted, the share this build still emits. Not a truth score: it
    catches a dropped heading rule or a collapsed table on the 53 documents no judge sampled."""
    out = {}
    if base.get("headings"):
        remaining = list(now.get("headings") or [])
        kept = 0
        for h in base["headings"]:
            if h in remaining:
                remaining.remove(h)
                kept += 1
        out["headings_kept"] = round(kept / len(base["headings"]), 4)
    if base.get("grids"):
        want = [{"rows": g[0], "cols": g[1], "has_header_row": g[2]} for g in base["grids"]]
        out["tables_kept"] = round(table_score(now.get("grids") or [], want)["score"], 4)
    return out


def score_drops(base: dict, now: dict, table_score) -> list[tuple]:
    """(metric, base, now) for every score below baseline. A score now missing counts as a drop:
    a scorer that stops reporting must not read as a scorer that stopped complaining."""
    drops = []
    for k in SCORE_KEYS:
        b, n = base.get(k), now.get(k)
        if b is None:
            continue
        if k.startswith("gt_") and now.get("_gt_not_run"):
            continue
        if n is None or n < b:
            drops.append((k, b, n))
    for k, v in kept_scores(base, now, table_score).items():
        if v < 1.0:
            drops.append((k, 1.0, v))
    return drops


def score_gains(base: dict, now: dict) -> list[tuple]:
    return [(k, base[k], now[k]) for k in SCORE_KEYS
            if base.get(k) is not None and now.get(k) is not None and now[k] > base[k]]


def _named_ops(named: list[dict]) -> dict:
    out = {}
    for entry in named:
        for slug, rows in entry.get("changes", {}).items():
            for r in rows:
                out[(slug, r["engine"], r["op"], r["from"], r["to"])] = entry["name"]
    return out


def _named_drops(named: list[dict]) -> dict:
    out = {}
    for entry in named:
        for slug, rows in entry.get("accepted_drops", {}).items():
            for r in rows:
                out[(slug, r["engine"], r["metric"], r["from"], r["to"])] = entry["name"]
    return out


def resource_flags(base_doc: dict, now_doc: dict) -> dict:
    """Footprint and time growth beyond tolerance for one document, for the report only."""
    mem, time_ = [], []
    for engine, ops in base_doc.get("ops", {}).items():
        for op, b in ops.items():
            n = now_doc.get("ops", {}).get(engine, {}).get(op)
            if not (_ok(b) and _ok(n)) or "m" not in b or "m" not in n:
                continue
            bm, nm = b["m"], n["m"]
            if "peak" in bm and "peak" in nm:
                bp, np_ = bm["peak"][0], nm["peak"][0]
                if (np_ > bp * FOOTPRINT_RATIO and np_ - bp > FOOTPRINT_ABS_MIB
                        and np_ > bm["peak"][2]):
                    mem.append((engine, op, bp, np_))
            if bm.get("instr") and nm.get("instr"):
                bi, ni = bm["instr"][0], nm["instr"][0]
                if ni > bi * INSTRUCTIONS_RATIO and ni - bi > INSTRUCTIONS_ABS_M and ni > bm["instr"][2]:
                    time_.append((engine, op, "Minstr", bi, ni))
            else:
                bc, nc = bm["cpu"][0], nm["cpu"][0]
                if nc > bc * CPU_RATIO and nc - bc > CPU_ABS_S and nc > bm["cpu"][2]:
                    time_.append((engine, op, "cpu_s", bc, nc))
    return {"footprint": mem, "time": time_}


def compare(baseline: dict, run: dict, named: list[dict], table_score) -> dict:
    """The verdict and everything the report needs. `run` covers baseline docs or a subset."""
    op_names = _named_ops(named)
    drop_names = _named_drops(named)
    result = {
        "identical": [], "changed": {}, "lost": {}, "drops": {}, "gains": {},
        "unnamed": {}, "named": {}, "unnamed_drops": {}, "named_drops": {},
        "footprint": {}, "time": {}, "missing": [], "route_changes": [],
    }
    for slug, b in baseline["docs"].items():
        n = run["docs"].get(slug)
        if n is None:
            if run.get("scope") == "all":
                result["missing"].append(slug)
            continue
        lost = lost_in(b, n)
        if lost:
            result["lost"][slug] = lost
        changed = changed_ops(b, n)
        for engine in b.get("ops", {}):
            if engine not in n.get("ops", {}):
                continue
            for op, bop in b["ops"][engine].items():
                nop = n["ops"][engine].get(op) or {}
                if bop.get("route") and nop.get("route") and bop["route"] != nop["route"]:
                    result["route_changes"].append((slug, engine, op, bop["route"], nop["route"]))
        if changed:
            result["changed"][slug] = {
                "ops": changed,
                "pages": {e: changed_pages(b, n, e) for e in n.get("ops", {})},
            }
        else:
            result["identical"].append(slug)
        for c in changed:
            key = (slug, *c)
            if key in op_names:
                result["named"].setdefault(slug, []).append((*c, op_names[key]))
            else:
                result["unnamed"].setdefault(slug, []).append(c)
        for engine, base_scores in b.get("scores", {}).items():
            now_scores = n.get("scores", {}).get(engine)
            if now_scores is None:
                continue
            drops = score_drops(base_scores, now_scores, table_score)
            gains = score_gains(base_scores, now_scores)
            if gains:
                result["gains"].setdefault(slug, []).extend((engine, *g) for g in gains)
            for metric, was, is_ in drops:
                if engine != "eager" and metric.startswith("gt_"):
                    continue  # the judged page scores come from one eager scorer run
                row = (engine, metric, was, is_)
                result["drops"].setdefault(slug, []).append(row)
                name = drop_names.get((slug, engine, metric, was, is_))
                if name:
                    result["named_drops"].setdefault(slug, []).append((*row, name))
                else:
                    result["unnamed_drops"].setdefault(slug, []).append(row)
        flags = resource_flags(b, n)
        if flags["footprint"]:
            result["footprint"][slug] = flags["footprint"]
        if flags["time"]:
            result["time"][slug] = flags["time"]

    if result["lost"] or result["unnamed_drops"] or result["missing"]:
        result["verdict"], result["exit"] = "FAIL", 1
    elif result["unnamed"]:
        result["verdict"], result["exit"] = "UNNAMED", 3
    else:
        result["verdict"], result["exit"] = "PASS", 0
    return result


def percentile(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    values = sorted(values)
    return values[min(len(values) - 1, int(q * len(values)))]


def noise_summary(run: dict) -> dict:
    """The repeat range this run itself saw, as (max-min)/median: the band a reader should
    discount any timing or footprint movement by. Blank with one repeat, because one repeat
    measures no noise; the report says so instead of printing 0%."""
    spreads: dict = {"wall": [], "cpu": [], "instr": [], "peak": []}
    for d in run["docs"].values():
        for ops in d.get("ops", {}).values():
            for op in ops.values():
                for k, v in (op.get("m") or {}).items():
                    if k in spreads and op["m"].get("n", 1) > 1 and v[0]:
                        spreads[k].append((v[2] - v[1]) / v[0])
    return {k: {"p50": round(percentile(v, 0.5), 4), "p95": round(percentile(v, 0.95), 4), "n": len(v)}
            for k, v in spreads.items() if v}


def totals(docs: dict, slugs=None) -> dict:
    """Corpus sums of medians per engine and operation, and the mean of each score."""
    out: dict = {}
    scores: dict = {}
    for slug, d in docs.items():
        if slugs is not None and slug not in slugs:
            continue
        for engine, ops in d.get("ops", {}).items():
            for op, r in ops.items():
                if not _ok(r) or "m" not in r:
                    continue
                t = out.setdefault(f"{engine}/{op}", {"wall_s": 0.0, "cpu_s": 0.0, "Minstr": 0, "peak_mib_max": 0.0})
                t["wall_s"] += r["m"]["wall"][0]
                t["cpu_s"] += r["m"]["cpu"][0]
                if r["m"].get("instr"):
                    t["Minstr"] += r["m"]["instr"][0]
                if r["m"].get("peak"):
                    t["peak_mib_max"] = max(t["peak_mib_max"], r["m"]["peak"][0])
        for k, v in (d.get("scores", {}).get("eager") or {}).items():
            if isinstance(v, (int, float)) and k in SCORE_KEYS:
                scores.setdefault(k, []).append(v)
    for t in out.values():
        t["wall_s"] = round(t["wall_s"], 1)
        t["cpu_s"] = round(t["cpu_s"], 1)
    return {"ops": out,
            "score_means": {k: {"mean": round(statistics.mean(v), 4), "docs": len(v)}
                            for k, v in sorted(scores.items())}}
