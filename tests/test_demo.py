#!/usr/bin/env python3
"""End-to-end test on a self-contained demo PDF we own (tests/demo/demo.pdf).

This is the suite's CI backbone: it needs no third-party corpus, so it runs on every
fresh clone. It verifies distillpdf.to_html() recovers the KNOWN structure of a generated
document — title, headings, paragraphs, a bullet list, a borderless table, and a captioned
figure — plus reading order and the toc()/section() APIs, against demo_groundtruth.json.

Regenerate the fixture (PDF + ground truth) with `python tests/gen_demo.py` (dev deps:
reportlab + pillow). The fixture is committed; the generator is not needed at test time.
"""
import json
import os
import re

import distillpdf

HERE = os.path.dirname(os.path.abspath(__file__))
DEMO = os.path.join(HERE, "demo", "demo.pdf")
with open(os.path.join(HERE, "demo", "demo_groundtruth.json")) as _f:
    GT = json.load(_f)


def _html():
    return distillpdf.Pdf.open(DEMO).to_html()


def _text(html):
    return re.sub(r"\s+", " ", re.sub(r"<[^>]+>", " ", html)).strip()


def _cells(html, tag="t[dh]"):
    return [re.sub(r"\s+", " ", re.sub(r"<[^>]+>", "", c)).strip()
            for c in re.findall(rf"<{tag}[^>]*>(.*?)</{tag}>", html, re.DOTALL)]


def test_title_is_h1():
    h = _html()
    assert re.search(r"<h1\b[^>]*>\s*" + re.escape(GT["title"]) + r"\s*</h1>", h), "title not emitted as <h1>"


def test_headings_present():
    h = _html()
    for head in GT["headings"]:
        assert re.search(r"<h[1-6]\b[^>]*>(?:\s*<b>)?\s*" + re.escape(head), h), f"missing heading: {head!r}"


def test_paragraphs_intact():
    t = _text(_html())  # no loss, no scramble
    for snippet in GT["paragraph_snippets"]:
        assert snippet in t, f"paragraph text lost or scrambled: {snippet!r}"


def test_bullet_list():
    items = _cells(_html(), tag="li")
    for bullet in GT["list_items"]:
        assert bullet in items, f"bullet not in a <li>: {bullet!r}"


def test_table_cells():
    cells = _cells(_html())
    expected = GT["table"]["header"] + [c for row in GT["table"]["rows"] for c in row]
    for cell in expected:
        assert cell in cells, f"table cell missing: {cell!r}"


def test_figure_with_caption():
    h = _html()
    assert "<img" in h or "<svg" in h, "figure graphic missing"
    assert GT["figure_caption_prefix"] in h, "figure caption missing"


def test_reading_order():
    h = _html()
    pos = [h.find(head) for head in GT["headings"]]
    assert all(p >= 0 for p in pos), "a heading is missing from the output"
    assert pos == sorted(pos), "headings are out of reading order"


def test_toc():
    titles = [t for (_lvl, t, _pg, _id) in distillpdf.Pdf.open(DEMO).toc()]
    assert any(GT["title"] in t for t in titles), "TOC missing the title"
    for head in GT["headings"]:
        assert any(head in t for t in titles), f"TOC missing heading: {head!r}"


def test_section_extraction():
    sec = distillpdf.Pdf.open(DEMO).section(GT["section_probe"])
    assert sec, "section() returned nothing for the Introduction"
    assert "reconstructs the logical structure" in _text(sec), "section() body is wrong"


if __name__ == "__main__":
    for _n, _fn in sorted(globals().items()):
        if _n.startswith("test_") and callable(_fn):
            _fn()
            print("ok", _n)
    print("DEMO OK")
