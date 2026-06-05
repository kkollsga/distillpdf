"""The to_html() document contract, asserted across EVERY owned fixture: valid charset
+ a single lean stylesheet, well-formed balanced markup, one <section> per page, no
presentational bloat (<script>/class=), thin markup-to-text ratio, and — across the
whole owned set — coverage of each semantic feature the pipeline emits."""
import os
import re

import distillpdf
import pytest

import _htmlcheck as hc

OWNED = hc.owned_pdfs()
IDS = [os.path.basename(p) for p in OWNED]


@pytest.fixture(scope="module")
def htmls():
    return {os.path.basename(p): hc.html_for(p) for p in OWNED}


def test_owned_set_nonempty():
    assert len(OWNED) >= 10, f"expected the full owned fixture set, found {len(OWNED)}"


@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_well_formed(path):
    ok, errs = hc.well_formed(hc.html_for(path))
    assert ok, f"{os.path.basename(path)}: malformed HTML: {errs[:4]}"


@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_section_per_page(path):
    h = hc.html_for(path)
    # Count PAGE sections only — the front-matter <header> may carry its own
    # `<section id="abstract">`, which is not a page section.
    n_sec = len(re.findall(r"<section data-page=", h))
    n_pg = distillpdf.Pdf.open(path).page_count()
    assert n_sec == n_pg, f"{os.path.basename(path)}: {n_sec} page <section> vs {n_pg} pages"


@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_shell_and_no_bloat(path):
    h = hc.html_for(path)
    assert '<meta charset="utf-8">' in h, "missing UTF-8 charset"
    assert "<script" not in h, "emitted a <script> (not semantic HTML)"
    assert "class=" not in h, "emitted class= attributes (presentational)"
    assert len(re.findall(r"<style\b", h)) <= 1, "more than one <style> block"


@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_markup_thinness(path):
    h = hc.html_for(path)
    pt = hc.plain_bytes(h)
    if pt < 200:
        return  # too little text for a meaningful ratio
    ratio = hc.markup_bytes(h) / pt
    assert ratio <= 2.6, f"{os.path.basename(path)}: markup ratio {ratio:.2f} (bloated)"


def test_feature_coverage(htmls):
    """Across the owned set, every semantic feature the pipeline produces appears at
    least once — so a regression that stops emitting one is caught."""
    blob = "\n".join(htmls.values())
    features = {
        "h1 title": r"<h1\b",
        "h2 section": r"<h2\b",
        "deep heading": r"<h[34]\b",
        "bold": r"<b>",
        "table": r"<table\b",
        "raster image": r"<img\b[^>]*data:image",
        "vector svg": r"<svg\b",
        "unordered list": r"<ul\b",
        "ordered list": r"<ol\b",
        "hyperlink": r'<a href=',
        "figure": r"<figure\b",
        "figcaption": r"<figcaption\b",
        "toc nav": r"<nav\b",
    }
    missing = [name for name, pat in features.items() if not re.search(pat, blob)]
    assert not missing, f"features never emitted across the owned set: {missing}"
