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


def test_two_figures_one_page_captions_not_swapped():
    """Regression: with a raster figure above a vector figure on the SAME page, each
    caption binds to the figure it sits under (caption→figure anchoring by nearest
    edge, not by top edge which swapped them)."""
    g = GT["figures_onepage.pdf"]
    h = html("figures_onepage.pdf")
    figs = re.findall(r"<figure\b.*?</figure>", h, re.DOTALL)
    img_fig = next((f for f in figs if "<img" in f), "")
    svg_fig = next((f for f in figs if "<svg" in f), "")
    img_cap = re.search(r"<figcaption>(.*?)</figcaption>", img_fig, re.DOTALL)
    svg_cap = re.search(r"<figcaption>(.*?)</figcaption>", svg_fig, re.DOTALL)
    assert img_cap and g["fig1_caption"][:30] in re.sub(r"\s+", " ", img_cap.group(1)), \
        "raster figure did not get Figure 1's caption"
    assert svg_cap and g["fig2_caption"][:30] in re.sub(r"\s+", " ", svg_cap.group(1)), \
        "vector figure did not get Figure 2's caption"


def test_inline_xref_not_a_caption():
    h = html(NAME)
    caps = [re.sub(r"\s+", " ", c) for c in re.findall(r"<figcaption>(.*?)</figcaption>", h, re.DOTALL)]
    assert not any(G["inline_xref"][:20] in c for c in caps), \
        "inline 'As shown in Figure 1' cross-reference captured as a caption"
    assert G["inline_xref"][:20] in text(h), "inline cross-reference text lost"


def test_dense_vector_page_emits_both_figures():
    """A dense vector page (a 12-rule grid plus ~300 scatter marks, ~700 content-stream
    operators) must render both figures as inline <svg>. Under the old 60k operation cap
    such a page could be dropped whole — the walk returned no figures at all rather than
    the figures it had already painted."""
    g = GT["dense_vector.pdf"]
    h = html("dense_vector.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == g["n_figures"], f"expected {g['n_figures']} <svg>, got {len(svgs)}"
    assert sum(s.count("<path") for s in svgs) > g["scatter_paths"], "scatter marks missing from the svg"


def test_unfiltered_form_vector_ink_survives():
    """The vector half of the same defect: five filled bars painted inside a Form XObject
    whose stream carries no /Filter. The walker decoded the form as zero bytes, so the whole
    figure vanished from the render while `extract_images` (which carries the raw-bytes
    fallback) still saw the form."""
    g = GT["unfiltered_form.pdf"]
    h = html("unfiltered_form.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == 1, f"expected the bars to render as one <svg>, got {len(svgs)}"
    assert svgs[0].count("<path") == g["n_bars"], \
        f"expected {g['n_bars']} bars, got {svgs[0].count('<path')}"
    cap = re.search(r"<figcaption>(.*?)</figcaption>", h, re.DOTALL)
    assert cap and g["caption"][:30] in re.sub(r"\s+", " ", cap.group(1)), \
        "the unfiltered-form figure did not get its caption"


def test_indirect_mediabox_sizes_the_figure_against_the_real_page():
    """A page whose inherited ``/MediaBox`` extents are INDIRECT references must be measured,
    not guessed. The direct-only number reader turned `[0 0 9 0 R 10 0 R]` into a zero-width
    box, so the page fell back to 612pt and every figure on it was scaled as the wrong share
    of the page (here 82% instead of the 50% the grid really spans)."""
    g = GT["indirect_mediabox.pdf"]
    h = html("indirect_mediabox.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == g["n_figures"], f"expected {g['n_figures']} <svg>, got {len(svgs)}"
    # A figure renders at 1.5x its share of the page width, clamped to 100%. The grid spans
    # 504 of 1008pt, so ~76%; measured against the 612pt guess it computes >100% and clamps
    # to a full-width figure — visibly the wrong size.
    m = re.search(r"width:([\d.]+)%", svgs[0])
    assert m, f"no percentage width on the svg: {svgs[0][:200]}"
    share = float(m.group(1))
    expected = 150.0 * g["figure_width"] / g["page_width"]
    assert abs(share - expected) < 6.0, \
        f"figure sized {share:.1f}% of the body, expected ~{expected:.0f}% (the 612pt guess clamps to 100%)"
    assert share < 99.0, "the figure is not full-width — a 100% clamp means the page was guessed"


def test_cropbox_only_page_still_extracts():
    """Page 2 of the same fixture states only a /CropBox — the spec's page-box fallback. It
    must read normally rather than being measured as US-Letter."""
    g = GT["indirect_mediabox.pdf"]
    assert g["crop_page_text"] in text(html("indirect_mediabox.pdf"))


def test_indirect_extgstate_alpha_does_not_hide_the_figure():
    """``/GA`` is ``<< /ca 10 0 R /CA 11 0 R >>``. Read with the direct-only number reader
    both alphas came back 0.0 — below the "effectively invisible" threshold — so every bar
    and axis rule painted under ``/GA gs`` was dropped and the whole bar chart vanished from
    the render (0 <svg>, 0 <path>). The authored opacities must survive to the SVG."""
    g = GT["indirect_numbers.pdf"]
    h = html("indirect_numbers.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == g["n_figures"], f"expected {g['n_figures']} <svg>, got {len(svgs)}"
    assert svgs[0].count("<path") == g["n_paths"], \
        f"expected {g['n_paths']} paths, got {svgs[0].count('<path')}"
    assert f'fill-opacity="{g["fill_alpha"]}"' in svgs[0], "the indirect /ca did not reach the render"
    assert f'stroke-opacity="{g["stroke_alpha"]}"' in svgs[0], "the indirect /CA did not reach the render"


def test_a_page_with_no_resources_still_draws_its_paths():
    """``m``/``l``/``c``/``re`` and the ``f``/``S`` that paint them name no resource, but the
    vector walk returned empty the moment a page's whole ``/Resources`` chain was — so a
    page that draws its figure with nothing but path operators lost all of it.

    ``no_resources_paths.pdf`` is the controlled A/B: two pages, identical content streams,
    the sole difference an empty ``/Resources << >>`` on page 2. Page 1 emitted 0 ``<svg>``
    and page 2 emitted 1; both must now emit exactly one, with all eight bars."""
    g = GT["no_resources_paths.pdf"]
    h = html("no_resources_paths.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == len(g["svgs_per_page"]), \
        f"both pages must render their bars, got {len(svgs)} <svg>"
    for i, svg in enumerate(svgs, 1):
        assert svg.count("<path") == g["paths_per_page"], \
            f"page {i}: expected {g['paths_per_page']} bars, got {svg.count('<path')}"
        assert "fill-opacity" not in svg, \
            f"page {i}: no /ExtGState anywhere, so the ink must paint at the default full opacity"


def test_composited_figure_keeps_the_streams_paint_order():
    """``composite_svg`` used to emit every raster first and all the vector ink after it —
    a grouping that is right only when the stream painted that way. A figure that paints an
    OPAQUE panel over a raster then rendered as a bare panel: the ``<image>`` was in the SVG
    and covered by ink that, in the source, sat behind it.

    ``paint_order.pdf`` is the controlled A/B: two geometrically identical figures, the sole
    difference the order of two operators. The top one paints the raster then the grey panel
    (panel must win); the bottom one paints the panel then the raster (raster must win)."""
    g = GT["paint_order.pdf"]
    h = html("paint_order.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == g["figures"], f"expected {g['figures']} composited <svg>, got {len(svgs)}"
    panel = f'fill="{g["panel_fill"]}"'
    for i, svg in enumerate(svgs):
        assert "<image " in svg, f"figure {i}: the raster must be composited into the <svg>"
        assert panel in svg, f"figure {i}: the opaque panel must be in the <svg>"
        img_at, panel_at = svg.index("<image "), svg.index(panel)
        if i == 0:
            assert img_at < panel_at, "raster painted FIRST must render behind the panel"
        else:
            assert img_at > panel_at, "raster painted LAST must render on top of the panel"
