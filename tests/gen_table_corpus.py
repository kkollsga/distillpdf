#!/usr/bin/env python3
"""Generate the **table torture corpus**: PDFs whose every cell's text, position and span
are KNOWN, emitted together with `truth.json` from the same in-memory objects.

Spec: the table torture corpus spec (rev 2). This file implements it; it does
not redesign it. Read §4 (source fidelity — the acceptance criterion) before changing a
case, and §13 for the build order this file follows.

Why it exists, in one line each:
  * bench100's table score is `0.5*count + 0.5*rows x cols` — it reads **1.0 when every
    value sits under the wrong header**;
  * `tests/test_tables.py`'s cell recall is a token **set** — the same content scores 1.0
    wherever the tokens land.
Both blind spots close only if the corpus knows cell POSITIONS, which a generated corpus
does by construction and a judged one never can.

Usage
-----
    python3 tests/gen_table_corpus.py                 # emit PDFs + truth.json
    python3 tests/gen_table_corpus.py --parity        # + the §4.3/§4.4 parity gate report
                                                      #   (needs the local benchmarking/ tree)
    python3 tests/gen_table_corpus.py --freeze        # (re)initialise floors.json
    python3 tests/gen_table_corpus.py --audit-seed 7  # §10.1.3 seed-sensitivity audit

Determinism (hard requirement — `scripts/check-fixture-drift.sh` regenerates this tree at a
DIFFERENT path and diffs it):
  * `rl_config.invariant = 1` before any reportlab document exists;
  * no images at all, so the reportlab image-name defect G1 fixed (an absolute path digested
    into `/FormXob.<md5>`) cannot recur here;
  * every case seeds its own `random.Random` from a CRC of its id, so adding or removing a
    case never churns another one's content, and `hash()` (PYTHONHASHSEED-salted) is unused;
  * `json.dump(..., sort_keys=True)`.
"""
from __future__ import annotations

import argparse
import json
import os
import random
import sys
import unicodedata
import zlib

from reportlab import rl_config

# Must precede any DocTemplate: without it reportlab stamps a wall-clock /CreationDate and
# seeds the trailer /ID from it, rewriting every file on every run.
rl_config.invariant = 1

from reportlab.lib import colors                                          # noqa: E402
from reportlab.lib.pagesizes import letter                                # noqa: E402
from reportlab.lib.styles import ParagraphStyle, getSampleStyleSheet      # noqa: E402
from reportlab.lib.units import inch                                      # noqa: E402
from reportlab.pdfgen import canvas as rl_canvas                          # noqa: E402
from reportlab.platypus import (BaseDocTemplate, Frame, PageTemplate,     # noqa: E402
                                PageBreak, Paragraph, SimpleDocTemplate, Spacer,
                                Table, TableStyle)

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _rawpdf as RAW                                                     # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "table_corpus")
SEED = 42
PW, PH = letter

S = getSampleStyleSheet()
CELL = ParagraphStyle("cell", fontSize=8, leading=9.5)
BODY = ParagraphStyle("body", parent=S["BodyText"], fontSize=9.5, leading=12)

TRUTH: dict = {}

# ---------------------------------------------------------------------------- provenance
#
# Frozen at build step 0 by the §4.1 median rule — "the page of that family whose bench100
# table score is nearest the family MEDIAN, never the best or worst" — evaluated against the
# frozen bench100 accuracy baseline.
# Frozen as literals on purpose: benchmarking/ is licence-encumbered and absent in CI, and
# truth.json must be reproducible without it.
#
# `C_real` is recorded for the report only; the parity run re-reads it from the live
# accuracy file so a stale literal can never silently pass a case.
SRC = {
    "arxiv":            ("cs_SE_2607.28331", 4, 0.9286),
    "forms.irs":        ("forms_irs_f1065", 6, 0.5974),
    "gov.nasa":         ("gov_nasa_ntrs20240002628", 326, 0.7917),
    "gov.usgs":         ("gov_usgs_usgs70278109", 2, 0.9097),
    "policy.worldbank": ("policy_worldbank_wbD34442284", 18, 0.4583),
    "standards.ietf":   ("standards_ietf_rfc9420", 113, 0.9222),
    # measured TYPES (bench100 GT records no table style, so these were typed off the page)
    "type.borderless":  ("math_NA_2607.28104", 35, 0.7786),
    "type.tagged":      ("policy_worldbank_wbD34442285", 11, 0.8974),
    # named in the spec rather than resolved by the median rule
    "usgs.hazard":      ("gov_usgs_usgs70277338", 1, 0.9444),
    "usgs.newsletter":  ("gov_usgs_usgs70277647", 1, 0.6330),
}


#: §4.3/§7 — individual generated cases measured OVERSIMPLIFIED against their named source
#: and, after enrichment was attempted and failed, kept with the flag and **excluded from
#: the coverage claim**. The disposition is deliberately per file: a hard sibling must not
#: license an easy one, and an accepted easy case must not erase a sibling that is honestly
#: within band. Recorded in `truth.json` so the claim is computable offline and reproduced
#: with its reason in the §4.4 report. A per-file violation not named here is unexplained and
#: fails the run.
_BORDERLESS_OVERSIMPLIFIED = (
        "Enrichment attempted (measured 12x6 shape, then multi-word crowding so the gutters "
        "nearly close — the gutter IS the structure in a whitespace-only table) and the "
        "extractor still reads it exactly: C_gen 0.9952 vs C_real 0.7786 on "
        "math_NA_2607.28104 p35. Enrichment beyond this point is IMPOSSIBLE WITHIN T1's "
        "definition: §5 defines T1 as one unambiguous structure with NO confounds and floors "
        "near perfection, while the source page's 0.7786 comes from confounds T1 forbids. "
        "The two requirements cannot both hold for this family, so the flag stays and "
        "`borderless` is NOT claimed as source-covered. The untagged/tagged borderless "
        "carriers remain valid REGRESSION locks — which is what T1 is for."
)
_MULTITIER_OVERSIMPLIFIED = (
        "Enrichment attempted (widened 5 -> 9 columns at the source page's own width, then "
        "deepened 3 -> 4 header tiers) and the extractor still scores C_gen 1.0000 vs C_real "
        "0.7917 on gov_nasa_ntrs20240002628 p326. Root cause of the gap, measured: that "
        "page's own table is 1 row x 9 columns, so its difficulty is a DEGENERATE SINGLE-ROW "
        "shape, not header tiers. Enriching toward it would mean importing a different "
        "confound and destroying §5's 'exactly ONE challenge per case' property that makes a "
        "T2 failure name its cause. Flag kept; `multitier_header` is not claimed as "
        "source-covered."
)
_WIDE_SHALLOW_RULED_OVERSIMPLIFIED = (
    "The frozen §4.1 source is the World Bank family-median-score page. Visual and ground-"
    "truth inspection shows its difficulty comes from a 13-column ruled table with three "
    "merged header tiers; distillPDF currently duplicates it as two 3x13 emissions. The "
    "generated ruled carrier deliberately isolates the T2 `wide_shallow` challenge at the "
    "family's measured 2x7 shape. Adding merged/multitier headers or producer-specific "
    "duplicate triggers would import a second confound and violate §6.3's exactly-one-"
    "challenge contract. The source cannot be changed without violating the frozen median "
    "rule, so this file remains a regression lock but is excluded from source coverage; "
    "its borderless sibling is adjudicated independently."
)
_WRAPPED_GRID_OVERSIMPLIFIED = (
    "The frozen §4.1 source is the World Bank family-median-score page. Visual and ground-"
    "truth inspection shows its difficulty comes from a 13-column ruled table with three "
    "merged header tiers; distillPDF currently duplicates it as two 3x13 emissions. This "
    "generated ruled carrier deliberately isolates the T2 `wrapped_cells` challenge. "
    "Adding the source's merged/multitier header structure or producer-specific duplicate "
    "triggers would import a second confound and violate §6.3's exactly-one-challenge "
    "contract. The source cannot be changed without violating the frozen median rule, so "
    "this file remains a regression lock but is excluded from source coverage; its "
    "borderless sibling is adjudicated independently."
)

OVERSIMPLIFIED = {
    **{fname: _BORDERLESS_OVERSIMPLIFIED for fname in (
        "t1_borderless_source_median.pdf", "t1_borderless_small.pdf",
        "t1_borderless_in_prose.pdf", "t1_borderless_tagged.pdf")},
    **{fname: _MULTITIER_OVERSIMPLIFIED for fname in (
        "t2_multitier_header_two_tier.pdf", "t2_multitier_header_four_tier.pdf")},
    "t2_wide_shallow_ruled.pdf": _WIDE_SHALLOW_RULED_OVERSIMPLIFIED,
    "t2_wrapped_cells_in_grid.pdf": _WRAPPED_GRID_OVERSIMPLIFIED,
}


def src(key, mimics):
    slug, page, _ = SRC[key]
    return {"doc": slug, "page": page, "corpus": "bench88", "mimics": mimics}


def bench54(mimics):
    """No SEC filings exist in the 88-doc selection (spec §10.4.2), so every SEC-shaped case
    is reconstructed from the earlier corpus and its parity is UNRUNNABLE. Stated here, once,
    so it cannot be quietly upgraded."""
    return {"doc": "bench54", "page": None, "corpus": "bench54", "mimics": mimics}


def external(doc, mimics):
    return {"doc": doc, "page": None, "corpus": "external", "mimics": mimics}


def bench25(doc, page, mimics):
    """The 25-document `benchmarking/corpus_tests/` corpus. It has cell-level content truth
    but no rows/cols, so the bench100 `C` metric is not computable there: parity unrunnable.
    (The spec labels `med_crispr` "(bench88)"; it is not in the 88-doc selection — see the
    phase report.)"""
    return {"doc": doc, "page": page, "corpus": "bench25", "mimics": mimics}


# ------------------------------------------------------------------------------- content

HEADERS = ["Name", "Region", "Samples", "Depth m", "Grade %", "Zone", "Unit"]
WORDS = ["Alpha", "Beta", "Gamma", "Delta", "Basalt", "Granite", "Shale", "North", "South",
         "East", "West", "Quartz", "Olivine", "Feldspar", "Pyrite", "Calcite", "Ridge",
         "Basin", "Layer", "Terrace"]
PROSE = ("Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor "
         "incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis "
         "nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.")


def rng_for(case_id: str) -> random.Random:
    """A per-case RNG seeded from a CRC of the case id — stable across interpreters (unlike
    `hash()`) and independent of every other case, so adding one case cannot churn another's
    committed bytes."""
    return random.Random(SEED ^ zlib.crc32(case_id.encode()))


def mkgrid(case_id, rows, cols, header=True, numeric=(), blank_frac=0.0, long_text=False):
    """`long_text` crowds the cells so the inter-column gutters nearly close — the §4.3
    enrichment axis for a whitespace-only table, where the gutter IS the structure."""
    r = rng_for(case_id)
    out = []
    if header:
        out.append([HEADERS[c % len(HEADERS)] for c in range(cols)])
    for i in range(rows - (1 if header else 0)):
        row = []
        for c in range(cols):
            if blank_frac and r.random() < blank_frac:
                row.append("")
            elif c in numeric:
                row.append(f"{r.randint(1, 999)}.{r.randint(0, 9)}")
            elif long_text:
                row.append(f"{r.choice(WORDS)} {r.choice(WORDS).lower()} {i + 1}")
            else:
                row.append(f"{r.choice(WORDS)}{i + 1}")
        out.append(row)
    return out


def _authored_value_traits(value):
    """Semantic traits for a value the generator owns, before PDF rendering."""
    s = str(value)
    traits = ["numeric"]
    if s.startswith("-") or (s.startswith("(") and s.endswith(")")):
        traits.append("signed")
    # A lone comma followed by exactly three digits is an authored grouping separator
    # (`1,234`), not a decimal. Dot decimals and mixed US/EU separators remain decimal.
    comma_tail = s.rsplit(",", 1)[-1] if "," in s else ""
    if "." in s or ("," in s and ("." in s or len(comma_tail.rstrip(")")) != 3)):
        traits.append("decimal")
    if "%" in s:
        traits.append("percent")
    if "$" in s or "€" in s or any(ch.isalpha() for ch in s):
        traits.append("unit")
    return traits


def to_cells(grid, header_rows=1, spans=(), *, numeric=()):
    """The §7 explicit cell list from a rectangular grid of strings.

    `spans` is a list of (r, c, rowspan, colspan); covered slots are OMITTED (span text lives
    on the anchor), which is exactly what the scorer credits."""
    covered = set()
    span_at = {}
    for r, c, rs, cs in spans:
        span_at[(r, c)] = (rs, cs)
        for dr in range(rs):
            for dc in range(cs):
                if (dr, dc) != (0, 0):
                    covered.add((r + dr, c + dc))
    header_anchors = []
    for r in range(min(header_rows, len(grid))):
        for c in range(len(grid[r])):
            if (r, c) in covered:
                continue
            _, cs = span_at.get((r, c), (1, 1))
            header_anchors.append((r, c, cs))
    out = []
    for r, row in enumerate(grid):
        for c, txt in enumerate(row):
            if (r, c) in covered:
                continue
            rec = {"r": r, "c": c, "text": unicodedata.normalize("NFC", str(txt))}
            rs, cs = span_at.get((r, c), (1, 1))
            if rs > 1:
                rec["rowspan"] = rs
            if cs > 1:
                rec["colspan"] = cs
            if r < header_rows:
                rec["header"] = True
                rec["role"] = "header"
                rec["header_path"] = []
            else:
                rec["role"] = "data"
                rec["header_path"] = [[hr, hc] for hr, hc, hcs in header_anchors
                                      if hc <= c < hc + hcs]
            if not str(txt):
                rec["blank"] = True
            if r >= header_rows and c in numeric and str(txt):
                rec["value_traits"] = _authored_value_traits(txt)
            out.append(rec)
    return out


# ------------------------------------------------------------------- platypus with a bbox

_DRAWS: list = []


class LocTable(Table):
    """A `Table` that records where it actually landed.

    `bbox_norm` is required on every table (§7) and is load-bearing for the T3
    interleaved/adjacent matching, so it is captured from the canvas at draw time rather than
    predicted. A table that SPLITS across pages draws once per piece and reportlab's split
    builds fresh instances, so `split` re-stamps the id — otherwise a 3-page table's draws
    are anonymous and its bbox unrecoverable."""
    tid = None

    def draw(self):
        x, y = self.canv.absolutePosition(0, 0)
        _DRAWS.append({"tid": self.tid, "page": self.canv.getPageNumber() - 1,
                       "x": x, "y": y, "w": self._width, "h": self._height,
                       "col_widths": list(self._colWidths),
                       "row_heights": list(self._rowHeights)})
        Table.draw(self)

    def split(self, aW, aH):
        parts = Table.split(self, aW, aH)
        for p in parts:
            if isinstance(p, LocTable):
                p.tid = self.tid
        return parts


def _norm_bbox(d, pw=PW, ph=PH):
    return [round(d["x"] / pw, 4), round((ph - d["y"] - d["h"]) / ph, 4),
            round((d["x"] + d["w"]) / pw, 4), round((ph - d["y"]) / ph, 4)]


def _cell_axes(d, grid, pw=PW, ph=PH):
    """Compact exact authored cell boundaries as normalized column/row edges."""
    widths, heights = d.get("col_widths", ()), d.get("row_heights", ())
    if len(widths) != max(len(row) for row in grid) or len(heights) != len(grid):
        return None  # split/hand-authored geometry is unavailable, never guessed
    xs = [d["x"]]
    for width in widths:
        xs.append(xs[-1] + width)
    ys = [ph - (d["y"] + d["h"])]
    for height in heights:
        ys.append(ys[-1] + height)
    return {"col_edges_norm": [round(x / pw, 4) for x in xs],
            "row_edges_norm": [round(y / ph, 4) for y in ys]}


def build_doc(fname, story, pagesize=(PW, PH), frames=None):
    """Build one platypus document and return the per-table draw records, in order."""
    _DRAWS.clear()
    path = os.path.join(OUT, fname)
    if frames is None:
        SimpleDocTemplate(path, pagesize=pagesize, topMargin=0.8 * inch,
                          bottomMargin=0.8 * inch).build(story)
    else:
        doc = BaseDocTemplate(path, pagesize=pagesize, topMargin=0.8 * inch,
                              bottomMargin=0.8 * inch)
        doc.addPageTemplates(PageTemplate(frames=frames))
        doc.build(story)
    grouped, cur = [], None
    for d in _DRAWS:
        if cur is not None and (d["tid"] is None or d["tid"] == cur["tid"]):
            cur["draws"].append(d)
        else:
            cur = {"tid": d["tid"], "draws": [d]}
            grouped.append(cur)
    return grouped


def _enrich_truth_table(table):
    """Complete schema-3 roles and authored column-header paths for every producer."""
    header_anchors = [
        (cell["r"], cell["c"], cell.get("colspan", 1))
        for cell in table["cells"]
        if cell.get("header") or cell["r"] < table.get("header_rows", 1)
    ]
    for cell in table["cells"]:
        if cell.get("header") or cell["r"] < table.get("header_rows", 1):
            cell["header"] = True
            cell["role"] = "header"
            cell["header_path"] = []
        else:
            cell["role"] = "data"
            cell["header_path"] = [[r, c] for r, c, colspan in header_anchors
                                   if c <= cell["c"] < c + colspan]


def emit(fname, *, tier, family, variant, tagged, tables, expect,
         source=None, invented=None, pages=1, note=None, parity=None):
    assert source or invented, f"{fname}: every case names a source or is marked invented (§4.1)"
    for table in tables:
        _enrich_truth_table(table)
    rec = {"tier": tier, "family": family, "variant": variant, "tagged": tagged,
           "pages": pages, "tables": tables, "expect": expect}
    if source:
        rec["source"] = source
    else:
        rec["invented"] = True
        rec["why"] = invented
    if note:
        rec["note"] = note
    if parity:
        rec["parity"] = parity          # "unrunnable" — flagged, never silently skipped
    if fname in OVERSIMPLIFIED:
        rec["oversimplified"] = True
        rec["oversimplified_why"] = OVERSIMPLIFIED[fname]
    TRUTH[fname] = rec
    return rec


def tbl(draws, grid, *, style, header_rows=1, spans=(), page=0, numeric=()):
    d = draws[0]
    table = {"page": d.get("page", page), "bbox_norm": _norm_bbox(d),
             "n_rows": len(grid), "n_cols": max(len(r) for r in grid),
             "style": style, "header_rows": header_rows,
             "cells": to_cells(grid, header_rows, spans, numeric=numeric)}
    axes = _cell_axes(d, grid)
    if axes:
        table.update(axes)
    return table


def flow(grid, style, *, tid, colw=None, header_rows=1, numeric=(), spans=(),
         font=8.5, wrap_cols=(), row_h=None):
    """One styled `LocTable` for a logical grid, plus the TableStyle the family needs."""
    ncols = max(len(r) for r in grid)
    colw = colw or [(6.4 * inch) / ncols] * ncols
    pdf = [[Paragraph(str(c), CELL) if ci in wrap_cols and str(c) else str(c)
            for ci, c in enumerate(row)] for row in grid]
    t = LocTable(pdf, colWidths=colw, rowHeights=row_h)
    t.tid = tid
    cmds = [("FONTSIZE", (0, 0), (-1, -1), font),
            ("LEFTPADDING", (0, 0), (-1, -1), 4), ("RIGHTPADDING", (0, 0), (-1, -1), 4),
            ("TOPPADDING", (0, 0), (-1, -1), 2), ("BOTTOMPADDING", (0, 0), (-1, -1), 2),
            ("VALIGN", (0, 0), (-1, -1), "MIDDLE")]
    for c in numeric:
        cmds.append(("ALIGN", (c, 0), (c, -1), "RIGHT"))
    if header_rows:
        cmds.append(("FONTNAME", (0, 0), (-1, header_rows - 1), "Helvetica-Bold"))
    if style == "full_grid":
        cmds += [("GRID", (0, 0), (-1, -1), 0.5, colors.black)]
        if header_rows:
            cmds.append(("BACKGROUND", (0, 0), (-1, header_rows - 1), colors.Color(.85, .85, .91)))
    elif style == "booktabs":
        cmds += [("LINEABOVE", (0, 0), (-1, 0), 1.0, colors.black),
                 ("LINEBELOW", (0, header_rows - 1), (-1, header_rows - 1), 0.5, colors.black),
                 ("LINEBELOW", (0, -1), (-1, -1), 1.0, colors.black)]
    elif style == "column_ruled":
        cmds += [("LINEBEFORE", (0, 0), (-1, -1), 0.5, colors.black),
                 ("LINEAFTER", (-1, 0), (-1, -1), 0.5, colors.black)]
    elif style == "zebra":
        cmds += [("ROWBACKGROUNDS", (0, header_rows), (-1, -1),
                  [colors.Color(.90, .90, .94), colors.white])]
    elif style == "overdecorated":
        cmds += [("GRID", (0, 0), (-1, -1), 0.5, colors.black),
                 ("BOX", (0, 0), (-1, -1), 2.0, colors.black),
                 ("INNERGRID", (0, 0), (-1, -1), 0.5, colors.grey)]
    for r, c, rs, cs in spans:
        cmds.append(("SPAN", (c, r), (c + cs - 1, r + rs - 1)))
    t.setStyle(TableStyle(cmds))
    return t


def assert_no_verticals(fname):
    """§4.2.4, as an assertion on the EMITTED BYTES: arXiv booktabs has **exactly zero**
    vertical rules. A stray vertical is a different structural type, and a corpus whose
    `booktabs` cases quietly carry one is testing something it does not name."""
    n = RAW.stroke_census(os.path.join(OUT, fname))
    if n["v"] != 0:
        raise AssertionError(
            f"{fname}: booktabs must contain ZERO vertical strokes (spec §4.2.4, measured "
            f"v-rule median 0 over n=28 arXiv tables) — found {n['v']}. A vertical rule makes "
            f"this a different type; fix the style, do not relax the assertion.")


# =====================================================================================
# T0 — Negatives: pages with ZERO tables that present tabular temptations.
# Gate: hard `detected == 0` per file, one designated watch exception (§6.1).
# =====================================================================================

def t0_negatives():
    # --- 1. three-column newsletter prose read as three tables ---------------------------
    fw = (PW - 2 * inch) / 3 - 10
    frames = [Frame(inch + i * (fw + 15), inch, fw, PH - 1.6 * inch) for i in range(3)]
    build_doc("t0_neg_newsletter3.pdf", [Paragraph(PROSE, BODY) for _ in range(12)],
              frames=frames)
    emit("t0_neg_newsletter3.pdf", tier=0, family="neg_newsletter3", variant="only",
         tagged=False, tables=[], expect={"table_count": 0},
         source=src("usgs.newsletter", "3-column newsletter prose whose column gutters are a "
                    "phantom grid; the live corpus false positive this locks"))

    # --- 2. two-column academic prose ---------------------------------------------------
    fw = (PW - 2 * inch) / 2 - 8
    frames = [Frame(inch, inch, fw, PH - 1.6 * inch),
              Frame(inch + fw + 16, inch, fw, PH - 1.6 * inch)]
    build_doc("t0_neg_twocol_prose.pdf", [Paragraph(PROSE, BODY) for _ in range(8)],
              frames=frames)
    emit("t0_neg_twocol_prose.pdf", tier=0, family="neg_twocol_prose", variant="only",
         tagged=False, tables=[], expect={"table_count": 0},
         invented="generic two-column academic layout; migrated verbatim from gen_tables.py")

    # --- 3. flowing sentences naming fields and values ----------------------------------
    r = rng_for("neg_labelvalue")
    build_doc("t0_neg_labelvalue.pdf",
              [Paragraph(f"The {r.choice(WORDS).lower()} field at site {i} measured "
                         f"{r.randint(1, 99)} units during the survey, which the team "
                         f"recorded alongside the surrounding {r.choice(WORDS).lower()} "
                         f"readings before moving on to the next location.", BODY)
               for i in range(12)], frames=None)
    emit("t0_neg_labelvalue.pdf", tier=0, family="neg_labelvalue", variant="prose", tagged=False,
         tables=[], expect={"table_count": 0},
         invented="label-and-value content as flowing sentences; migrated from gen_tables.py")

    # --- 4. the COMPACT label:value lookalike (the hard half) ----------------------------
    # G1 deleted `tbl_18_neg_labelvalue.pdf` as an orphan and its replacement was re-authored
    # as full prose, so the hard shape — two ragged columns of short tokens that look exactly
    # like a borderless table — left the corpus. Re-authored here, per the todos.md handoff.
    r = rng_for("neg_compact_labelvalue")
    c = rl_canvas.Canvas(os.path.join(OUT, "t0_neg_compact_labelvalue.pdf"), pagesize=(PW, PH))
    c.setFont("Helvetica-Bold", 13)
    c.drawString(72, 730, "Field notes")
    c.setFont("Helvetica", 10)
    y = 700
    for i in range(16):
        c.drawString(72, y, f"{r.choice(WORDS)} field {i}: {r.randint(1, 99)}")
        y -= 16
    c.setFont("Helvetica", 9.5)
    c.drawString(72, y - 14, "Readings were logged by hand and transcribed the following week.")
    c.save()
    emit("t0_neg_compact_labelvalue.pdf", tier=0, family="neg_labelvalue", variant="compact",
         tagged=False, tables=[], expect={"table_count": 0},
         invented="compact `Olivine field 0: 43` lines — ragged short-token pairs that look "
                  "like a borderless 2-column table; the hard negative G1's fixture cleanup "
                  "left unrepresented (dev-docs/todos.md, re-author in G5)")

    # --- 5. display equations at fixed column x's (migrated L4, verbatim) ---------------
    c = rl_canvas.Canvas(os.path.join(OUT, "t0_neg_equation.pdf"), pagesize=(PW, PH))
    c.setFont("Helvetica", 11)
    y = 640
    for i in range(4):
        c.drawString(90, y, f"(3.{i + 1}) y")
        c.drawString(220, y, "= a x")
        c.drawString(330, y, f"+ b {i + 1}")
        y -= 26
    c.save()
    emit("t0_neg_equation.pdf", tier=0, family="neg_equation", variant="only", tagged=False,
         tables=[], expect={"table_count": 0},
         source=bench54("display equations whose terms sit at fixed column x's — the "
                        "math-as-table class the math_PR documents carry; migrated verbatim "
                        "from gen_tables.py L4"),
         parity="unrunnable")

    # --- 6. glossary: term + long wrapped definition (migrated L3, verbatim) ------------
    glossary = [
        ["Basalt", "A dark fine-grained volcanic rock that forms from the rapid cooling of "
                   "magnesium-rich lava exposed at the surface of the crust."],
        ["Granite", "A coarse-grained intrusive igneous rock composed mainly of quartz and "
                    "feldspar that crystallises slowly deep below the ground."],
        ["Shale", "A fine-grained sedimentary rock formed from the compaction of silt and "
                  "clay particles over very long geological periods of time."],
        ["Quartz", "A hard crystalline mineral made of silicon dioxide that is found widely "
                   "across many different rock types and mineral veins worldwide."],
    ]
    t = LocTable([[term, Paragraph(desc, CELL)] for term, desc in glossary],
                 colWidths=[1.0 * inch, 4.4 * inch])
    t.setStyle(TableStyle([("FONTSIZE", (0, 0), (-1, -1), 9),
                           ("VALIGN", (0, 0), (-1, -1), "TOP")]))
    build_doc("t0_neg_glossary.pdf", [t])
    emit("t0_neg_glossary.pdf", tier=0, family="neg_glossary", variant="only", tagged=False,
         tables=[], expect={"table_count": 0},
         invented="term + wrapped definition rows; reaches the column model but must be "
                  "refused as prose (migrated verbatim from gen_tables.py L3)")

    # --- 7. TOC with dot leaders and right-aligned page numbers -------------------------
    c = rl_canvas.Canvas(os.path.join(OUT, "t0_neg_toc_leaders.pdf"), pagesize=(PW, PH))
    c.setFont("Helvetica-Bold", 15)
    c.drawString(72, 730, "Contents")
    c.setFont("Helvetica", 10.5)
    y = 700
    for i, name in enumerate(["Introduction", "Methods and materials", "Regional setting",
                              "Sampling programme", "Analytical results", "Discussion",
                              "Conclusions", "Acknowledgements", "References",
                              "Appendix A Field logs", "Appendix B Laboratory data"]):
        label = f"{i + 1}  {name} "
        c.drawString(90, y, label)
        w = c.stringWidth(label, "Helvetica", 10.5)
        dots = int((470 - (90 + w)) / c.stringWidth(".", "Helvetica", 10.5))
        c.drawString(90 + w, y, "." * max(dots, 0))
        num = str(3 + i * 7)
        c.drawRightString(500, y, num)
        y -= 19
    c.save()
    emit("t0_neg_toc_leaders.pdf", tier=0, family="neg_toc_leaders", variant="only",
         tagged=False, tables=[], expect={"table_count": 0},
         invented="dot-leader TOC: a left column, a right-aligned numeric column and a "
                  "perfectly regular row pitch — every table signal but the table")

    # --- 8. the IRS field grid, at the MEASURED shape (the watch case) ------------------
    # 7 rows x 3 cols, 92 h-rules, 111 v-rules, 0 fills and NO HEADER ROW: headers appear on
    # only 38% of forms.irs table pages (§4.2.2) — these are field grids, not data tables.
    # Scored and reported, NOT gated: `table_count_any: [0, 1]`. The 38% statistic is the
    # evidence the eventual policy decision cites.
    c = rl_canvas.Canvas(os.path.join(OUT, "t0_neg_form_grid.pdf"), pagesize=(PW, PH))
    c.setFont("Helvetica-Bold", 12)
    c.drawString(72, 740, "Schedule K  Partners' Distributive Share Items")
    x0, y0, cw, rh = 72.0, 300.0, 156.0, 54.0
    r = rng_for("neg_form_grid")
    c.setLineWidth(0.5)
    h_drawn = v_drawn = 0
    for i in range(8):                                   # main grid: 8 h
        c.line(x0, y0 + i * rh, x0 + 3 * cw, y0 + i * rh)
        h_drawn += 1
    for j in range(4):                                   # main grid: 4 v
        c.line(x0 + j * cw, y0, x0 + j * cw, y0 + 7 * rh)
        v_drawn += 1
    for i in range(7):
        for j in range(3):
            bx, by = x0 + j * cw + 6, y0 + i * rh + 6
            c.rect(bx, by, cw - 12, rh - 24)             # entry box: 2 h + 2 v
            h_drawn += 2
            v_drawn += 2
            # Two rules per field (caption underline + subtotal rule) and three money-column
            # ticks: the counts are chosen so the EMITTED census lands on the measured
            # medians 92 h / 111 v exactly (8 + 21*4 = 92; 4 + 21*5 + 2 = 111), asserted below.
            c.line(bx, by + rh - 26, bx + cw - 12, by + rh - 26)
            c.line(bx, by + 2, bx + cw - 12, by + 2)
            h_drawn += 2
            for k in range(1, 4):
                c.line(bx + k * (cw - 12) / 4.0, by, bx + k * (cw - 12) / 4.0, by + rh - 24)
                v_drawn += 1
            c.setFont("Helvetica", 6.5)
            c.drawString(bx + 2, by + rh - 22, f"{i * 3 + j + 1}  {HEADERS[(i + j) % 7]}")
            c.setFont("Helvetica", 9)
            c.drawString(bx + 4, by + 5, f"{r.randint(1, 999):,}")
    for k in range(2):                                   # the two remaining verticals
        c.line(72 + k * 3 * cw / 2.0, 282, 72 + k * 3 * cw / 2.0, 296)
    c.setFont("Helvetica", 8)
    c.drawString(72, 270, "Attach to Form 1065. See separate instructions.")
    c.save()
    got = RAW.stroke_census(os.path.join(OUT, "t0_neg_form_grid.pdf"))
    if (got["h"], got["v"], got["fills"]) != (92, 111, 0):
        raise AssertionError(
            f"neg_form_grid must carry the MEASURED forms.irs rule census 92 h / 111 v / 0 "
            f"fills (§4.2) — emitted {got['h']}/{got['v']}/{got['fills']}. This case exists to "
            f"be built to the measurement, not to a plausible number.")
    emit("t0_neg_form_grid.pdf", tier=0, family="neg_form_grid", variant="watch", tagged=False,
         tables=[], expect={"table_count_any": [0, 1]},
         source=src("forms.irs", "IRS field grid at the measured shape: 7x3, no header row "
                    "(headers appear on only 38% of forms.irs table pages), heavily ruled"),
         note=f"WATCH: scored and reported, NOT gated. Emitted rule census "
              f"h={got['h']} v={got['v']} fills={got['fills']} against the measured medians "
              f"92/111/0. Whether a field grid is a table is an open policy question; the 38% "
              f"header statistic is the evidence that decision cites.")


# =====================================================================================
# T1 — Clean singles: one table, one unambiguous structure, built to a MEASURED shape,
# in tagged AND untagged variants (§6.2). Gate: hard floors near perfection.
# =====================================================================================

T1_TYPES = {
    # type -> (rows, cols, header_rows, source-key, mimics)
    "full_grid": (10, 6, 1, "usgs.hazard",
                  "hazard-zone table: family medians 10x6, 100% header, ruled both ways, "
                  "shaded header band and zone-tint fills (8 fills measured)"),
    "booktabs": (5, 5, 1, "arxiv",
                 "arXiv booktabs: medians 5x5, 89% header, 12 h-rules and EXACTLY ZERO "
                 "vertical rules"),
    "borderless": (12, 6, 1, "type.borderless",
                   "whitespace-only table: measured over the 4 bench88 GT table pages whose "
                   "page carries no rule at all — medians 12 rows x 6 cols, 100% header"),
    "column_ruled": (6, 4, 1, None,
                     "vertical rules only, no horizontals — measured ABSENT from the corpus; "
                     "exists precisely so it stops being untested"),
    "zebra": (6, 4, 1, None,
              "row-background shading as the only row signal — hypothesised, measured absent"),
}


def t1_clean_singles():
    for typ, (rows, cols, hdr, skey, mimics) in T1_TYPES.items():
        prov = (dict(source=src(skey, mimics)) if skey else dict(invented=mimics))

        # ---- variant 1: source_median (the measured shape) -----------------------------
        cid = f"t1_{typ}_source_median"
        grid = mkgrid(cid, rows, cols, numeric=(cols - 1,), long_text=(typ == "borderless"))
        extra = []
        if typ == "full_grid":                      # the 8 measured zone-tint fills
            r = rng_for(cid + "fills")
            for _ in range(8):
                rr, cc = r.randint(1, rows - 1), r.randint(0, cols - 1)
                extra.append(("BACKGROUND", (cc, rr), (cc, rr), colors.Color(.94, .91, .84)))
        t = flow(grid, typ, tid=cid, numeric=(cols - 1,), header_rows=hdr)
        if extra:
            t.setStyle(TableStyle(extra))
        g = build_doc(f"{cid}.pdf", [Paragraph(f"Table 1. {typ} at the source median shape",
                                               S["Heading3"]), t])
        emit(f"{cid}.pdf", tier=1, family=typ, variant="source_median", tagged=False,
             tables=[tbl(g[0]["draws"], grid, style=typ, header_rows=hdr)],
             expect={"table_count": 1}, **prov)
        if typ == "booktabs":
            assert_no_verticals(f"{cid}.pdf")

        # ---- variant 2: small (4 cols x 5 rows, 1 header row, 1 right-aligned numeric) --
        cid = f"t1_{typ}_small"
        grid = mkgrid(cid, 5, 4, numeric=(3,))
        t = flow(grid, typ, tid=cid, numeric=(3,))
        g = build_doc(f"{cid}.pdf", [t])
        emit(f"{cid}.pdf", tier=1, family=typ, variant="small", tagged=False,
             tables=[tbl(g[0]["draws"], grid, style=typ)], expect={"table_count": 1}, **prov)
        if typ == "booktabs":
            assert_no_verticals(f"{cid}.pdf")

        # ---- variant 3: in_prose -------------------------------------------------------
        cid = f"t1_{typ}_in_prose"
        grid = mkgrid(cid, 5, 4, numeric=(3,))
        t = flow(grid, typ, tid=cid, numeric=(3,))
        g = build_doc(f"{cid}.pdf", [Paragraph(PROSE, BODY), Spacer(1, 14), t, Spacer(1, 14),
                                     Paragraph(PROSE, BODY)])
        emit(f"{cid}.pdf", tier=1, family=typ, variant="in_prose", tagged=False,
             tables=[tbl(g[0]["draws"], grid, style=typ)], expect={"table_count": 1}, **prov)
        if typ == "booktabs":
            assert_no_verticals(f"{cid}.pdf")

        # ---- variant 4: TAGGED — the source_median geometry, declared -------------------
        # Owner-mandated axis (§6.2): L0 reads declarations exactly and inference handles the
        # rest — two entirely different code paths. Same visual, different path.
        cid = f"t1_{typ}_tagged"
        grid = mkgrid(cid, rows, cols, numeric=(cols - 1,))
        gd = [[{"text": v} for v in row] for row in grid]
        rec = RAW.tagged_table(os.path.join(OUT, f"{cid}.pdf"), gd, style=typ,
                               header_rows=hdr, col_w=min(76.0, 460.0 / cols),
                               y_top=700.0 - 0, row_h=18.0,
                               title=f"Table 1. {typ}, declared")
        rec["page"] = 0
        emit(f"{cid}.pdf", tier=1, family=typ, variant="tagged", tagged=True,
             tables=[rec], expect={"table_count": 1}, **prov)
        if typ == "booktabs":
            assert_no_verticals(f"{cid}.pdf")


# =====================================================================================
# T2 — Single confound: exactly ONE challenge per case on an otherwise-T1 carrier, so a
# failure names its cause. Two variants per family make the CARRIER the diagnosis (§6.3).
# =====================================================================================

def t2_confounds():
    usgs = src("gov.usgs", "USGS data table at the family median shape (10x6, 100% header, "
                           "ruled both ways, fills)")
    wb = src("policy.worldbank", "World Bank policy table: measured 2 rows x 7 cols, 76% "
                                 "header, n=34 — the second-largest population in the corpus")
    ietf = src("standards.ietf", "IETF RFC page: 7x4 of content under 174 h-rules, 120 "
                                 "v-rules and 42 fills — decoration >> structure")
    irs = src("forms.irs", "IRS continuation grid: data rows only, no header row (headers "
                           "appear on only 38% of forms.irs table pages)")
    nasa = src("gov.nasa", "NASA grouped-header table; family medians 4 rows x 2 cols, 44% "
                           "header. Widened to 4 columns so a header TIER is expressible — "
                           "the 2-column median cannot carry one")
    arx = src("arxiv", "arXiv results table with footnote markers in cells")
    tagsrc = src("type.tagged", "a bench88 document that declares /StructTreeRoot /Table; "
                                "measured over the 10 GT table pages in tagged documents")

    def two(fam, prov, mk, parity=None):
        """Emit the two variants of one T2 family."""
        for variant, builder in mk.items():
            cid = f"t2_{fam}_{variant}"
            tables, expect, note = builder(cid)
            emit(f"{cid}.pdf", tier=2, family=fam, variant=variant, tagged=False,
                 tables=tables, expect=expect, note=note, parity=parity, **prov)

    # --- blank_cells --------------------------------------------------------------------
    def blanks(frac):
        def build(cid):
            grid = mkgrid(cid, 10, 6, numeric=(5,), blank_frac=frac)
            for c in range(6):                    # every column keeps at least one value
                if all(not grid[r][c] for r in range(1, 10)):
                    grid[1][c] = f"{WORDS[c]}1"
            g = build_doc(f"{cid}.pdf", [flow(grid, "full_grid", tid=cid, numeric=(5,))])
            n = sum(1 for row in grid[1:] for v in row if not v)
            return ([tbl(g[0]["draws"], grid, style="full_grid")], {"table_count": 1},
                    f"{n} blank cells of {9 * 6}; blanks are the stated core full-grid loss")
        return build
    two("blank_cells", dict(source=usgs), {"grid_10pct": blanks(0.10), "grid_30pct": blanks(0.30)})

    # --- merged_colspan -----------------------------------------------------------------
    def colspan(style):
        def build(cid):
            grid = [["Geochemistry", "", "", "Location", "", ""],
                    ["Sample", "Depth", "Grade", "Lat", "Lon", "Zone"]]
            r = rng_for(cid)
            for i in range(6):
                grid.append([f"S{i + 1}", f"{i * 3 + 2}.1", f"{i + 1}.4",
                             f"{60 + i}.2", f"{10 + i}.5", r.choice(WORDS)])
            spans = [(0, 0, 1, 3), (0, 3, 1, 3)]
            t = flow(grid, style, tid=cid, header_rows=2, spans=spans)
            g = build_doc(f"{cid}.pdf", [t])
            return ([tbl(g[0]["draws"], grid, style=style, header_rows=2, spans=spans)],
                    {"table_count": 1},
                    "span anchor carries the text; covered slots are empty; header "
                    "attribution must be correct across the span")
        return build
    two("merged_colspan", dict(source=usgs),
        {"over_grid": colspan("full_grid"), "over_booktabs": colspan("booktabs")})

    # --- merged_rowspan -----------------------------------------------------------------
    def rowspan(style):
        def build(cid):
            grid = [["Zone", "Sample", "Value", "Unit"]]
            r = rng_for(cid)
            for z in ("North", "South"):
                for i in range(3):
                    grid.append([z if i == 0 else "", f"{z[0]}{i + 1}",
                                 f"{r.randint(1, 99)}", "ppm"])
            spans = [(1, 0, 3, 1), (4, 0, 3, 1)]
            g = build_doc(f"{cid}.pdf", [flow(grid, style, tid=cid, spans=spans)])
            return ([tbl(g[0]["draws"], grid, style=style, spans=spans)], {"table_count": 1},
                    "value cells keep their own rows; the span text is not smeared down them")
        return build
    two("merged_rowspan", dict(source=usgs),
        {"in_grid": rowspan("full_grid"), "in_borderless": rowspan("borderless")})

    # --- multitier_header ---------------------------------------------------------------
    def multitier(tiers):
        # ENRICHED after the first parity run (§4.3): at 5 columns both variants scored
        # C_gen 1.0000 against C_real 0.7917 on NASA p326 — a gap of +0.208, oversimplified.
        # The parity page's own table is 1 row x 9 columns, so the carrier is widened toward
        # that measured width (7 / 9 columns) and crowded to an 8-column-per-6.4in pitch. The
        # confound stays exactly one: header tiers.
        def build(cid):
            if tiers == 2:
                grid = [["", "Phase 1", "", "", "Phase 2", "", ""],
                        ["ID", "Min", "Mean", "Max", "Min", "Mean", "Max"]]
                spans = [(0, 1, 1, 3), (0, 4, 1, 3)]
                hdr = 2
            else:
                grid = [["", "Campaign", "", "", "", "", "", "", ""],
                        ["", "Site A", "", "", "", "Site B", "", "", ""],
                        ["", "Phase 1", "", "Phase 2", "", "Phase 1", "", "Phase 2", ""],
                        ["ID", "Min", "Max", "Min", "Max", "Min", "Max", "Min", "Max"]]
                spans = [(0, 1, 1, 8), (1, 1, 1, 4), (1, 5, 1, 4),
                         (2, 1, 1, 2), (2, 3, 1, 2), (2, 5, 1, 2), (2, 7, 1, 2)]
                hdr = 4
            n = len(grid[-1])
            r = rng_for(cid)
            for i in range(5):
                grid.append([f"R{i + 1}"] + [f"{r.randint(1, 50)}" for _ in range(n - 1)])
            g = build_doc(f"{cid}.pdf", [flow(grid, "full_grid", tid=cid, font=7,
                                              header_rows=hdr, spans=spans)])
            return ([tbl(g[0]["draws"], grid, style="full_grid", header_rows=hdr, spans=spans)],
                    {"table_count": 1},
                    "the full header PATH above each body cell must be recoverable (§8.3)")
        return build
    two("multitier_header", dict(source=nasa),
        {"two_tier": multitier(2), "four_tier": multitier(4)})

    # --- band_rows ----------------------------------------------------------------------
    def band(style, shaded):
        def build(cid):
            grid = [["Unit", "Depth m", "Grade %", "Samples"]]
            r = rng_for(cid)
            bands = []
            for name in ("Upper sequence", "Lower sequence"):
                bands.append(len(grid))
                grid.append([name, "", "", ""])
                for i in range(3):
                    grid.append([f"{r.choice(WORDS)}{i + 1}", f"{r.randint(1, 99)}.{i}",
                                 f"{r.randint(1, 9)}.{i}", f"{r.randint(10, 99)}"])
            spans = [(b, 0, 1, 4) for b in bands]
            t = flow(grid, style, tid=cid, spans=spans)
            cmds = [("FONTNAME", (0, b), (-1, b), "Helvetica-Bold") for b in bands]
            if shaded:
                cmds += [("BACKGROUND", (0, b), (-1, b), colors.Color(.87, .87, .93))
                         for b in bands]
            t.setStyle(TableStyle(cmds))
            g = build_doc(f"{cid}.pdf", [t])
            return ([tbl(g[0]["draws"], grid, style=style, spans=spans)], {"table_count": 1},
                    "ONE table: the band row is a full-span row, not a table terminator")
        return build
    two("band_rows", dict(source=usgs),
        {"bold_in_grid": band("full_grid", False), "shaded_booktabs": band("booktabs", True)})

    # --- wide_shallow (the measured World Bank shape — 2 rows x 7 cols) -----------------
    def wide(style):
        def build(cid):
            r = rng_for(cid)
            grid = [["Indicator", "2019", "2020", "2021", "2022", "2023", "Trend"],
                    ["GDP growth", *[f"{r.randint(-3, 9)}.{r.randint(0, 9)}%" for _ in range(5)],
                     r.choice(["rising", "flat", "falling"])]]
            g = build_doc(f"{cid}.pdf", [Paragraph(PROSE, BODY), Spacer(1, 12),
                                         flow(grid, style, tid=cid, numeric=(1, 2, 3, 4, 5)),
                                         Spacer(1, 12), Paragraph(PROSE, BODY)])
            return ([tbl(g[0]["draws"], grid, style=style, numeric=(1, 2, 3, 4, 5))], {"table_count": 1},
                    "found at 2x7 — not merged into the surrounding prose, not read "
                    "column-major; a 5x5 default never produces this shape")
        return build
    two("wide_shallow", dict(source=wb), {"ruled": wide("full_grid"), "borderless": wide("borderless")})

    # --- overdecorated (IETF: 7x4 of content under 174/120 rules + 42 fills) ------------
    def overdec(fills_heavy):
        def build(cid):
            grid = mkgrid(cid, 7, 4, numeric=(3,))
            t = flow(grid, "overdecorated", tid=cid, numeric=(3,))
            cmds = [("BOX", (0, 0), (-1, -1), 2.5, colors.black),
                    ("LINEBELOW", (0, 0), (-1, 0), 2.0, colors.black)]
            r = rng_for(cid)
            n = 21 if fills_heavy else 6
            for _ in range(n):
                rr, cc = r.randint(1, 6), r.randint(0, 3)
                cmds.append(("BACKGROUND", (cc, rr), (cc, rr), colors.Color(.90, .92, .86)))
            t.setStyle(TableStyle(cmds))
            g = build_doc(f"{cid}.pdf", [t])
            got = RAW.stroke_census(os.path.join(OUT, f"{cid}.pdf"))
            return ([tbl(g[0]["draws"], grid, style="overdecorated")], {"table_count": 1},
                    f"dims must be the CONTENT grid 7x4, not the decoration count "
                    f"(emitted h={got['h']} v={got['v']} fills={got['fills']}); ruling is not "
                    f"structure")
        return build
    two("overdecorated", dict(source=ietf),
        {"double_ruled": overdec(False), "fills_heavy": overdec(True)})

    # --- wrapped_cells ------------------------------------------------------------------
    def wrapped(style):
        # ENRICHED after the first parity run (§4.3): a comfortable 6x3 scored C_gen 1.0000
        # against C_real 0.4583 on World Bank p18 — a gap of +0.542, the worst in the corpus.
        # The parity page's own table is 2 rows x 13 columns, and the family is measured
        # WIDE AND SHALLOW (2x7, n=34). The carrier is therefore widened to 7 columns at the
        # measured pitch with the wrapped text crowded into three of them; the confound stays
        # exactly one: cell wrapping.
        def build(cid):
            r = rng_for(cid)
            grid = [["Instrument", "Policy commitment", "Safeguard", "Status", "2022",
                     "2023", "Reviewer"]]
            for i in range(5):
                grid.append([
                    f"Loan {i + 1}",
                    f"The borrower shall maintain the agreed macro-fiscal framework, publish "
                    f"the quarterly execution report, and transmit indicator {i + 1} to the "
                    f"supervising unit together with the audited statements of the "
                    f"implementing agency before the end of each reporting quarter.",
                    f"Environmental and social category {'ABC'[i % 3]}, with continuous "
                    f"third-party monitoring of the affected watershed and an annual "
                    f"resettlement audit covering every displaced household.",
                    "on track" if i % 2 == 0 else "delayed",
                    f"{r.randint(1, 99)}.{i}", f"{r.randint(1, 99)}.{i}",
                    f"{r.choice(WORDS)} unit"])
            g = build_doc(f"{cid}.pdf", [flow(grid, style, tid=cid, wrap_cols=(1, 2, 6),
                                              font=7,
                                              colw=[0.75 * inch, 1.7 * inch, 1.5 * inch,
                                                    0.65 * inch, 0.5 * inch, 0.5 * inch,
                                                    0.8 * inch])])
            return ([tbl(g[0]["draws"], grid, style=style)], {"table_count": 1},
                    "a wrapped cell is ONE logical string; wrap points are presentation "
                    "and compare space-joined (§7)")
        return build
    two("wrapped_cells", dict(source=wb),
        {"in_grid": wrapped("full_grid"), "in_borderless": wrapped("borderless")})

    # --- numeric_alignment (bench54 NVIDIA 10-K class; migrated L2 verbatim) ------------
    def numalign(decimal):
        def build(cid):
            amts = (["5", "1,234", "67", "890,123", "4", "56,789", "8", "12,000"] if not decimal
                    else ["5.5", "1,234.05", "67.125", "890,123.1", "4.0", "56,789.25",
                          "8.75", "12,000.5"])
            grid = [["Item", "Count", "Amount"]]
            for i, a in enumerate(amts):
                grid.append([f"Item{i + 1}", f"{(i + 1) * 3}", a])
            g = build_doc(f"{cid}.pdf", [flow(grid, "borderless", tid=cid, numeric=(1, 2),
                                              colw=[1.3 * inch, 0.9 * inch, 1.2 * inch])])
            return ([tbl(g[0]["draws"], grid, style="borderless", numeric=(1, 2))], {"table_count": 1},
                    "right-aligned values 1-7 digits wide scatter their LEFT edges row to "
                    "row; the column count must stay 3 and each value keep its own cell")
        return build
    two("numeric_alignment",
        dict(source=bench54("NVIDIA 10-K right-aligned amount columns; migrated verbatim from "
                            "gen_tables.py L2 (right_aligned_numeric)")),
        {"right_aligned": numalign(False), "decimal_aligned": numalign(True)},
        parity="unrunnable")

    # --- financial ----------------------------------------------------------------------
    def financial(eu):
        def build(cid):
            r = rng_for(cid)
            grid = [["Line item", "FY2024", "FY2023"]]
            for name in ("Revenue", "Cost of revenue", "Operating expenses",
                         "Impairment", "Net income"):
                vals = []
                for _ in range(2):
                    n = r.randint(1000, 99999) / 100.0
                    if eu:
                        s = f"{n:,.2f}".replace(",", "\x00").replace(".", ",").replace("\x00", ".")
                    else:
                        s = f"${n:,.2f}"
                    if name in ("Cost of revenue", "Impairment"):
                        s = f"({s})"
                    vals.append(s)
                grid.append([name, *vals])
            g = build_doc(f"{cid}.pdf", [flow(grid, "booktabs", tid=cid, numeric=(1, 2),
                                              colw=[2.2 * inch, 1.6 * inch, 1.6 * inch])])
            return ([tbl(g[0]["draws"], grid, style="booktabs", numeric=(1, 2))], {"table_count": 1},
                    "cell text compares LITERALLY: `1,234.56` != `1234.56`, and the "
                    "parenthesised negative and the currency mark are part of the value")
        return build
    two("financial",
        dict(source=bench54("NVIDIA 10-K financial statements: US `1,234.56` / `(1,234)` / `$` "
                            "and the EU `1.234,56` convention")),
        {"us_format": financial(False), "eu_format": financial(True)}, parity="unrunnable")

    # --- footnote_markers ---------------------------------------------------------------
    def footnotes(star):
        def build(cid):
            mark = (lambda i: "*" * (i + 1)) if star else (lambda i: "†‡§"[i])
            r = rng_for(cid)
            grid = [["Model", "Params", "BLEU", "Notes"]]
            for i in range(4):
                grid.append([f"{r.choice(WORDS)}-{i + 1}{mark(i % 3)}", f"{r.randint(1, 90)}M",
                             f"{r.randint(20, 40)}.{r.randint(0, 9)}", f"seed {i}"])
            g = build_doc(f"{cid}.pdf",
                          [flow(grid, "booktabs", tid=cid, numeric=(1, 2)), Spacer(1, 8),
                           Paragraph("* trained for 100 epochs.  ** ablation.  *** ours.", CELL)])
            return ([tbl(g[0]["draws"], grid, style="booktabs")], {"table_count": 1},
                    "the marker stays inside its own cell and creates no phantom column")
        return build
    two("footnote_markers", dict(source=arx),
        {"daggers": footnotes(False), "asterisks": footnotes(True)})

    # --- sparse_wide (bench54 moderna p153 / Transformer Table 3; migrated L1) ----------
    def sparse(declared, populated):
        def build(cid):
            grid = [["Key"] + [f"Q{c}" for c in range(1, declared)]]
            for i in range(9):
                row = [f"Row{i + 1}"] + [""] * (declared - 1)
                for k in range(populated):
                    row[((i * (k + 1) * 3) % (declared - 1)) + 1] = f"{(i + 1) * (7 + k)}"
                grid.append(row)
            for c in range(1, declared):          # every declared column keeps >= 1 value
                if all(not grid[i][c] for i in range(1, 10)):
                    grid[1 + (c % 9)][c] = f"{c * 11}"
            g = build_doc(f"{cid}.pdf", [flow(grid, "borderless", tid=cid, font=7,
                                              colw=[(6.6 * inch) / declared] * declared)])
            return ([tbl(g[0]["draws"], grid, style="borderless")], {"table_count": 1},
                    "header-named but sparsely populated columns must SURVIVE, and each "
                    "value stay under its own header")
        return build
    two("sparse_wide",
        dict(source=bench54("moderna p153: 21 declared columns, 3 populated; and the "
                            "Transformer Table 3 / 10-K class (migrated from gen_tables.py L1)")),
        {"ten_col": sparse(10, 2), "twentyone_col": sparse(21, 3)}, parity="unrunnable")

    # --- no_header ----------------------------------------------------------------------
    def noheader(style):
        # ENRICHED after the first parity run (§4.3): one clean 6x4 grid scored C_gen 0.9821
        # against C_real 0.5974 on IRS f1065 p6 — a gap of +0.385. That real page carries
        # FOUR tables (2x6, 22x4, 9x4, 9x4), which is what makes it hard: the difficulty of an
        # IRS continuation page is table MULTIPLICITY under uniform ruling, not any one grid.
        # The carrier now stacks three data-only blocks of different heights, as the source
        # does. The confound stays exactly one: no header row.
        def build(cid):
            shapes = ((9, 4), (5, 4), (12, 4))
            story = [Paragraph("(continued from the previous page)", CELL), Spacer(1, 6)]
            grids = []
            for k, (rows, cols) in enumerate(shapes):
                grid = mkgrid(f"{cid}{k}", rows, cols, header=False, numeric=(2, 3))
                grids.append(grid)
                story.append(flow(grid, style, tid=f"{cid}{k}", header_rows=0, numeric=(2, 3),
                                  font=8))
                story.append(Spacer(1, 16))
            g = build_doc(f"{cid}.pdf", story)
            return ([tbl(g[k]["draws"], grids[k], style=style, header_rows=0)
                     for k in range(len(shapes))],
                    {"table_count": len(shapes)},
                    "a continuation page carrying several data-only blocks: each is found "
                    "and `header_rows: 0` is respected on all of them")
        return build
    two("no_header", dict(source=irs),
        {"grid": noheader("full_grid"), "borderless": noheader("borderless")})

    # --- non_latin (invented — measured absent: the corpus has NO curated GT table page) -
    def nonlatin(which):
        def build(cid):
            if which == "nordic":
                grid = [["Område", "Prøve", "Dybde"],
                        ["Nordfjæra", "Skjærgård", "12,5"],
                        ["Sørøya", "Vågen", "18,0"],
                        ["ÆØÅ-feltet", "Måløy", "31,2"]]
            else:
                grid = [["Größe", "Prüfung", "Résumé"],
                        ["Straße", "gemäß", "déjà vu"],
                        ["Fluß", "Übung", "élève"],
                        ["Weiß", "Öffnung", "français"]]
            g = build_doc(f"{cid}.pdf", [flow(grid, "full_grid", tid=cid)])
            return ([tbl(g[0]["draws"], grid, style="full_grid")], {"table_count": 1},
                    "cell text must be byte-exact after NFC; WinAnsi + base-14 only (CJK / "
                    "Cyrillic need font embedding — spec §11)")
        return build
    two("non_latin",
        dict(invented="build step 0 found NO curated / non-English GT table page in the "
                      "88-doc corpus, so this has no measured analogue and is never claimed "
                      "as source coverage (spec §10.4.4)"),
        {"nordic": nonlatin("nordic"), "german_french": nonlatin("de_fr")})

    # --- glyph_per_tj (raw PDF: reportlab cannot emit one show-op per glyph) ------------
    def glyph_case(kerned):
        def build(cid):
            path = os.path.join(OUT, f"{cid}.pdf")
            grid = [["Region", "Samples", "Depth"],
                    ["Texas", "128", "42.5"],
                    ["South", "96", "31.0"],
                    ["East ridge", "77", "18.2"]]
            COLS, ADV, TOP, RH = (72.0, 220.0, 360.0), 5.4, 700.0, 16.0
            body = [RAW.text_op(72, 740, "A Table Drawn One Glyph At A Time", b"F2", 15)]
            for ri, row in enumerate(grid):
                y = TOP - ri * RH
                for ci, cell in enumerate(row):
                    x = COLS[ci]
                    if ri == 0:
                        body.append(RAW.text_op(x, y, cell, b"F3", 9))
                    elif kerned:
                        # word runs in one TJ array with kerns between glyphs — the same text,
                        # a different stream idiom
                        for word in cell.split(" "):
                            parts = b"".join(b"(%s) -12 " % RAW.esc(ch) for ch in word)
                            body.append(b"BT /F3 9 Tf %.2f %.2f Td [ %s ] TJ ET" % (x, y, parts))
                            x += (len(word) + 1) * ADV
                    else:
                        for i, ch in enumerate(cell):
                            if ch != " ":
                                body.append(RAW.text_op(x + i * ADV, y, ch, b"F3", 9))
            content = b"\n".join(body)
            fonts = b" ".join(b"/%s %d 0 R" % (n, 5 + i) for i, n in enumerate(RAW.FONTS))
            objs = {1: b"<< /Type /Catalog /Pages 2 0 R >>",
                    2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] >>",
                    3: (b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources "
                        b"<< /Font << %s >> >> >>" % fonts),
                    4: RAW.stream(content)}
            for i, ob in enumerate(RAW.FONTS.values()):
                objs[5 + i] = ob
            RAW.assemble_pdf(objs, path)
            bbox = [72 / PW, (PH - (TOP + 12)) / PH, 420 / PW, (PH - (TOP - 3 * RH - 3)) / PH]
            return ([{"page": 0, "bbox_norm": [round(v, 4) for v in bbox], "n_rows": 4,
                      "n_cols": 3, "style": "borderless", "header_rows": 1,
                      "cells": to_cells(grid)}], {"table_count": 1},
                    "no intra-word spaces: `Texas` must not come out of a cell as `T e x a s` "
                    "(266 of the earlier corpus's 632 detected tables were shredded this way)")
        return build
    two("glyph_per_tj",
        dict(source=bench54("SEC filings emit one show-operator per glyph; Courier at 9pt "
                            "makes every advance exactly 0.6em = 5.4, so a glyph boundary is "
                            "a gap of 0 and a real word space is a gap of 5.4")),
        {"one_tj_per_glyph": glyph_case(False), "kerned_tj_runs": glyph_case(True)},
        parity="unrunnable")

    # --- tagged_only_signal (passable ONLY via the declared path) -----------------------
    def tagonly(misaligned):
        def build(cid):
            r = rng_for(cid)
            grid = [["Indicator", "Baseline", "Target", "Actual"]]
            for i in range(5):
                grid.append([f"{r.choice(WORDS)} index", f"{r.randint(10, 99)}",
                             f"{r.randint(10, 99)}", f"{r.randint(10, 99)}"])
            gd = [[{"text": v} for v in row] for row in grid]
            # `misaligned`: a per-row x nudge, so the visual columns are plausible but WRONG
            # while the declaration stays right — inference reading geometry alone lands on a
            # different grid than the file declares.
            rec = RAW.tagged_table(
                os.path.join(OUT, f"{cid}.pdf"), gd, style="borderless", header_rows=1,
                col_w=110.0, row_h=20.0, title="Table 2. Results", declare=True,
                x_jitter=(0.0, 14.0, -11.0, 8.0, -16.0, 5.0) if misaligned else ())
            rec["page"] = 0
            return ([rec], {"table_count": 1},
                    "zero ruling lines: the declaration is the ONLY structure signal, so "
                    "this is passable via the declared path and not otherwise")
        return build
    for variant, mis in (("no_rules", False), ("misaligned", True)):
        cid = f"t2_tagged_only_signal_{variant}"
        tables, expect, note = tagonly(mis)(cid)
        emit(f"{cid}.pdf", tier=2, family="tagged_only_signal", variant=variant, tagged=True,
             tables=tables, expect=expect, note=note, source=tagsrc)


# =====================================================================================
# T3 — Compound & adversarial: observed failure modes, often several confounds at once.
# Gate: the RED LEDGER — each case carries expected pass|known_fail in floors.json, and a
# known_fail runs as a strict xfail so a landed fix fails the run in the good direction.
# =====================================================================================

def t3_adversarial():
    # --- t3_blank_borderless -------------------------------------------------------------
    cid = "t3_blank_borderless"
    grid = mkgrid(cid, 8, 5, numeric=(4,), blank_frac=0.20)
    for c in range(5):
        if all(not grid[r][c] for r in range(1, 8)):
            grid[1][c] = f"{WORDS[c]}1"
    g = build_doc(f"{cid}.pdf", [flow(grid, "borderless", tid=cid, numeric=(4,))])
    emit(f"{cid}.pdf", tier=3, family="t3_blank_borderless", variant="only", tagged=False,
         tables=[tbl(g[0]["draws"], grid, style="borderless")], expect={"table_count": 1},
         invented="compound of the two weakest signals — no rules AND 20% blanks",
         note="dims exact and the blanks at the right (r, c)")

    # --- t3_spacer_cols (SEC shape: 21 declared columns, 3 populated) -------------------
    cid = "t3_spacer_cols"
    r = rng_for(cid)
    grid = [["Line item"] + sum(([f"FY{2020 + i}", ""] for i in range(10)), [])]
    for name in ("Revenue", "Cost of revenue", "Gross profit", "Operating income",
                 "Net income"):
        row = [name] + [""] * 20
        for k, i in enumerate((1, 9, 17)):
            row[i] = f"${r.randint(100, 9999):,}"
        grid.append(row)
    g = build_doc(f"{cid}.pdf", [flow(grid, "borderless", tid=cid, font=6,
                                      colw=[1.5 * inch] + [0.26 * inch] * 20)])
    emit(f"{cid}.pdf", tier=3, family="t3_spacer_cols", variant="only", tagged=False,
         tables=[tbl(g[0]["draws"], grid, style="borderless")],
         expect={"table_count": 1, "cols_any": [3, 21]},
         source=bench54("moderna p153 — 21 declared columns of which 3 are populated, the "
                        "rest layout spacers. SEC shape reconstructed from the earlier "
                        "corpus; NOT sampled in bench88, which contains no SEC filings"),
         parity="unrunnable",
         note="values must sit under the correct headers whichever column count is read")

    # --- t3_boundary_in_content ---------------------------------------------------------
    # A column gap that falls INSIDE natural text spacing. Canvas-positioned so the x's are
    # exact. Geometry only: the pdfTeX stream idiom that produced the original is out of
    # reach (spec §10.4.1), and external parity is unrunnable, so this replica is a
    # regression lock, not a substitute for doc 002.
    cid = "t3_boundary_in_content"
    c = rl_canvas.Canvas(os.path.join(OUT, f"{cid}.pdf"), pagesize=(PW, PH))
    c.setFont("Helvetica-Bold", 12)
    c.drawString(72, 730, "Table 3: Cross-section properties")
    rows = [["A [m2]", "80", "K value"],
            ["0.0125", "80", "1.44"], ["0.0250", "80", "2.88"], ["0.0500", "80", "5.76"]]
    XS = (72.0, 132.0, 168.0)
    y = 700
    c.setLineWidth(0.7)
    c.line(72, 714, 300, 714)
    for ri, row in enumerate(rows):
        c.setFont("Helvetica-Bold" if ri == 0 else "Helvetica", 9)
        for ci, v in enumerate(row):
            c.drawString(XS[ci], y, v)
        if ri == 0:
            c.line(72, y - 4, 300, y - 4)
        y -= 15
    c.line(72, y + 11, 300, y + 11)
    c.save()
    emit(f"{cid}.pdf", tier=3, family="t3_boundary_in_content", variant="only", tagged=False,
         tables=[{"page": 0, "bbox_norm": [round(72 / PW, 4), round((PH - 716) / PH, 4),
                                           round(300 / PW, 4), round((PH - 640) / PH, 4)],
                  "n_rows": 4, "n_cols": 3, "style": "booktabs", "header_rows": 1,
                  "cells": to_cells(rows)}],
         expect={"table_count": 1},
         source=external("doc 002 (pdfTeX calibration corpus)",
                         "a column boundary that falls inside natural text spacing, so "
                         "`A [m2]` / `80` / `K value` merge into one cell"),
         parity="unrunnable",
         note="GEOMETRY replicated, pdfTeX's kerned-TJ stream idiom NOT (spec §10.4.1): if "
              "the defect is idiom-triggered this replica can pass while doc 002 fails, and "
              "external parity cannot detect that")

    # --- t3_interleaved -----------------------------------------------------------------
    cid = "t3_interleaved"
    c = rl_canvas.Canvas(os.path.join(OUT, f"{cid}.pdf"), pagesize=(PW, PH))
    c.setFont("Helvetica-Bold", 12)
    c.drawString(72, 730, "Two independent tables at different x-bands")
    r = rng_for(cid)
    left = [["Site", "Depth"]] + [[f"S{i + 1}", f"{r.randint(10, 99)}"] for i in range(5)]
    right = [["Model", "Score"]] + [[f"M{i + 1}", f"{r.randint(1, 9)}.{i}"] for i in range(5)]
    LX, RX, TOP, RH = (72.0, 150.0), (350.0, 440.0), 690.0, 18.0
    for i in range(6):
        y = TOP - i * RH
        c.setFont("Helvetica-Bold" if i == 0 else "Helvetica", 9)
        c.drawString(LX[0], y, left[i][0]); c.drawString(LX[1], y, left[i][1])
        c.drawString(RX[0], y, right[i][0]); c.drawString(RX[1], y, right[i][1])
    c.save()
    lb = [72 / PW, (PH - TOP - 10) / PH, 200 / PW, (PH - (TOP - 5 * RH) + 4) / PH]
    rb = [350 / PW, (PH - TOP - 10) / PH, 490 / PW, (PH - (TOP - 5 * RH) + 4) / PH]
    emit(f"{cid}.pdf", tier=3, family="t3_interleaved", variant="only", tagged=False,
         tables=[{"page": 0, "bbox_norm": [round(v, 4) for v in lb], "n_rows": 6, "n_cols": 2,
                  "style": "borderless", "header_rows": 1, "cells": to_cells(left)},
                 {"page": 0, "bbox_norm": [round(v, 4) for v in rb], "n_rows": 6, "n_cols": 2,
                  "style": "borderless", "header_rows": 1, "cells": to_cells(right)}],
         expect={"table_count": 2, "must_not_merge": True},
         source=external("doc 006 (pdfTeX calibration corpus)",
                         "two unrelated tables interleaved row by row at different x-bands"),
         parity="unrunnable", note="each table keeps its own cells; they must not fuse")

    # --- t3_adjacent_no_fuse ------------------------------------------------------------
    cid = "t3_adjacent_no_fuse"
    r = rng_for(cid)
    g1 = [["Site", "Depth", "Grade"]] + [[f"S{i + 1}", f"{r.randint(10, 99)}",
                                          f"{r.randint(1, 9)}.{i}"] for i in range(4)]
    g2 = [["Model", "Params", "Score"]] + [[f"M{i + 1}", f"{r.randint(1, 90)}M",
                                            f"{r.randint(20, 40)}.{i}"] for i in range(4)]
    colw = [1.6 * inch] * 3
    t1 = flow(g1, "booktabs", tid=cid + "_a", colw=colw)
    t2 = flow(g2, "booktabs", tid=cid + "_b", colw=colw)
    grp = build_doc(f"{cid}.pdf", [t1, Spacer(1, 18), t2])
    emit(f"{cid}.pdf", tier=3, family="t3_adjacent_no_fuse", variant="only", tagged=False,
         tables=[tbl(grp[0]["draws"], g1, style="booktabs"),
                 tbl(grp[1]["draws"], g2, style="booktabs")],
         expect={"table_count": 2},
         source=external("doc 000 (pdfTeX calibration corpus)",
                         "two tables sharing one column grid, 18pt apart, distinct headers"),
         parity="unrunnable", note="an 18pt gap and different headers is TWO tables")

    # --- t3_page_break / t3_page_break_repeat / t3_long_multipage -----------------------
    for cid, nrows, ncols, repeat in (("t3_page_break", 40, 4, 0),
                                      ("t3_page_break_repeat", 40, 4, 1),
                                      ("t3_long_multipage", 60, 6, 0)):
        grid = mkgrid(cid, nrows, ncols, numeric=(ncols - 1,))
        t = flow(grid, "full_grid", tid=cid, colw=[(6.4 * inch) / ncols] * ncols, font=8)
        t.repeatRows = repeat
        grp = build_doc(f"{cid}.pdf", [Paragraph(PROSE, BODY), Spacer(1, 10), t])
        d0 = grp[0]["draws"][0]
        npages = max(d["page"] for d in grp[0]["draws"]) + 1
        emit(f"{cid}.pdf", tier=3, family=cid, variant="only", tagged=False, pages=npages,
             tables=[{"page": d0["page"], "bbox_norm": _norm_bbox(d0), "n_rows": nrows,
                      "n_cols": ncols, "style": "full_grid", "header_rows": 1,
                      "cells": to_cells(grid)}],
             expect={"table_count_any": [1, npages]},
             invented=("a long table forced across a page break with the header NOT repeated"
                       if not repeat else
                       "the same table with `repeatRows=1`, so the header is redrawn on "
                       "every page and must not become a body row"),
             note=f"{npages} pages; cells in row order, no dropped boundary rows. Placement "
                  f"is gated, the stitching POLICY is not (`table_count_any`).")

    # --- Phase 7A cross-page preregistration -------------------------------------------
    # These four cases freeze the continuation proof boundary before any product change:
    # three accepted-parent negatives and one deliberately red genuine continuation.  All
    # are two-page, four-column controlled pairs so a future stitcher cannot hide behind a
    # page-count, shape, or column-count distinction.

    cid = "t3_crosspage_independent_geometry"
    first = [["Site", "Region", "Depth", "Grade"],
             ["Aster", "North", "12.4", "2.1"],
             ["Birch", "North", "18.0", "1.7"],
             ["Cedar", "West", "24.8", "3.0"],
             ["Dune", "South", "31.6", "2.6"],
             ["Elm", "East", "37.3", "1.9"]]
    second = [["Model", "Family", "Params", "Score"],
              ["Atlas", "Linear", "12M", "31.4"],
              ["Beacon", "Tree", "48M", "35.2"],
              ["Cobalt", "Kernel", "27M", "33.8"],
              ["Drift", "Linear", "19M", "32.6"],
              ["Ember", "Tree", "61M", "36.1"]]
    first_widths = [1.10 * inch, 1.10 * inch, 0.85 * inch, 0.85 * inch]
    second_widths = [0.80 * inch, 1.35 * inch, 0.70 * inch, 1.25 * inch]
    pieces = build_doc(
        f"{cid}.pdf",
        [Spacer(1, 430),
         Paragraph("Table A. Northern field observations", CELL), Spacer(1, 6),
         flow(first, "full_grid", tid=cid + "_a", colw=first_widths),
         PageBreak(),
         Paragraph("Table B. Independent model evaluation", CELL), Spacer(1, 6),
         flow(second, "full_grid", tid=cid + "_b", colw=second_widths)],
    )
    emit(f"{cid}.pdf", tier=3, family=cid, variant="only", tagged=False, pages=2,
         tables=[tbl(pieces[0]["draws"], first, style="full_grid"),
                 tbl(pieces[1]["draws"], second, style="full_grid")],
         expect={"table_count": 2, "must_not_merge": True},
         invented="Phase 7 preregistered negative: independent tables on adjacent pages "
                  "have the same four-column count but different x-edges and ownership",
         note="accepted parent contract: exactly two tables; cross-page shape similarity "
              "alone never licenses a merge")

    cid = "t3_crosspage_independent_caption"
    headers = ["Item", "Region", "Value", "Status"]
    first = [headers,
             ["Permit A", "North", "14", "open"],
             ["Permit B", "South", "19", "closed"],
             ["Permit C", "East", "23", "open"],
             ["Permit D", "West", "28", "review"],
             ["Permit E", "North", "34", "closed"]]
    second = [headers,
              ["Audit D", "North", "71", "passed"],
              ["Audit E", "South", "64", "passed"],
              ["Audit F", "West", "58", "review"],
              ["Audit G", "East", "76", "passed"],
              ["Audit H", "North", "69", "review"]]
    widths = [1.25 * inch, 1.15 * inch, 0.85 * inch, 1.15 * inch]
    pieces = build_doc(
        f"{cid}.pdf",
        [Spacer(1, 430),
         flow(first, "full_grid", tid=cid + "_a", colw=widths), Spacer(1, 6),
         Paragraph("Table 7. Permit register", CELL),
         PageBreak(),
         Paragraph("Table 8. Audit outcomes", CELL), Spacer(1, 6),
         flow(second, "full_grid", tid=cid + "_b", colw=widths)],
    )
    emit(f"{cid}.pdf", tier=3, family=cid, variant="only", tagged=False, pages=2,
         tables=[tbl(pieces[0]["draws"], first, style="full_grid"),
                 tbl(pieces[1]["draws"], second, style="full_grid")],
         expect={"table_count": 2, "must_not_merge": True},
         invented="Phase 7 preregistered negative: adjacent-page tables deliberately share "
                  "literal headers and exact x-edges but have distinct captions/ownership",
         note="accepted parent contract: both tables retain their identical header row and "
              "their caption-distinguished contents remain separate")

    cid = "t3_crosspage_aligned_prose"
    grid = [["Station", "Region", "Depth", "Status"],
            ["Delta-1", "North", "11.2", "active"],
            ["Delta-2", "South", "16.8", "active"],
            ["Delta-3", "West", "22.5", "closed"],
            ["Delta-4", "East", "27.9", "active"],
            ["Delta-5", "North", "33.1", "closed"]]
    x0, x1, top, row_h = 72.0, 540.0, 210.0, 20.0
    col_w = (x1 - x0) / 4
    c = rl_canvas.Canvas(os.path.join(OUT, f"{cid}.pdf"), pagesize=(PW, PH))
    c.setFont("Helvetica", 8)
    c.drawString(x0, top + 18, "Table 11. Stations at the end of the reporting page")
    c.setLineWidth(1.0)
    c.line(x0, top, x1, top)
    c.setLineWidth(0.5)
    c.line(x0, top - row_h, x1, top - row_h)
    c.setLineWidth(1.0)
    c.line(x0, top - len(grid) * row_h, x1, top - len(grid) * row_h)
    for ri, row in enumerate(grid):
        c.setFont("Helvetica-Bold" if ri == 0 else "Helvetica", 8.5)
        y = top - (ri + 1) * row_h + 6
        for ci, value in enumerate(row):
            c.drawString(x0 + ci * col_w + 4, y, value)
    c.showPage()
    c.setFont("Helvetica", 9)
    aligned = ["The field team", "continued its", "regional survey", "after dawn."]
    for ci, value in enumerate(aligned):
        c.drawString(x0 + ci * col_w + 4, 724, value)
    prose = [
        "These four aligned fragments form one sentence, not a continuation row.",
        "The narrative then resumes across the full text width on the second page.",
        "No word on this page belongs to the station table on the previous page.",
    ]
    for i, line in enumerate(prose):
        c.drawString(x0, 694 - i * 18, line)
    c.save()
    bbox = [round(x0 / PW, 4), round((PH - top) / PH, 4), round(x1 / PW, 4),
            round((PH - (top - len(grid) * row_h)) / PH, 4)]
    emit(f"{cid}.pdf", tier=3, family=cid, variant="only", tagged=False, pages=2,
         tables=[{"page": 0, "bbox_norm": bbox, "n_rows": len(grid), "n_cols": 4,
                  "style": "booktabs", "header_rows": 1, "cells": to_cells(grid),
                  "col_edges_norm": [round((x0 + i * col_w) / PW, 4) for i in range(5)],
                  "row_edges_norm": [round((PH - top + i * row_h) / PH, 4)
                                     for i in range(len(grid) + 1)]}],
         expect={"table_count": 1},
         invented="Phase 7 preregistered negative: a bottom-page table is followed by a "
                  "top-page sentence whose four fragments align with the table columns",
         note="accepted parent contract: one table only; exact page-2 prose survives once "
              "and creates neither a continuation row nor a phantom table")

    cid = "t3_crosspage_header_text_data"
    headers = ["Metric", "Region", "Value", "Status"]
    first = [headers,
             ["Yield", "North", "41.2", "final"],
             ["Moisture", "South", "8.7", "final"],
             ["Density", "East", "2.4", "draft"],
             ["Purity", "West", "97.1", "final"]]
    # Row zero on page 2 is intentionally text-equal to the header, but it is ordinary body
    # data: no bold/fill and no header separator.  A stitcher may suppress only independently
    # evidenced repeated headers, never strings that merely compare equal.
    second = [headers,
              ["Recovery", "North", "88.0", "final"],
              ["Loss", "South", "4.1", "draft"],
              ["Variance", "East", "1.8", "final"],
              ["Output", "West", "73.6", "final"]]
    widths = [1.25 * inch, 1.15 * inch, 0.85 * inch, 1.15 * inch]
    pieces = build_doc(
        f"{cid}.pdf",
        [Spacer(1, 410),
         Paragraph("Table 14. Production measures (continued on next page)", CELL),
         Spacer(1, 6),
         flow(first, "full_grid", tid=cid + "_a", colw=widths),
         PageBreak(),
         flow(second, "full_grid", tid=cid + "_b", colw=widths, header_rows=0)],
    )
    logical = first + second
    d0 = pieces[0]["draws"][0]
    emit(f"{cid}.pdf", tier=3, family=cid, variant="only", tagged=False, pages=2,
         tables=[{"page": d0["page"], "bbox_norm": _norm_bbox(d0),
                  "n_rows": len(logical), "n_cols": 4, "style": "full_grid",
                  "header_rows": 1, "cells": to_cells(logical, header_rows=1)}],
         expect={"table_count": 1},
         invented="Phase 7 preregistered positive: one genuine adjacent-page continuation "
                  "whose first page-2 data row is text-equal to the four headers",
         note="KNOWN FAIL until continuation lands: merge to one 10x4 table, retain the "
              "text-equal page-2 row as data, and suppress no row without independent "
              "header styling/separator ownership")

    # --- t3_tagged_degenerate -----------------------------------------------------------
    cid = "t3_tagged_degenerate"
    grid = mkgrid(cid, 6, 4, numeric=(3,))
    gd = [[{"text": v} for v in row] for row in grid]
    rec = RAW.tagged_table(os.path.join(OUT, f"{cid}.pdf"), gd, style="full_grid",
                           header_rows=1, col_w=96.0, row_h=18.0,
                           title="A grid the file mis-declares", struct_override="degenerate")
    rec["page"] = 0
    emit(f"{cid}.pdf", tier=3, family="t3_tagged_degenerate", variant="only", tagged=True,
         tables=[rec], expect={"table_count": 1},
         source=src("type.tagged", "a /StructTreeRoot that declares three single-row shards "
                    "over a real 6x4 ruled grid — the degenerate declaration shape "
                    "gen_tagged_table already pins as REFUSED"),
         note="the declaration is rejected and GEOMETRY wins: 6x4")

    # --- t3_rotate90 --------------------------------------------------------------------
    cid = "t3_rotate90"
    grid = mkgrid(cid, 7, 5, numeric=(4,))
    gd = [[{"text": v} for v in row] for row in grid]
    rec = RAW.tagged_table(os.path.join(OUT, f"{cid}.pdf"), gd, style="full_grid",
                           header_rows=1, col_w=90.0, row_h=18.0, x0=100.0, y_top=600.0,
                           title="A landscape table on a turned page", declare=False,
                           rotate=90)
    rec["page"] = 0
    emit(f"{cid}.pdf", tier=3, family="t3_rotate90", variant="only", tagged=False,
         tables=[rec], expect={"table_count": 1},
         source=bench25("med_crispr", "19-24",
                        "a /Rotate 90 page carrying a ruled landscape table"),
         parity="unrunnable",
         note="`bbox_norm` is stated in POST-rotation display space (spec §7). reportlab "
              "cannot set /Rotate per page, so this is hand-assembled.")

    # --- t3_separation_header -----------------------------------------------------------
    # A header row filled in a Separation space through a Type-2 tint transform — the defect
    # that painted a pale (198,198,224) header near-BLACK. reportlab emits no spot colours.
    cid = "t3_separation_header"
    r = rng_for(cid)
    grid = [["Constituent", "Benchmark", "Value", "Units"]]
    for name, bench in (("Arsenic", "Federal MCL"), ("Boron", "Federal HAL"),
                        ("Radon-222", "Proposed MCL"), ("Nitrate", "State MCL"),
                        ("Selenium", "Federal MCL")):
        grid.append([name, bench, f"{r.randint(1, 9000):,}", r.choice(["ppb", "pCi"])])
    X0, YT, CW, RH = 72.0, 640.0, 116.0, 20.0
    n_rows, n_cols = len(grid), 4
    ty1 = YT + RH
    body = [RAW.text_op(X0, YT + 40, "Groundwater quality benchmarks", b"F2", 13),
            b"/CS0 cs", b"1 scn %.2f %.2f %.2f %.2f re f"
            % (X0, ty1 - RH, n_cols * CW, RH)]
    for ri, row in enumerate(grid):
        y = ty1 - (ri + 1) * RH + 6
        for ci, v in enumerate(row):
            body.append(RAW.text_op(X0 + ci * CW + 4, y, v, b"F2" if ri == 0 else b"F1", 9))
    for i in range(n_rows + 1):
        body.append(RAW.hline(X0, X0 + n_cols * CW, ty1 - i * RH))
    for j in range(n_cols + 1):
        body.append(RAW.vline(X0 + j * CW, ty1 - n_rows * RH, ty1))
    content = b"\n".join(body)
    fonts = b" ".join(b"/%s %d 0 R" % (n, 5 + i) for i, n in enumerate(RAW.FONTS))
    objs = {1: b"<< /Type /Catalog /Pages 2 0 R >>",
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] >>",
            3: (b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << %s >> "
                b"/ColorSpace << /CS0 [/Separation /Spot /DeviceRGB 10 0 R] >> >> >>" % fonts),
            4: RAW.stream(content),
            10: b"<< /FunctionType 2 /Domain [0 1] /C0 [1 1 1] /C1 [0.776 0.776 0.878] /N 1 >>"}
    for i, ob in enumerate(RAW.FONTS.values()):
        objs[5 + i] = ob
    RAW.assemble_pdf(objs, os.path.join(OUT, f"{cid}.pdf"))
    bbox = [X0 / PW, (PH - ty1) / PH, (X0 + n_cols * CW) / PW, (PH - (ty1 - n_rows * RH)) / PH]
    emit(f"{cid}.pdf", tier=3, family="t3_separation_header", variant="only", tagged=False,
         tables=[{"page": 0, "bbox_norm": [round(v, 4) for v in bbox], "n_rows": n_rows,
                  "n_cols": n_cols, "style": "full_grid", "header_rows": 1,
                  "cells": to_cells(grid)}],
         expect={"table_count": 1},
         invented="the audited near-black header defect: tint 1 in a Separation space whose "
                  "Type-2 transform ends at (198,198,224). Read as a grey level it painted "
                  "#1a1a1a over the header text",
         note="the header TEXT must be extracted and the table stay intact")

    # --- t3_panel_duplicate --------------------------------------------------------------
    cid = "t3_panel_duplicate"
    ROWS = [["Constituent", "Benchmark", "Value"],
            ["Arsenic", "Federal MCL", "10 ppb"],
            ["Boron", "Federal HAL", "6,000 ppb"],
            ["Radon-222", "Proposed MCL", "4,000 pCi"]]
    COLS, TOP, RH = (96.0, 260.0, 410.0), 572.0, 24.0
    panel = [b"q 0.93 0.95 0.90 rg 72 400 468 240 re f Q",
             b"q 0 0 0 RG 1.2 w 72 400 468 240 re S Q"]
    panel += [RAW.hline(88, 500, y) for y in (566, 494)]
    # curve marks keep the cluster a FIGURE (has_graphic_ink) rather than a ruled table
    panel += [b"q 0.20 0.45 0.70 rg 470 %d m 500 %d 500 %d 470 %d c f Q"
              % (y, y + 18, y + 42, y + 30) for y in (420, 450)]
    panel.append(b"q 0 0 0 RG 0.8 w 470 420 m 512 462 l S Q")
    body = [RAW.text_op(72, 730, "A Callout Panel Around A Real Table", b"F2", 17),
            RAW.text_op(72, 700, "The panel below is a figure; the grid inside it is a table.",
                        b"F1", 10)]
    body += panel
    body.append(RAW.text_op(88, 594, "Benchmarks for evaluating groundwater quality", b"F2", 11))
    for ri, row in enumerate(ROWS):
        y = TOP - ri * RH
        for ci, cell in enumerate(row):
            body.append(RAW.text_op(COLS[ci], y, cell, b"F1", 9))
    content = b"\n".join(body)
    fonts = b" ".join(b"/%s %d 0 R" % (n, 5 + i) for i, n in enumerate(RAW.FONTS))
    objs = {1: b"<< /Type /Catalog /Pages 2 0 R >>",
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 612 792] >>",
            3: (b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << %s >> >> >>"
                % fonts),
            4: RAW.stream(content)}
    for i, ob in enumerate(RAW.FONTS.values()):
        objs[5 + i] = ob
    RAW.assemble_pdf(objs, os.path.join(OUT, f"{cid}.pdf"))
    bbox = [88 / PW, (PH - 584) / PH, 480 / PW, (PH - (TOP - 3 * RH - 4)) / PH]
    emit(f"{cid}.pdf", tier=3, family="t3_panel_duplicate", variant="only", tagged=False,
         tables=[{"page": 0, "bbox_norm": [round(v, 4) for v in bbox], "n_rows": 4,
                  "n_cols": 3, "style": "booktabs", "header_rows": 1, "cells": to_cells(ROWS)}],
         expect={"table_count": 1, "exactly_once": True},
         source=bench25("geology_usgs_fs", 3,
                        "a callout panel (a figure) with a real ruled table inside it; the "
                        "SVG copy kept `Perchlorate` and lost `Radon-222`"),
         parity="unrunnable",
         note="every cell appears EXACTLY ONCE across the whole emitted document")

    # --- t3_zebra_blanks -----------------------------------------------------------------
    cid = "t3_zebra_blanks"
    grid = mkgrid(cid, 9, 5, numeric=(4,), blank_frac=0.15)
    for c in range(5):
        if all(not grid[r][c] for r in range(1, 9)):
            grid[1][c] = f"{WORDS[c]}1"
    g = build_doc(f"{cid}.pdf", [flow(grid, "zebra", tid=cid, numeric=(4,))])
    emit(f"{cid}.pdf", tier=3, family="t3_zebra_blanks", variant="only", tagged=False,
         tables=[tbl(g[0]["draws"], grid, style="zebra")], expect={"table_count": 1},
         invented="zebra shading with no rules and 15% blanks — the row fills are the ONLY "
                  "row signal, and blanks punch holes in the text evidence",
         note="dims and blank positions correct")

    # --- t3_kitchen_sink ------------------------------------------------------------------
    cid = "t3_kitchen_sink"
    r = rng_for(cid)
    grid = [["", "Phase 1", "", "Phase 2", "", "Notes"],
            ["Unit", "Min", "Max", "Min", "Max", "Comment"]]
    bands = []
    for name in ("Upper sequence", "Lower sequence"):
        bands.append(len(grid))
        grid.append([name, "", "", "", "", ""])
        for i in range(3):
            grid.append([f"{r.choice(WORDS)}{i + 1}*",
                         f"${r.randint(100, 9999):,}.{r.randint(10, 99)}",
                         f"({r.randint(10, 999):,})",
                         f"{r.randint(1, 99)}" if i != 1 else "",
                         f"{r.randint(1, 99)}.{i}",
                         "The supervising unit reviews this line every quarter."])
    spans = [(0, 1, 1, 2), (0, 3, 1, 2)] + [(b, 0, 1, 6) for b in bands]
    t = flow(grid, "full_grid", tid=cid, header_rows=2, spans=spans, wrap_cols=(5,), font=7.5,
             colw=[1.2 * inch, 0.85 * inch, 0.85 * inch, 0.7 * inch, 0.7 * inch, 2.1 * inch])
    t.setStyle(TableStyle([("FONTNAME", (0, b), (-1, b), "Helvetica-Bold") for b in bands]))
    g = build_doc(f"{cid}.pdf", [t])
    emit(f"{cid}.pdf", tier=3, family="t3_kitchen_sink", variant="only", tagged=False,
         tables=[tbl(g[0]["draws"], grid, style="full_grid", header_rows=2, spans=spans)],
         expect={"table_count": 1},
         invented="composite of six confounds at once: grid + 2-tier merged header + band "
                  "rows + wrapped cells + footnote markers + financial numerics",
         note="WATCH-ONLY CANARY — floored at measured and NEVER tuned against (spec §10.1.4)")


# ================================================== §4.3 the anti-oversimplification gate

PARITY_BAND = 0.15
PARITY_REPORT = os.path.join(HERE, "..", "dev-docs", "bench", "results",
                             "table_corpus_parity.md")


def _bench100():
    """Import bench100's scorer and use ITS table-scoring function.

    Predicted sticking point §9(d), taken seriously: `C_gen` and `C_real` are only comparable
    if they come from the same code path. `C_real` is the committed `dpacc_tables` in
    `bench100_accuracy.json`; `C_gen` is `bench100_score.table_score(...)["score"]` over our
    emitted HTML. Reimplementing the formula here would produce two numbers that merely look
    alike."""
    root = os.path.abspath(os.path.join(HERE, ".."))
    sys.path.insert(0, os.path.join(root, "dev-docs", "bench", "scripts"))
    import bench100_score
    acc_path = os.path.join(root, "dev-docs", "bench", "results", "bench100_accuracy.json")
    return bench100_score, json.load(open(acc_path))["pages"]


def adjudicate_parity(cases):
    """Apply the §4.3 threshold to every generated file, never to a sibling average.

    ``cases`` contains already-scored rows. Keeping this decision pure makes the trust
    property regression-testable without loading PDFs or the licence-encumbered corpus.
    A disposition is accepted only when that exact file carries a non-empty recorded reason.
    """
    rows, over, accepted = [], [], []
    for case in cases:
        row = dict(case)
        if row.get("invented"):
            verdict, gap = "no analogue", None
        elif row.get("C_real") is None:
            verdict, gap = "parity unrunnable", None
        else:
            gap = row["C_gen"] - row["C_real"]
            if gap <= PARITY_BAND:
                verdict = "ok"
            elif row.get("accepted_reason"):
                verdict = "OVERSIMPLIFIED (accepted, excluded from coverage)"
            else:
                verdict = "OVERSIMPLIFIED"
        row["gap"] = round(gap, 4) if gap is not None else None
        row["C_gen"] = round(row["C_gen"], 4)
        if row.get("C_real") is not None:
            row["C_real"] = round(row["C_real"], 4)
        row["verdict"] = verdict
        rows.append(row)
        if verdict.startswith("OVERSIMPLIFIED (accepted"):
            accepted.append(row)
        elif verdict == "OVERSIMPLIFIED":
            over.append(row)
    return rows, over, accepted


def _family_parity_summary(rows):
    """Diagnostic family roll-up; verdicts remain the union of per-file decisions."""
    groups = {}
    for row in rows:
        g = groups.setdefault((row["family"], row["source"]), [])
        g.append(row)
    out = []
    for (family, source), files in sorted(groups.items()):
        checkable = [r for r in files if r["C_real"] is not None]
        verdicts = {r["verdict"] for r in files}
        if "OVERSIMPLIFIED" in verdicts:
            verdict = "OVERSIMPLIFIED — unexplained"
        elif any(v.startswith("OVERSIMPLIFIED (accepted") for v in verdicts):
            verdict = "partial — accepted exclusion(s)"
        elif checkable:
            verdict = "ok"
        elif verdicts == {"no analogue"}:
            verdict = "no analogue"
        else:
            verdict = "parity unrunnable"
        gaps = [r["gap"] for r in checkable]
        out.append({
            "family": family, "source": source, "n_files": len(files),
            "mean_C_gen": round(sum(r["C_gen"] for r in files) / len(files), 4),
            "C_real": checkable[0]["C_real"] if checkable else None,
            "max_gap": max(gaps) if gaps else None,
            "verdict": verdict,
        })
    return out


def parity(outdir=OUT):
    """Score every generated PDF and its named real page through the same scorer.

    Oversimplification is adjudicated **per generated file**: ``C_gen - C_real > 0.15``.
    Family means are reported only as diagnostics and can never alter a file's verdict.
    """
    import distillpdf

    b100, acc = _bench100()
    C_real_by_page = {f'{p["slug"]}#{p["page"]}': p["dpacc_tables"] for p in acc}
    truth = json.load(open(os.path.join(outdir, "truth.json")))["files"]

    cases = []
    for fname, rec in sorted(truth.items()):
        html = distillpdf.Pdf.open(os.path.join(outdir, fname)).to_html(
            return_string=True, image_mode="drop")
        got = b100.html_grids(b100._body(html))
        want = [{"rows": t["n_rows"], "cols": t["n_cols"],
                 "has_header_row": bool(t.get("header_rows"))} for t in rec["tables"]]
        c_gen = b100.table_score(got, want)["score"]

        s = rec.get("source")
        if rec.get("invented"):
            source, c_real = "(invented — measured absent)", None
        elif rec.get("parity") == "unrunnable" or not s or s.get("corpus") != "bench88":
            corp = (s or {}).get("corpus", "?")
            doc = (s or {}).get("doc", "?")
            label = corp if doc == corp else f"{corp}: {doc}"
            source = label + (f" p{s['page']}" if s and s.get("page") else "")
            c_real = None
        else:
            page_key = f'{s["doc"]}#{s["page"]}'
            c_real = C_real_by_page.get(page_key)
            source = f"{s['doc']} p{s['page']}"
        cases.append({
            "file": fname, "family": rec["family"], "source": source,
            "C_gen": c_gen, "C_real": c_real, "tier": rec["tier"],
            "invented": bool(rec.get("invented")),
            "accepted_reason": (rec.get("oversimplified_why")
                                if rec.get("oversimplified") else None),
        })

    rows, over, accepted = adjudicate_parity(cases)
    _write_parity_report(rows, _family_parity_summary(rows), over, accepted)
    return rows, over


def _write_parity_report(rows, family_rows, over, accepted):
    os.makedirs(os.path.dirname(PARITY_REPORT), exist_ok=True)
    n_checked = sum(1 for r in rows if r["C_real"] is not None)
    n_unrun = sum(1 for r in rows if r["verdict"] == "parity unrunnable")
    n_inv = sum(1 for r in rows if r["verdict"] == "no analogue")
    with open(PARITY_REPORT, "w") as f:
        f.write(f"""# Table torture corpus — source-fidelity parity report

Generated by `tests/gen_table_corpus.py --parity`. One verdict per **generated file**;
family rows are diagnostic roll-ups only.

**What this measures.** For every case with a `bench88` source, the extractor is scored on the
generated PDF and on the named real source page **with the same function** — bench100's
`table_score` (`0.5 * count-agreement + 0.5 * rows x cols on matched tables`). A case is
**oversimplified iff `C_gen - C_real > {PARITY_BAND}`**. Harder than reality always passes:
the failure mode of a generated corpus is being *cleaner* than the wild, never dirtier.

**No sibling averaging in the gate.** Every file is thresholded independently. Family means
below are useful diagnostics, but a hard sibling cannot cancel an oversimplified sibling.

**What this does NOT measure.** `C` is the **position-blind** metric — the one that scores 1.0
when the grid is right and every value sits under the wrong header. It is used here only
because it is the one metric computable on real pages, where no cell-level truth exists.
Parity on `C` is **necessary, not sufficient**; the cell-positional and header-attribution
gates in `tests/test_table_corpus.py` remain the real measurement.

**Provenance is not upgraded quietly.** The 88-document corpus contains **no SEC filings**, so
every SEC-shaped family (`sparse_wide`, `financial`, `numeric_alignment`, `glyph_per_tj`,
`t3_spacer_cols`) is reconstructed from the earlier 54-document corpus and is recorded
`parity: unrunnable` — never silently skipped, never counted as source coverage. The same
applies to the external pdfTeX calibration docs (000/002/006) and to the two cases whose
source lives in the 25-document `corpus_tests` corpus, which has cell truth but no rows/cols.

| rows | count |
|---|---|
| generated files parity-checked against a real bench88 page | {n_checked} |
| parity unrunnable (bench54 / external / bench25 source) | {n_unrun} |
| invented — no measured analogue, never claimed as coverage | {n_inv} |
| OVERSIMPLIFIED — accepted, excluded from the coverage claim | {len(accepted)} |
| **OVERSIMPLIFIED — unexplained (blocks citing the corpus)** | **{len(over)}** |

## Per-file verdicts

| tier | file | family | source | C_gen | C_real | gap | verdict |
|---|---|---|---|---|---|---|---|
""")
        for r in sorted(rows, key=lambda r: (r["tier"], r["family"], r["file"])):
            cr = f"{r['C_real']:.4f}" if r["C_real"] is not None else "—"
            gp = f"{r['gap']:+.4f}" if r["gap"] is not None else "—"
            f.write(f"| T{r['tier']} | `{r['file']}` | `{r['family']}` | {r['source']} | "
                    f"{r['C_gen']:.4f} | {cr} | {gp} | {r['verdict']} |\n")
        f.write("\n## Family summary (diagnostic only)\n\n"
                "`mean C_gen` is never used for adjudication; `max gap` exposes the easiest "
                "checkable sibling.\n\n"
                "| family | source | files | mean C_gen | C_real | max gap | disposition |\n"
                "|---|---|---:|---:|---:|---:|---|\n")
        for r in family_rows:
            cr = f"{r['C_real']:.4f}" if r["C_real"] is not None else "—"
            gp = f"{r['max_gap']:+.4f}" if r["max_gap"] is not None else "—"
            f.write(f"| `{r['family']}` | {r['source']} | {r['n_files']} | "
                    f"{r['mean_C_gen']:.4f} | {cr} | {gp} | {r['verdict']} |\n")
        if over:
            f.write("\n## Oversimplified — disposition required\n\n"
                    "A release with an unexplained `OVERSIMPLIFIED` row is a release whose "
                    "torture-corpus numbers may not be cited (spec §4.4).\n\n")
            for r in over:
                f.write(f"* **`{r['file']}`** (`{r['family']}`) vs {r['source']}: C_gen "
                        f"{r['C_gen']:.4f} vs C_real {r['C_real']:.4f} "
                        f"(gap {r['gap']:+.4f})\n")
        else:
            f.write("\n## Oversimplified — unexplained\n\nNone.\n")
        f.write("\n## Oversimplified — accepted, excluded from the coverage claim\n\n"
                "Enrichment was attempted on each of these and recorded; the flag stays in "
                "`truth.json` (`oversimplified: true`) so the coverage claim is computable "
                "offline, and the type is NOT claimed as source-covered. These cases remain "
                "valid REGRESSION locks — they are excluded from the *coverage* claim, not "
                "from the gate.\n\n")
        if accepted:
            for r in accepted:
                f.write(f"### `{r['file']}` (`{r['family']}`) vs {r['source']}\n\n"
                        f"C_gen {r['C_gen']:.4f} vs C_real {r['C_real']:.4f} "
                        f"(gap {r['gap']:+.4f}).\n\n{r['accepted_reason']}\n\n")
        else:
            f.write("None.\n")
        covered = sorted(r["file"] for r in rows
                         if r["verdict"] == "ok" and r["C_real"] is not None)
        not_covered = sorted(r["file"] for r in rows if r["verdict"] != "ok")
        f.write("\n## The coverage claim, stated exactly\n\n"
                "**Generated cases claimed as source-covered** — a real bench88 page was "
                "scored with the same "
                "function and the generated case is not materially easier than it:\n\n"
                + "".join(f"* `{f}`\n" for f in covered)
                + "\n**Generated cases NOT claimed as source coverage** — invented (no measured analogue), "
                  "parity unrunnable (the source is outside the 88-document corpus), or "
                  "oversimplified and accepted as such:\n\n"
                + "".join(f"* `{f}`\n" for f in not_covered))
    print(f"parity report -> {PARITY_REPORT}")


# ============================================================================== the driver

def generate():
    os.makedirs(OUT, exist_ok=True)
    TRUTH.clear()
    t0_negatives()
    t1_clean_singles()
    t2_confounds()
    t3_adversarial()
    doc = {"schema": 3, "seed": SEED, "files": TRUTH}
    with open(os.path.join(OUT, "truth.json"), "w") as f:
        json.dump(doc, f, indent=1, sort_keys=True)
        f.write("\n")
    return doc


def summarise(doc):
    files = doc["files"]
    n_tab = sum(len(v["tables"]) for v in files.values())
    tiers = {t: sum(1 for v in files.values() if v["tier"] == t) for t in (0, 1, 2, 3)}
    inv = sum(1 for v in files.values() if v.get("invented"))
    unr = sum(1 for v in files.values() if v.get("parity") == "unrunnable")
    b88 = sum(1 for v in files.values() if (v.get("source") or {}).get("corpus") == "bench88")
    tag = sum(1 for v in files.values() if v["tagged"])
    print(f"{len(files)} PDFs, {n_tab} ground-truth tables  "
          f"(T0 {tiers[0]} / T1 {tiers[1]} / T2 {tiers[2]} / T3 {tiers[3]})")
    print(f"  tagged variants: {tag}    bench88 parity-checkable: {b88}    "
          f"parity unrunnable: {unr}    invented: {inv}")
    print(f"-> {OUT}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--parity", action="store_true",
                    help="run the §4.3 anti-oversimplification gate and write the §4.4 report "
                         "(needs the local, licence-encumbered benchmarking/ tree)")
    ap.add_argument("--freeze", action="store_true",
                    help="(re)initialise tests/table_corpus/floors.json at measured - slack")
    ap.add_argument("--audit-seed", type=int, default=None,
                    help="§10.1.3 seed-sensitivity audit: regenerate elsewhere with another "
                         "seed and rescore, ungated")
    args = ap.parse_args(argv)

    global SEED, OUT
    if args.audit_seed is not None:
        import tempfile
        SEED = args.audit_seed
        OUT = tempfile.mkdtemp(prefix="table_corpus_seed%d_" % SEED)
    doc = generate()
    summarise(doc)

    if args.audit_seed is not None:
        from test_table_corpus import score_corpus
        res = score_corpus(OUT)
        print(f"seed {SEED}: positional cell accuracy {res['aggregate']['cell_acc']:.4f}, "
              f"grid exact {res['aggregate']['grid_exact']:.4f}, "
              f"header attribution {res['aggregate']['header_acc']:.4f}")
        print("(compare the committed run; large movement = content-specific overfitting)")
        return 0
    if args.freeze:
        from test_table_corpus import freeze_floors
        freeze_floors(OUT)
    if args.parity:
        rows, over = parity(OUT)
        for r in sorted(rows, key=lambda r: (r["tier"], r["family"], r["file"])):
            cr = f"{r['C_real']:.4f}" if r["C_real"] is not None else "  —   "
            gp = f"{r['gap']:+.4f}" if r["gap"] is not None else "   —   "
            print(f"{r['file']:44s} {r['family']:22s} {r['source']:38s} "
                  f"C_gen {r['C_gen']:.4f}  "
                  f"C_real {cr}  gap {gp}  {r['verdict']}")
        if over:
            print(f"\n{len(over)} OVERSIMPLIFIED row(s) — each must be enriched toward its "
                  f"source's measured parameters (§4.2) or keep the flag and be EXCLUDED "
                  f"from the coverage claim for its type.")
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
