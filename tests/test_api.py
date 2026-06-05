"""The Pdf public API surface, exercised on owned fixtures: open/from_bytes, page_count,
plain-text extraction (whole-doc + per-page), and the structured extractors
(fonts/images/tables/links) plus the diagnostic span dumps — shapes, types, and basic
invariants. Guards the API contract that downstream callers depend on."""
import os
import re

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
    assert doc.to_html(return_string=True) == distillpdf.Pdf.open(HEADINGS).to_html(return_string=True)
    with open(HEADINGS, "rb") as f:
        data = f.read()
    assert distillpdf.from_bytes(data).to_html(return_string=True) == distillpdf.Pdf.from_bytes(data).to_html(return_string=True)


def test_from_bytes_matches_open():
    with open(HEADINGS, "rb") as f:
        data = f.read()
    a = distillpdf.Pdf.from_bytes(data).to_html(return_string=True)
    b = distillpdf.Pdf.open(HEADINGS).to_html(return_string=True)
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


def test_to_html_render_options():
    """Rendering options live on to_html() (not open): mode/toc per call.
    `return_string=True` returns the HTML instead of writing a file."""
    d = distillpdf.Pdf.open(HEADINGS)
    assert d.to_html(return_string=True).startswith("<!doctype html>")
    assert "data-page" not in d.to_html(return_string=True)          # section default
    assert "data-page" in d.to_html(mode="page", return_string=True)  # page mode
    assert "<nav>" not in d.to_html(toc=False, return_string=True)
    assert d.to_html(mode="page", return_string=True) == d.to_html(mode="page", return_string=True)


def test_open_takes_no_render_options():
    """open() no longer accepts rendering options — they belong on to_html()."""
    with pytest.raises(TypeError):
        distillpdf.Pdf.open(HEADINGS, mode="page")


def test_to_html_to_file(tmp_path):
    """to_html() writes a file by default and returns 1; an explicit path or directory
    chooses the location, and the written content matches return_string=True."""
    d = distillpdf.Pdf.open(HEADINGS)
    s = d.to_html(return_string=True)
    assert s.startswith("<!doctype html>")
    # explicit file path → writes there, returns 1, content matches the string form
    dest = tmp_path / "out.html"
    assert d.to_html(str(dest)) == 1
    assert dest.read_text(encoding="utf-8") == s
    # directory → <source-stem>.html inside it
    assert d.to_html(str(tmp_path)) == 1
    assert (tmp_path / "headings.html").exists()
    # options carry through
    d.to_html(str(dest), mode="page")
    assert "data-page" in dest.read_text(encoding="utf-8")


def test_to_html_default_writes_next_to_source(tmp_path):
    """With no path, to_html() writes <source>.html next to the opened PDF and returns 1."""
    src = tmp_path / "doc.pdf"
    src.write_bytes(open(HEADINGS, "rb").read())
    assert distillpdf.Pdf.open(str(src)).to_html() == 1
    assert (tmp_path / "doc.html").read_text(encoding="utf-8").startswith("<!doctype html>")


def test_to_html_from_bytes_needs_path(tmp_path):
    with open(HEADINGS, "rb") as f:
        d = distillpdf.from_bytes(f.read())
    with pytest.raises(Exception):
        d.to_html()  # no source path to derive a name, and not return_string
    assert d.to_html(return_string=True).startswith("<!doctype html>")  # string is fine
    dest = tmp_path / "b.html"
    assert d.to_html(str(dest)) == 1


def test_to_markdown_string_placeholders():
    """to_markdown(return_string=True) returns a string; images are caption-only
    placeholders (no data: URIs), headings/tables become Markdown."""
    md = distillpdf.Pdf.open(HEADINGS).to_markdown(return_string=True)
    assert isinstance(md, str) and md.strip()
    assert "](data:" not in md          # no inline bytes for string output
    assert md.lstrip().startswith(("#", "-"))  # heading or TOC list, not HTML


def test_to_markdown_to_file_extracts_images(tmp_path):
    """to_markdown() to a file (external by default) writes the .md plus an img/ folder of
    figure files, referenced relatively, and returns 1."""
    dest = tmp_path / "fig.md"
    assert distillpdf.Pdf.open(FIGURES).to_markdown(str(dest)) == 1
    md = dest.read_text(encoding="utf-8")
    imgdir = tmp_path / "img"
    assert imgdir.is_dir() and any(imgdir.iterdir()), "no img/ files written"
    # every img/ reference resolves to a real file
    import re
    refs = re.findall(r"\]\((img/[^)]+)\)", md)
    assert refs, "no img/ references in markdown"
    for r in refs:
        assert (tmp_path / r).exists(), f"missing {r}"


def test_to_markdown_drop_placeholders():
    md = distillpdf.Pdf.open(FIGURES).to_markdown(image_mode="drop", return_string=True)
    assert "](data:" not in md  # never any image bytes


def test_export_html_removed():
    """The old export_html() method is gone — superseded by to_html(path=...)."""
    assert not hasattr(distillpdf.Pdf.open(HEADINGS), "export_html")


def test_to_html_external_images(tmp_path):
    """to_html(image_mode="external") extracts figures to an img/ folder and references them,
    instead of inlining base64 — a much smaller file."""
    d = distillpdf.Pdf.open(FIGURES)
    inline = d.to_html(return_string=True)  # default embed → self-contained / inline
    assert "data:image" in inline

    dest = tmp_path / "fig.html"
    assert d.to_html(str(dest), image_mode="external") == 1
    h = dest.read_text(encoding="utf-8")
    assert "data:image" not in h          # nothing inlined
    assert "<svg" not in h                # vector figures externalised to .svg files
    imgdir = tmp_path / "img"
    assert imgdir.is_dir() and any(imgdir.iterdir())
    refs = re.findall(r'src="(img/[^"]+)"', h)
    assert refs and all((tmp_path / r).exists() for r in refs)
    assert len(h) < len(inline)          # externalised HTML is smaller


def test_to_html_string_is_self_contained(tmp_path):
    """A returned string has no folder to write into, so it stays self-contained (inline)
    and creates no img/ folder."""
    d = distillpdf.Pdf.open(FIGURES)
    h = d.to_html(return_string=True)
    assert "data:image" in h
    assert not (tmp_path / "img").exists()


def test_to_html_default_embed_to_file(tmp_path):
    """The default image_mode is embed: writing to a file gives one self-contained .html
    (inline images, no img/ folder)."""
    dest = tmp_path / "e.html"
    assert distillpdf.Pdf.open(FIGURES).to_html(str(dest)) == 1
    assert "data:image" in dest.read_text(encoding="utf-8")
    assert not (tmp_path / "img").exists()


def test_invalid_image_mode_raises():
    with pytest.raises(Exception):
        distillpdf.Pdf.open(FIGURES).to_html(image_mode="bogus")


def test_to_html_and_markdown_share_img_layout(tmp_path):
    """HTML and Markdown file output write the same img/ filenames for the same PDF."""
    h_dir = tmp_path / "h"
    m_dir = tmp_path / "m"
    h_dir.mkdir()
    m_dir.mkdir()
    distillpdf.Pdf.open(FIGURES).to_html(str(h_dir / "a.html"), image_mode="external")
    distillpdf.Pdf.open(FIGURES).to_markdown(str(m_dir / "a.md"), image_mode="external")
    h_imgs = sorted(os.listdir(h_dir / "img"))
    m_imgs = sorted(os.listdir(m_dir / "img"))
    assert h_imgs == m_imgs and h_imgs


def test_images_drop_emits_placeholder():
    """image_mode="drop" drops inline base64 images and replaces each with a
    `<image N>` placeholder, while keeping the surrounding <figure>/caption."""
    on = distillpdf.Pdf.open(FIGURES).to_html(return_string=True)
    off = distillpdf.Pdf.open(FIGURES).to_html(image_mode="drop", return_string=True)
    assert "data:image" in on and "<img " in on, "fixture expected to inline an image"
    assert "data:image" not in off and "<img " not in off, 'image_mode="drop" still inlined an image'
    assert "<image 1>" in off, "expected a numbered <image N> placeholder"
    # the figure wrapper survives, only the pixel payload is swapped out
    assert "<figure" in off


def test_toc_false_omits_nav_but_keeps_anchors():
    """to_html(toc=False) drops the <nav> table of contents while keeping heading id
    anchors — so links and toc()/section() still resolve."""
    d = distillpdf.Pdf.open(HEADINGS)
    on = d.to_html(return_string=True)
    off = d.to_html(toc=False, return_string=True)
    assert "<nav>" in on, "fixture expected to produce a TOC"
    assert "<nav>" not in off, "toc=False still emitted a <nav>"
    # heading anchors survive (so #sec-… links and section() keep working)
    assert 'id="sec-' in off
    # the parsed-outline API is independent of the toc flag
    assert len(d.toc()) >= 1
    assert d.section("methods") is not None


def test_toc_and_section_types():
    d = distillpdf.Pdf.open(HEADINGS)
    toc = d.toc()
    assert toc and all(len(e) == 4 for e in toc), "toc tuple shape wrong"
    lvl, title, page, anchor = toc[0]
    assert isinstance(lvl, int) and isinstance(title, str) and isinstance(page, int)
    assert distillpdf.Pdf.open(HEADINGS).section("methods") is not None
