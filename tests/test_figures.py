"""Figure handling on an owned fixture (figures.pdf): a raster image is wrapped in a
<figure> with its <figcaption>, a vector chart is transcoded to inline <svg> (with its
rotated axis label rendered as rotated <text>) and captioned, every <img> sits inside a
<figure>, and an inline 'Figure N ...' cross-reference is NOT captured as a caption."""
import re

from _fixtures import GT, html, text

NAME = "figures.pdf"
G = GT[NAME]


def _figures(h):
    return re.findall(r"<figure\b.*?</figure>", h, re.DOTALL)


def test_two_figures_emitted():
    assert len(_figures(html(NAME))) == G["n_figures"], "wrong number of <figure> blocks"


def test_raster_figure_with_caption():
    fig = next((f for f in _figures(html(NAME)) if "<img" in f), None)
    assert fig, "no <figure> containing an <img>"
    assert "data:image" in fig, "raster image not inlined as a data URI"
    cap = re.search(r"<figcaption>(.*?)</figcaption>", fig, re.DOTALL)
    assert cap and G["fig1_caption"][:30] in re.sub(r"\s+", " ", cap.group(1)), \
        "raster figure not paired with Figure 1 caption"


def test_vector_figure_svg_with_caption():
    fig = next((f for f in _figures(html(NAME)) if "<svg" in f), None)
    assert fig, "vector chart not transcoded to inline <svg>"
    assert "<path" in fig, "svg has no vector paths"
    cap = re.search(r"<figcaption>(.*?)</figcaption>", fig, re.DOTALL)
    assert cap and G["fig2_caption"][:30] in re.sub(r"\s+", " ", cap.group(1)), \
        "vector figure not paired with Figure 2 caption"


def test_rotated_axis_label_in_svg():
    fig = next((f for f in _figures(html(NAME)) if "<svg" in f), "")
    m = re.search(r'<text[^>]*transform="rotate\([^)]+\)"[^>]*>(.*?)</text>', fig)
    assert m, "rotated axis title not rendered as a rotated <text>"
    assert G["rotated_label"] in m.group(1), "rotated label text wrong"


def test_all_images_inside_a_figure():
    h = html(NAME)
    bare = re.findall(r"<img\b", h)
    in_fig = re.findall(r"<figure\b[^>]*>(?:(?!</figure>).)*?<img\b", h, re.DOTALL)
    assert len(bare) == len(in_fig), "an <img> is not wrapped in a <figure>"


def test_inline_xref_not_a_caption():
    h = html(NAME)
    caps = [re.sub(r"\s+", " ", c) for c in re.findall(r"<figcaption>(.*?)</figcaption>", h, re.DOTALL)]
    assert not any(G["inline_xref"][:20] in c for c in caps), \
        "inline 'As shown in Figure 1' cross-reference captured as a caption"
    assert G["inline_xref"][:20] in text(h), "inline cross-reference text lost"
