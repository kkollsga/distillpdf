"""Section mode — the to_html() default. Logical sections are first-order: every heading
becomes a nested <section id="sec-…"> (id on the wrapper, inner heading/paragraphs bare),
page identity (data-page / id=page-N) is dropped, and the TOC carries no page numbers.
Page mode is the explicit opt-in that restores the per-page structure.
"""
import os
import re

import distillpdf
import pytest

import _htmlcheck as hc
from _fixtures import FIX

HEADINGS = os.path.join(FIX, "headings.pdf")
DEMO = os.path.join(hc.HERE, "demo", "demo.pdf")
OWNED = hc.owned_pdfs()
IDS = [os.path.basename(p) for p in OWNED]


def test_default_mode_is_section():
    h = distillpdf.open(HEADINGS).to_html()  # no mode arg → section
    assert '<section id="sec-' in h
    assert "data-page" not in h and 'id="page-' not in h


def test_page_mode_opt_in():
    h = distillpdf.open(HEADINGS, mode="page").to_html()
    assert 'data-page="' in h and 'id="page-' in h
    assert '<section id="sec-' not in h  # page mode keeps the id on the heading


def test_section_wrapper_carries_id_inner_bare():
    h = distillpdf.open(DEMO).to_html()
    # the sec-… id sits on the <section>; the heading it wraps is bare
    m = re.search(r'<section id="sec-[^"]+">\s*<h\d\b([^>]*)>', h)
    assert m, "expected a <section id=…> directly wrapping its heading"
    assert "id=" not in m.group(1), "inner heading should be bare (id is on the wrapper)"


def test_toc_drops_pages_in_section_mode():
    entries = distillpdf.open(HEADINGS).toc()
    assert entries and all(pg == 0 for (_lvl, _t, pg, _id) in entries)
    # page mode still carries real page numbers
    pentries = distillpdf.open(HEADINGS, mode="page").toc()
    assert any(pg >= 1 for (_lvl, _t, pg, _id) in pentries)


def test_section_returns_balanced_block():
    ab = distillpdf.open(DEMO).section("abstract")
    assert ab and ab.startswith('<section id="sec-abstract">')
    assert ab.count("<section") == ab.count("</section>")  # balanced element
    assert ab.rstrip().endswith("</section>")
    assert "Abstract" in ab


def test_nav_links_resolve_to_section_wrappers():
    h = distillpdf.open(HEADINGS).to_html()
    hrefs = re.findall(r'<a href="#(sec-[^"]+)"', h)
    assert hrefs, "expected a section TOC with sec-… links"
    for href in hrefs:
        assert f'<section id="{href}">' in h, f"nav link #{href} has no section target"


def test_flags_compose_with_section_mode():
    h = distillpdf.open(DEMO, images=False, toc=False).to_html()
    assert "<nav>" not in h          # toc=False
    assert "data:image" not in h     # images=False
    assert '<section id="sec-' in h  # still section-structured


def test_invalid_mode_raises():
    with pytest.raises(ValueError):
        distillpdf.open(HEADINGS, mode="bogus")


@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_section_mode_well_formed(path):
    h = distillpdf.Pdf.open(path, mode="section").to_html()
    ok, errs = hc.well_formed(h)
    assert ok, f"{os.path.basename(path)}: malformed section-mode HTML: {errs[:4]}"
    assert h.count("<section") == h.count("</section>"), "unbalanced <section> nesting"
    assert "data-page" not in h and 'id="page-' not in h, "page identity leaked"
