"""Locks for Phase 5 figure recovery (src/html.rs is_dotleader_toc + caption-aware weak
vector promotion; src/vector.rs strong/weak cluster partition).

  - List-of-Figures dot-leader entries ("Figure N: title …… 95") must NOT become <figure>
    shells (single-line AND title-wraps-then-leaders variants); the text still flows as prose.
  - A small vector diagram below the strong figure bar, WITH a caption, IS recovered as a
    <figure> containing <svg>/<path> (caption-aware promotion).
  - Precision: incidental tiny marks with NO adjacent caption are never promoted (zero
    figures); the same diagram WITH a caption IS promoted (positive control) — so the gate
    discriminates rather than blanket-rejecting.

Each assertion would fail on the pre-change baseline (LoF entries emitted empty figures;
small diagrams were dropped).
"""
import re

from _fixtures import GT, html, text


def _figures(h):
    return re.findall(r"<figure\b.*?</figure>", h, re.DOTALL)


# ----- List-of-Figures dot-leader suppression -----
def test_list_of_figures_not_emitted_as_figures():
    h = html("lof_dotleader.pdf")
    g = GT["lof_dotleader.pdf"]
    assert len(_figures(h)) == g["n_figures"], f"LoF entries leaked as figures: {_figures(h)}"
    # The entry text is not lost — it flows into the body as ordinary prose.
    body = text(h)
    assert g["entry_text"] in body, "single-line LoF entry text lost"
    assert g["wrapped_text"] in body, "wrapped LoF entry text lost"


# ----- Caption-aware small-vector recovery -----
def test_small_vector_with_caption_recovered():
    h = html("small_vector_fig.pdf")
    g = GT["small_vector_fig.pdf"]
    figs = _figures(h)
    assert len(figs) == g["n_figures"], f"expected {g['n_figures']} figure, got {len(figs)}"
    fig = figs[0]
    assert "<svg" in fig and "<path" in fig, "small diagram not recovered as inline SVG"
    assert "Figure 1" in fig, "caption not linked to the recovered figure"


# ----- Rotated raster placement -----
def test_rotated_raster_image_keeps_rotation():
    """An image placed rotated 90° (a sideways axis label flattened to a bitmap) must be
    emitted with a matrix transform so it renders rotated, not stretched into an axis-aligned
    box. Locks the img.rs rotation-aware placement + composite_svg transform."""
    h = html("rotated_image.pdf")
    figs = _figures(h)
    assert figs, "no figure emitted for the rotated-image chart"
    blob = "".join(figs)
    assert re.search(r'<image\b[^>]*transform="matrix\(', blob), \
        "rotated raster emitted without a transform (would render stretched/mangled)"


# ----- Precision gate: no spurious promotion without a caption -----
def test_no_spurious_figures_without_caption():
    h = html("no_spurious_figs.pdf")
    g = GT["no_spurious_figs.pdf"]
    # Page 1 marks have no caption → no figure may be promoted; page 2 control has a caption
    # → exactly one figure. So the whole doc must have exactly the control's count.
    assert len(_figures(h)) == g["total_figures"], (
        f"expected {g['total_figures']} figure(s) total (control only), got {len(_figures(h))}")


# ----- A label grid must not delete the map it labels -----
def test_uncaptioned_map_survives_its_own_label_grid():
    """`map_label_grid.pdf` is a controlled A/B for the `not_in_table` proportionality guard.

    Both pages carry the SAME 4x4 grid of short place names, and on both the grid is what
    the table detector sees. The ink differs: page 1 is a map (Bézier coastline + slanted
    borders), page 2 is a ruled table (horizontal and vertical rules only).

    A table that covers a small corner of a figure cannot BE that figure's ink, so on page 1
    the map must survive; on page 2 the table really does own its region and no figure may
    be emitted. Pre-fix, ANY overlapping table deleted the vector, so page 1 emitted zero
    figures — which is how `geology_usgs_fs.pdf` p1's cover map went missing while the page
    still emitted an `<svg>` for the banner logo above it.
    """
    h = html("map_label_grid.pdf")
    g = GT["map_label_grid.pdf"]
    figs = _figures(h)
    total = g["page1_figures"] + g["page2_figures"]
    assert len(figs) == total, f"expected {total} figure(s) (the map only), got {len(figs)}"
    svg = "".join(f for f in figs if "<svg" in f)
    assert "<path" in svg, "the map came back without its ink"
    assert "C" in re.sub(r"[^A-Z]", "", re.search(r'd="([^"]+)"', svg).group(1)), \
        "the recovered figure is not the curved map"
    # Both label grids still reach the reader as tables — the guard adds a figure, it never
    # removes a table.
    assert h.count("<table") == 2, f"a table was lost: {h.count('<table')} remain"
