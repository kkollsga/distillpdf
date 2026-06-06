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


# ----- Precision gate: no spurious promotion without a caption -----
def test_no_spurious_figures_without_caption():
    h = html("no_spurious_figs.pdf")
    g = GT["no_spurious_figs.pdf"]
    # Page 1 marks have no caption → no figure may be promoted; page 2 control has a caption
    # → exactly one figure. So the whole doc must have exactly the control's count.
    assert len(_figures(h)) == g["total_figures"], (
        f"expected {g['total_figures']} figure(s) total (control only), got {len(_figures(h))}")
