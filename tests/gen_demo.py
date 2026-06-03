#!/usr/bin/env python3
"""Generate a self-contained DEMO pdf (we own it — no third-party content) + ground truth.

Run once to (re)create tests/demo/demo.pdf and tests/demo/demo_groundtruth.json, both of
which are COMMITTED so test_demo.py can verify distillpdf end-to-end on CI without the
gitignored corpus. Deterministic content → a stable regression fixture.

Uses the low-level canvas (absolute text positioning, no per-block `cm`) so the layout
matches how real PDFs place text and is extracted cleanly.

    python tests/gen_demo.py        # needs reportlab + pillow (dev only)
"""
import json
import os

from PIL import Image, ImageDraw
from reportlab.lib.pagesizes import letter
from reportlab.lib.styles import getSampleStyleSheet
from reportlab.lib.utils import simpleSplit
from reportlab.pdfgen import canvas
from reportlab.platypus import Paragraph, SimpleDocTemplate, Spacer

OUT = os.path.join(os.path.dirname(__file__), "demo")
os.makedirs(OUT, exist_ok=True)

TITLE = "DistillPDF Demo Document"
ABSTRACT = ("This document is a self-contained demo used by the distillpdf test suite. It "
            "exercises titles, headings, paragraphs, lists, tables and figures so the "
            "extraction pipeline can be verified without any third-party PDFs.")
INTRO_1 = ("DistillPDF reconstructs the logical structure of a PDF and emits clean, "
           "semantic HTML. This introduction is plain body prose that should be extracted "
           "as ordinary paragraphs in reading order.")
INTRO_2 = ("The following sections demonstrate a bulleted list, a data table and an "
           "embedded figure, each with known content the tests assert against.")
METHODS = ("We render this document with a fixed layout so its extracted structure is "
           "deterministic and can serve as a stable regression fixture.")
BULLETS = [
    "Headings are recovered from font size and weight.",
    "Tables are reconstructed into rows and cells.",
    "Figures are wrapped together with their captions.",
]
TABLE = [["Metric", "Value", "Unit"],
         ["Recall", "0.98", "ratio"],
         ["Latency", "12", "ms"]]
FIG_CAPTION = "Figure 1: A simple demo chart with two bars."

GROUND_TRUTH = {
    "title": TITLE,
    "headings": ["Abstract", "1 Introduction", "2 Methods"],
    "list_items": BULLETS,
    "table": {"header": TABLE[0], "rows": TABLE[1:]},
    "figure_caption_prefix": "Figure 1",
    # snippets that must survive extraction intact (no loss / no scramble)
    "paragraph_snippets": [
        "self-contained demo used by the distillpdf",
        "reconstructs the logical structure of a PDF",
        "fixed layout so its extracted structure is",
    ],
    "section_probe": "introduction",
}

PAGE_W, PAGE_H = letter
LM = 72            # left margin
COL_W = PAGE_W - 2 * LM
BODY_F, BODY_S = "Helvetica", 10.5
LEAD = 14          # line leading


def make_chart_png(path):
    img = Image.new("RGB", (320, 200), "white")
    d = ImageDraw.Draw(img)
    d.rectangle([10, 10, 309, 189], outline="black", width=2)
    d.rectangle([60, 120, 130, 180], fill=(70, 110, 200))
    d.rectangle([190, 60, 260, 180], fill=(200, 90, 70))
    img.save(path)


# A second fixture built with the HIGH-LEVEL platypus API (SimpleDocTemplate). Unlike
# the canvas above, platypus positions every flowable through a per-block graphics
# transform (`q [1 0 0 1 tx ty] cm BT … ET Q`) — text laid out in a LOCAL frame and only
# translated into place. If the extractor ignores that translate, every block collapses
# to local coordinates and the document scrambles. This fixture pins the cm-positioning
# regression: the sections must come out top-to-bottom, intact, in order.
PLATYPUS_SECTIONS = [
    ("Overview", "The overview section introduces the platypus layout pipeline and sets the stage."),
    ("Background", "Background material explains how each flowable is positioned by a translate transform."),
    ("Approach", "Our approach reconstructs the reading order directly from the graphics transform."),
    ("Results", "Results confirm the blocks are extracted from top to bottom with no content loss."),
    ("Conclusion", "In conclusion the cm positioned document reads in exactly the correct order."),
]
PLATYPUS_GT = {
    "headings": [h for h, _ in PLATYPUS_SECTIONS],
    # the ordered body sentences that must appear verbatim (no loss / no scramble) and
    # in this exact reading order:
    "ordered": [b for _, b in PLATYPUS_SECTIONS],
}


def build_platypus():
    pdf = os.path.join(OUT, "demo_platypus.pdf")
    styles = getSampleStyleSheet()
    story = []
    for head, body in PLATYPUS_SECTIONS:
        story.append(Paragraph(head, styles["Heading2"]))
        story.append(Paragraph(body, styles["BodyText"]))
        story.append(Spacer(1, 10))
    SimpleDocTemplate(pdf, pagesize=letter, topMargin=72, bottomMargin=72).build(story)
    print("wrote", pdf)


def build():
    png = os.path.join(OUT, "_chart.png")
    make_chart_png(png)
    pdf = os.path.join(OUT, "demo.pdf")
    c = canvas.Canvas(pdf, pagesize=letter)
    c.setTitle(TITLE)
    y = PAGE_H - 80

    def para(text, indent=0, gap=8):
        nonlocal y
        c.setFont(BODY_F, BODY_S)
        for line in simpleSplit(text, BODY_F, BODY_S, COL_W - indent):
            c.drawString(LM + indent, y, line)
            y -= LEAD
        y -= gap

    def heading(text):
        nonlocal y
        c.setFont("Helvetica-Bold", 13)
        c.drawString(LM, y, text)
        y -= 18

    c.setFont("Helvetica-Bold", 20)
    c.drawCentredString(PAGE_W / 2, y, TITLE)
    y -= 34

    heading("Abstract"); para(ABSTRACT)
    heading("1 Introduction"); para(INTRO_1); para(INTRO_2)
    for b in BULLETS:
        # ASCII '*' marker: reportlab encodes a real '•' as 0x7f in its Helvetica subset,
        # which isn't a recognized bullet; '*' is a clean weak-bullet distillpdf detects.
        para("*  " + b, indent=18, gap=3)
    y -= 6
    heading("2 Methods"); para(METHODS)
    y -= 18   # clear gap so the table below doesn't absorb the last prose line

    # Borderless table: header (bold) + rows at fixed column x-positions.
    col_x = [LM, LM + 150, LM + 230]
    c.setFont("Helvetica-Bold", 10)
    for cx, cell in zip(col_x, TABLE[0]):
        c.drawString(cx, y, cell)
    y -= LEAD
    c.setFont("Helvetica", 10)
    for row in TABLE[1:]:
        for cx, cell in zip(col_x, row):
            c.drawString(cx, y, cell)
        y -= LEAD
    y -= 24

    # Figure: image then caption.
    img_w, img_h = 173, 108
    c.drawImage(png, LM, y - img_h, width=img_w, height=img_h)
    y -= img_h + 14
    c.setFont("Helvetica", 9.5)
    c.drawCentredString(LM + img_w / 2, y, FIG_CAPTION)

    c.showPage()
    c.save()
    os.remove(png)
    with open(os.path.join(OUT, "demo_groundtruth.json"), "w") as f:
        json.dump({**GROUND_TRUTH, "platypus": PLATYPUS_GT}, f, indent=2)
    print("wrote", pdf, "+ demo_groundtruth.json")


if __name__ == "__main__":
    build()
    build_platypus()
