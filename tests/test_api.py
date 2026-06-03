"""The Pdf public API surface, exercised on owned fixtures: open/from_bytes, page_count,
plain-text extraction (whole-doc + per-page), and the structured extractors
(fonts/images/tables/links) plus the diagnostic span dumps — shapes, types, and basic
invariants. Guards the API contract that downstream callers depend on."""
import os

import distillpdf
import pytest

from _fixtures import FIX

HEADINGS = os.path.join(FIX, "headings.pdf")
FIGURES = os.path.join(FIX, "figures.pdf")
NUMERIC = os.path.join(FIX, "numeric.pdf")
LINKS = os.path.join(FIX, "links.pdf")


def test_open_and_page_count():
    assert distillpdf.Pdf.open(HEADINGS).page_count() == 1
    assert distillpdf.Pdf.open(LINKS).page_count() == 2


def test_module_level_open():
    """The top-level distillpdf.open()/from_bytes() shorthands return a Pdf and match
    the Pdf.open()/Pdf.from_bytes() classmethods."""
    doc = distillpdf.open(HEADINGS)
    assert isinstance(doc, distillpdf.Pdf)
    assert doc.page_count() == 1
    assert doc.to_html() == distillpdf.Pdf.open(HEADINGS).to_html()
    with open(HEADINGS, "rb") as f:
        data = f.read()
    assert distillpdf.from_bytes(data).to_html() == distillpdf.Pdf.from_bytes(data).to_html()


def test_from_bytes_matches_open():
    with open(HEADINGS, "rb") as f:
        data = f.read()
    a = distillpdf.Pdf.from_bytes(data).to_html()
    b = distillpdf.Pdf.open(HEADINGS).to_html()
    assert a == b, "from_bytes() and open() produced different HTML"


def test_open_bad_path_raises():
    with pytest.raises(Exception):
        distillpdf.Pdf.open(os.path.join(FIX, "does_not_exist.pdf"))


def test_extract_text_whole_and_per_page():
    d = distillpdf.Pdf.open(HEADINGS)
    whole = d.extract_text()
    assert "Structural Heading Showcase" in whole
    assert "Introduction" in whole
    p1 = d.extract_page_text(1)
    assert "Introduction" in p1
    assert p1.strip() in whole or p1.split("\n")[0] in whole


def test_extract_fonts():
    fonts = distillpdf.Pdf.open(HEADINGS).extract_fonts()
    assert isinstance(fonts, list) and fonts, "no fonts reported"
    f = fonts[0]
    for key in ("page", "name", "subtype", "base_font", "embedded", "has_tounicode"):
        assert key in f, f"font dict missing {key!r}"
    assert any("Helvetica" in (x["base_font"] or "") for x in fonts), "expected a Helvetica base font"


def test_extract_images():
    imgs = distillpdf.Pdf.open(FIGURES).extract_images()
    assert isinstance(imgs, list) and len(imgs) >= 1, "raster image not extracted"
    im = imgs[0]
    for key in ("page", "index", "width", "height", "color_space", "format", "data"):
        assert key in im, f"image dict missing {key!r}"
    assert im["width"] > 0 and im["height"] > 0
    assert isinstance(im["data"], (bytes, bytearray)) and len(im["data"]) > 0


def test_extract_tables():
    tables = distillpdf.Pdf.open(NUMERIC).extract_tables()
    assert tables, "numeric table not extracted"
    t = tables[0]
    for key in ("page", "n_rows", "n_cols", "cells"):
        assert key in t, f"table dict missing {key!r}"
    flat = [c for row in t["cells"] for c in row]
    assert "System" in " ".join(flat) and "84.6" in " ".join(flat), "table cells wrong"


def test_extract_links_shapes():
    links = distillpdf.Pdf.open(LINKS).extract_links()
    assert len(links) >= 2
    for lk in links:
        for key in ("page", "rect", "kind", "uri", "dest_page", "dest_name"):
            assert key in lk, f"link dict missing {key!r}"
        assert lk["kind"] in ("uri", "internal")
        assert len(lk["rect"]) == 4


def test_dbg_spans():
    d = distillpdf.Pdf.open(NUMERIC)
    spans = d._dbg_spans(1)
    assert spans and all(len(s) == 4 for s in spans), "_dbg_spans shape wrong"
    xy = d._dbg_spans_xy(1)
    assert xy and all(len(s) == 5 for s in xy), "_dbg_spans_xy shape wrong"


def test_toc_and_section_types():
    d = distillpdf.Pdf.open(HEADINGS)
    toc = d.toc()
    assert toc and all(len(e) == 4 for e in toc), "toc tuple shape wrong"
    lvl, title, page, anchor = toc[0]
    assert isinstance(lvl, int) and isinstance(title, str) and isinstance(page, int)
    assert distillpdf.Pdf.open(HEADINGS).section("methods") is not None
