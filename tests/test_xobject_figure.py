"""Regression lock for the Form-XObject text recursion (commit 864694e).

`extract_spans` recurses into Form XObjects, so text drawn INSIDE a full-page form overlay
(the e-filing / iText layout) reaches the page span stream. That recovery is wanted for
body prose — but a CAPTIONED diagram drawn in the same form carries internal node labels
laid out as a regular grid, which must NOT:

  1. be read as a data <table> that suppresses the figure (the figure must render as <svg>),
  2. leak into the body as <p> / <h*> headings (they belong on the figure),

while the surrounding body prose must still flow as prose, appearing exactly once.

The fixture (tests/gen_fixtures.py: gen_xobject_figure) draws the WHOLE page — body prose
above and below, plus a boxed three-column diagram with a "Figure 1:" caption — inside one
Form XObject. On the pre-fix tree the diagram came out as an empty <figure> (its labels
captured into spurious tables/headings); these asserts would fail there.
"""
import re

from _fixtures import html, text

DOC = "xobject_figure.pdf"
BODY_ABOVE = "assembled as a single full-page form overlay"
BODY_BELOW = "feeds the output layer"
FIG_LABELS = ("Image Encoder", "Text Encoder", "Output Layer")


def _figures(h):
    return re.findall(r"<figure\b.*?</figure>", h, re.DOTALL)


def _headings(h):
    return [re.sub("<[^>]+>", "", m).strip()
            for m in re.findall(r"<h[1-6]\b[^>]*>(.*?)</h[1-6]>", h, re.DOTALL)]


def test_figure_renders_with_svg():
    """The captioned diagram inside the form renders as exactly one <figure> with a graphic,
    not an empty shell — its label grid must not be mistaken for a data table that drops it."""
    h = html(DOC)
    figs = _figures(h)
    assert len(figs) == 1, f"expected one figure, got {len(figs)}: {figs}"
    assert "<svg" in figs[0] or "<img" in figs[0], f"figure has no graphic: {figs[0]}"
    # No spurious data table from the diagram's node-label grid.
    assert "<table" not in h, "diagram label grid leaked as a <table>"


def test_body_prose_flows_once():
    """Body text set inside the form flows as prose, each sentence appearing exactly once
    (no duplication from double-walking the form, no loss)."""
    body = text(h := html(DOC))
    assert body.count(BODY_ABOVE) == 1, f"body-above count={body.count(BODY_ABOVE)}"
    assert body.count(BODY_BELOW) == 1, f"body-below count={body.count(BODY_BELOW)}"
    # The body prose lives in <p>, not swallowed into the figure or a heading.
    paras = " ".join(re.sub("<[^>]+>", " ", p) for p in re.findall(r"<p\b[^>]*>(.*?)</p>", h, re.DOTALL))
    assert BODY_ABOVE in re.sub(r"\s+", " ", paras)
    assert BODY_BELOW in re.sub(r"\s+", " ", paras)


def test_figure_labels_do_not_leak_into_prose_or_headings():
    """The diagram's internal labels belong on the figure (SVG <text>), never in the body
    prose or as section headings. `text()` drops <svg>, so a label showing there is a leak."""
    h = html(DOC)
    body = text(h)  # SVG stripped
    for lab in FIG_LABELS:
        assert lab not in body, f"figure label {lab!r} leaked into body prose"
    heads = _headings(h)
    for lab in FIG_LABELS:
        assert not any(lab in hd for hd in heads), f"figure label {lab!r} promoted to a heading: {heads}"
