#!/usr/bin/env python3
"""Generate synthetic table PDFs with KNOWN ground truth, across styles & layouts.

Output:
  bench/corpus_tables/*.pdf
  bench/corpus_tables/ground_truth.json   { file: {tables:[{rows,cols,cells,style,has_header}], is_negative} }

Used by test_tables.py to score pylopdf's table detection (precision/recall/structure/
cell-content) against ground truth — the robust goalline for table extraction.
"""
import json, os, random
from reportlab.lib import colors
from reportlab.lib.pagesizes import letter
from reportlab.lib.units import inch
from reportlab.platypus import (SimpleDocTemplate, Table, TableStyle, Paragraph, Spacer,
                                BaseDocTemplate, Frame, PageTemplate)
from reportlab.lib.styles import getSampleStyleSheet

OUT = os.path.join(os.path.dirname(__file__), "corpus_tables")
os.makedirs(OUT, exist_ok=True)
S = getSampleStyleSheet()
GT = {}

PROSE = ("Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod "
         "tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, "
         "quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo.")

HEADERS = ["Name", "Region", "Samples", "Depth m", "Grade %"]
WORDS = ["Alpha","Beta","Gamma","Delta","Basalt","Granite","Shale","North","South","East",
         "West","Quartz","Olivine","Feldspar","Pyrite","Calcite","Zone","Unit","Layer"]


def make_cells(rows, cols, header=True, numeric_cols=()):
    data = []
    if header:
        data.append([HEADERS[c % len(HEADERS)] for c in range(cols)])
    for r in range(rows - (1 if header else 0)):
        row = []
        for c in range(cols):
            if c in numeric_cols:
                row.append(f"{random.randint(1,999)}.{random.randint(0,9)}")
            else:
                row.append(f"{random.choice(WORDS)}{r+1}")
        data.append(row)
    return data


def style_for(name, cols, header, numeric_cols):
    cmds = [("FONTSIZE", (0, 0), (-1, -1), 9),
            ("LEFTPADDING", (0, 0), (-1, -1), 6),
            ("RIGHTPADDING", (0, 0), (-1, -1), 6)]
    for c in numeric_cols:
        cmds.append(("ALIGN", (c, 0), (c, -1), "RIGHT"))
    if header:
        cmds.append(("FONTNAME", (0, 0), (-1, 0), "Helvetica-Bold"))
    if name == "grid":
        cmds.append(("GRID", (0, 0), (-1, -1), 0.5, colors.black))
    elif name == "box":
        cmds.append(("BOX", (0, 0), (-1, -1), 1, colors.black))
    elif name == "hrules":
        cmds.append(("LINEABOVE", (0, 0), (-1, 0), 1, colors.black))
        cmds.append(("LINEBELOW", (0, 0), (-1, 0), 0.5, colors.black))
        cmds.append(("LINEBELOW", (0, -1), (-1, -1), 1, colors.black))
    elif name == "header_shaded":
        cmds.append(("BACKGROUND", (0, 0), (-1, 0), colors.lightgrey))
        cmds.append(("GRID", (0, 0), (-1, -1), 0.5, colors.grey))
    # "borderless" => no line commands (whitespace-only table)
    return TableStyle(cmds)


def build(fname, story, tables_gt, negative=False):
    SimpleDocTemplate(os.path.join(OUT, fname), pagesize=letter,
                      topMargin=0.8*inch, bottomMargin=0.8*inch).build(story)
    GT[fname] = {"tables": tables_gt, "is_negative": negative}


def table_flow(cells, style_name, header, numeric_cols, colw=1.1*inch):
    t = Table(cells, colWidths=[colw]*len(cells[0]))
    t.setStyle(style_for(style_name, len(cells[0]), header, numeric_cols))
    return t, {"rows": len(cells), "cols": len(cells[0]), "cells": cells,
               "style": style_name, "has_header": header}


def main():
    random.seed(42)  # deterministic
    styles = ["grid", "box", "hrules", "header_shaded", "borderless"]
    idx = 0

    # 1. each style, standalone, 3 cols x 6 rows, header, 2 numeric cols
    for st in styles:
        cells = make_cells(6, 3, header=True, numeric_cols=(2,))
        t, gt = table_flow(cells, st, True, (2,))
        build(f"tbl_{idx:02d}_{st}_3x6.pdf", [Paragraph(f"{st} table", S["Heading2"]), t], [gt]); idx += 1

    # 2. column-count variation (grid + borderless)
    for st in ("grid", "borderless"):
        for cols in (2, 5):
            cells = make_cells(8, cols, header=True, numeric_cols=tuple(range(2, cols)))
            t, gt = table_flow(cells, st, True, tuple(range(2, cols)), colw=0.9*inch)
            build(f"tbl_{idx:02d}_{st}_{cols}col.pdf", [t], [gt]); idx += 1

    # 3. large table (20 rows)
    cells = make_cells(20, 4, header=True, numeric_cols=(2, 3))
    t, gt = table_flow(cells, "grid", True, (2, 3))
    build(f"tbl_{idx:02d}_grid_large.pdf", [t], [gt]); idx += 1

    # 4. table embedded in prose (before + after) — tests detection amid text + precision
    for st in ("grid", "borderless"):
        cells = make_cells(6, 3, header=True, numeric_cols=(2,))
        t, gt = table_flow(cells, st, True, (2,))
        story = [Paragraph(PROSE, S["BodyText"]), Spacer(1, 12), t, Spacer(1, 12),
                 Paragraph(PROSE, S["BodyText"])]
        build(f"tbl_{idx:02d}_{st}_in_prose.pdf", story, [gt]); idx += 1

    # 5. two tables on one page
    c1 = make_cells(5, 3, True, (2,)); t1, g1 = table_flow(c1, "grid", True, (2,))
    c2 = make_cells(5, 2, True, ()); t2, g2 = table_flow(c2, "header_shaded", True, ())
    build(f"tbl_{idx:02d}_two_tables.pdf",
          [t1, Spacer(1, 24), Paragraph("Second table follows.", S["BodyText"]), Spacer(1, 12), t2],
          [g1, g2]); idx += 1

    # 6. no-header table (data only)
    cells = make_cells(6, 4, header=False, numeric_cols=(2, 3))
    t, gt = table_flow(cells, "grid", False, (2, 3))
    build(f"tbl_{idx:02d}_grid_noheader.pdf", [t], [gt]); idx += 1

    # 7. NEGATIVES (no tables) — precision / false-positive checks
    build(f"tbl_{idx:02d}_neg_prose.pdf",
          [Paragraph(PROSE + " " + PROSE, S["BodyText"]) for _ in range(4)], [], negative=True); idx += 1
    build(f"tbl_{idx:02d}_neg_list.pdf",
          [Paragraph(f"• item {i} — {random.choice(WORDS)} description here", S["BodyText"]) for i in range(10)],
          [], negative=True); idx += 1
    build(f"tbl_{idx:02d}_neg_headings.pdf",
          sum(([Paragraph(f"Section {i}", S["Heading2"]), Paragraph(PROSE, S["BodyText"])] for i in range(4)), []),
          [], negative=True); idx += 1

    # HARD negative: two-column flowing prose (newspaper/paper layout) — the classic
    # false-positive trap. Must NOT be detected as a table.
    twocol = f"tbl_{idx:02d}_neg_twocol_prose.pdf"; idx += 1
    doc = BaseDocTemplate(os.path.join(OUT, twocol), pagesize=letter,
                          topMargin=0.8*inch, bottomMargin=0.8*inch)
    fw = (letter[0] - 2*inch) / 2 - 8
    frames = [Frame(inch, inch, fw, letter[1]-1.6*inch),
              Frame(inch + fw + 16, inch, fw, letter[1]-1.6*inch)]
    doc.addPageTemplates(PageTemplate(frames=frames))
    doc.build([Paragraph(PROSE, S["BodyText"]) for _ in range(8)])
    GT[twocol] = {"tables": [], "is_negative": True}

    # HARD negative: aligned label/value lines ("Field N    value") — looks 2-column.
    build(f"tbl_{idx:02d}_neg_labelvalue.pdf",
          [Paragraph(f"{random.choice(WORDS)} field {i}:&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;{random.randint(1,99)}", S["BodyText"]) for i in range(12)],
          [], negative=True); idx += 1

    with open(os.path.join(OUT, "ground_truth.json"), "w") as f:
        json.dump(GT, f, indent=2)
    n_tables = sum(len(v["tables"]) for v in GT.values())
    print(f"generated {len(GT)} PDFs, {n_tables} ground-truth tables, "
          f"{sum(1 for v in GT.values() if v['is_negative'])} negatives -> {OUT}")


if __name__ == "__main__":
    main()
