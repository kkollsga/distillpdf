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
from reportlab.lib.styles import getSampleStyleSheet, ParagraphStyle

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
               "style": style_name, "has_header": header, "complex": False}


CELL = ParagraphStyle("cell", fontSize=8, leading=9)


def complex_table(cells, extra_cmds, feature, colw=1.0*inch, gridded=True):
    """Build a complex table. `cells` is the LOGICAL grid (plain strings; '' for
    spanned/empty slots). `extra_cmds` carries SPAN/wrap commands."""
    # PDF cells: wrap long strings in Paragraphs so they actually multi-line.
    pdf = [[Paragraph(c, CELL) if isinstance(c, str) and len(c) > 24 else c for c in row]
           for row in cells]
    t = Table(pdf, colWidths=[colw] * len(cells[0]))
    cmds = [("FONTSIZE", (0, 0), (-1, -1), 8),
            ("VALIGN", (0, 0), (-1, -1), "MIDDLE")]
    if gridded:
        cmds.append(("GRID", (0, 0), (-1, -1), 0.5, colors.black))
    cmds += extra_cmds
    t.setStyle(TableStyle(cmds))
    return t, {"rows": len(cells), "cols": len(cells[0]), "cells": cells,
               "style": feature, "has_header": True, "complex": True}


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

    # ---- COMPLEX / MESSY tables (part of the goalline) ----
    # C1: spanning group header (colspan)
    cells = [["Geochemistry", "", "", "Location", ""],
             ["Sample", "Depth", "Grade", "Lat", "Lon"]]
    for r in range(6):
        cells.append([f"S{r+1}", f"{r*3+2}.1", f"{r+1}.4", f"{60+r}.2", f"{10+r}.5"])
    t, gt = complex_table(cells, [("SPAN", (0, 0), (2, 0)), ("SPAN", (3, 0), (4, 0)),
                                  ("FONTNAME", (0, 0), (-1, 1), "Helvetica-Bold")], "colspan_header", colw=0.85*inch)
    build(f"tbl_{idx:02d}_cmplx_colspan.pdf", [t], [gt]); idx += 1

    # C2: row-spanning category labels (rowspan)
    cells = [["Zone", "Sample", "Value", "Unit"]]
    for z, n in (("North", 3), ("South", 3)):
        for i in range(n):
            cells.append([z if i == 0 else "", f"{z[0]}{i+1}", f"{random.randint(1,99)}", "ppm"])
    spans = [("SPAN", (0, 1), (0, 3)), ("SPAN", (0, 4), (0, 6)),
             ("FONTNAME", (0, 0), (-1, 0), "Helvetica-Bold")]
    t, gt = complex_table(cells, spans, "rowspan", colw=1.0*inch)
    build(f"tbl_{idx:02d}_cmplx_rowspan.pdf", [t], [gt]); idx += 1

    # C3: multi-line wrapped cells
    cells = [["Method", "Description", "Result"]]
    for i in range(5):
        cells.append([f"M{i+1}",
                      f"A fairly long description of method {i+1} that wraps across multiple lines within the cell",
                      f"{random.choice(WORDS)} outcome {i+1}"])
    t, gt = complex_table(cells, [("FONTNAME", (0, 0), (-1, 0), "Helvetica-Bold")],
                          "multiline", colw=1.7*inch)
    build(f"tbl_{idx:02d}_cmplx_multiline.pdf", [t], [gt]); idx += 1

    # C4: nested two-row header (group + sub) + grid
    cells = [["", "Phase 1", "", "Phase 2", ""],
             ["ID", "Min", "Max", "Min", "Max"]]
    for r in range(6):
        cells.append([f"R{r+1}"] + [f"{random.randint(1,50)}" for _ in range(4)])
    t, gt = complex_table(cells, [("SPAN", (1, 0), (2, 0)), ("SPAN", (3, 0), (4, 0)),
                                  ("FONTNAME", (0, 0), (-1, 1), "Helvetica-Bold")], "nested_header", colw=0.8*inch)
    build(f"tbl_{idx:02d}_cmplx_nested.pdf", [t], [gt]); idx += 1

    # C5: sparse table (many empty cells)
    cells = [["Item", "Q1", "Q2", "Q3", "Q4"]]
    for r in range(6):
        row = [f"Item{r+1}"] + ["" for _ in range(4)]
        row[random.randint(1, 4)] = f"{random.randint(10,99)}"
        cells.append(row)
    t, gt = complex_table(cells, [("FONTNAME", (0, 0), (-1, 0), "Helvetica-Bold")], "sparse", colw=0.85*inch)
    build(f"tbl_{idx:02d}_cmplx_sparse.pdf", [t], [gt]); idx += 1

    # C6: wide table (9 columns)
    cells = [[f"C{c}" for c in range(9)]]
    for r in range(6):
        cells.append([f"{random.randint(1,99)}" for _ in range(9)])
    t, gt = complex_table(cells, [("FONTNAME", (0, 0), (-1, 0), "Helvetica-Bold")], "wide9", colw=0.62*inch)
    build(f"tbl_{idx:02d}_cmplx_wide.pdf", [t], [gt]); idx += 1

    # C7: borderless + colspan (hardest — no lines, merged header)
    cells = [["Results Summary", "", ""], ["Sample", "Mean", "StdDev"]]
    for r in range(6):
        cells.append([f"X{r+1}", f"{r+1}.2", f"0.{r+1}"])
    t, gt = complex_table(cells, [("SPAN", (0, 0), (2, 0)),
                                  ("FONTNAME", (0, 0), (-1, 1), "Helvetica-Bold")],
                          "borderless_merged", colw=1.0*inch, gridded=False)
    build(f"tbl_{idx:02d}_cmplx_borderless_merged.pdf", [t], [gt]); idx += 1

    # C8: table inside a two-column page (academic layout) — detection amid columns
    twc = f"tbl_{idx:02d}_cmplx_in_twocol.pdf"; idx += 1
    cells = make_cells(7, 3, header=True, numeric_cols=(1, 2))
    tt, gtc = table_flow(cells, "grid", True, (1, 2), colw=0.7*inch)
    gtc["complex"] = True; gtc["style"] = "in_twocol"
    doc = BaseDocTemplate(os.path.join(OUT, twc), pagesize=letter, topMargin=0.8*inch, bottomMargin=0.8*inch)
    fw = (letter[0] - 2*inch) / 2 - 8
    doc.addPageTemplates(PageTemplate(frames=[Frame(inch, inch, fw, letter[1]-1.6*inch),
                                              Frame(inch+fw+16, inch, fw, letter[1]-1.6*inch)]))
    doc.build([Paragraph(PROSE, S["BodyText"]), Spacer(1, 8), tt, Spacer(1, 8),
               Paragraph(PROSE, S["BodyText"]), Paragraph(PROSE, S["BodyText"])])
    GT[twc] = {"tables": [gtc], "is_negative": False}

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
