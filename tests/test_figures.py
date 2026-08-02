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


def test_a_rotated_page_emits_its_figure_in_display_orientation():
    """``/Rotate`` — the degrees a viewer turns a page CLOCKWISE before showing it — was read
    nowhere in the crate, so a landscape table authored on a ``/Rotate 90`` page emitted a
    sideways ``<svg>`` with its text running bottom-to-top.

    ``rotated_pages.pdf`` is the controlled A/B: four pages, ONE byte-identical content
    stream, only ``/Rotate`` differs (0/90/180/270). End to end, that must show up as:

      * the viewBox transposing on a quarter turn (200x300 upright, 300x200 turned);
      * the 20x20 corner marker moving to the corner the turn puts it in;
      * both labels surviving, with the page's turn COMPOSED onto each span's own baseline
        angle — ``Alpha`` (drawn upright) turns, ``Beta`` (drawn at 90 deg) comes out upright
        on the ``/Rotate 90`` page. Overwriting instead of composing double-rotates one.
    """
    g = GT["rotated_pages.pdf"]
    h = html("rotated_pages.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == len(g["rotations"]), f"one <svg> per page, got {len(svgs)}"
    # Local `d` of the corner marker, and the emitted rotate() degrees per label.
    want = [
        (0, "M0 300L20 300L20 280L0 280Z", (200, 300), {"Alpha": None, "Beta": "rotate(-90 "}),
        (90, "M0 0L0 20L20 20L20 0Z", (300, 200), {"Alpha": "rotate(90 ", "Beta": None}),
        (180, "M200 0L180 0L180 20L200 20Z", (200, 300), {"Alpha": "rotate(180 ", "Beta": "rotate(90 "}),
        (270, "M300 200L300 180L280 180L280 200Z", (300, 200), {"Alpha": "rotate(270 ", "Beta": "rotate(180 "}),
    ]
    for svg, (rot, marker, (w, h_), labels) in zip(svgs, want):
        vb = re.search(r'viewBox="([-\d. ]+)"', svg)
        assert vb, f"/Rotate {rot}: no viewBox"
        _, _, vw, vh = (float(v) for v in vb.group(1).split())
        # The viewBox pads the content box by 4pt on every side and grows to the labels,
        # so assert the ORIENTATION, which is what the turn decides.
        assert (vw > vh) == (w > h_), \
            f"/Rotate {rot}: viewBox {vw}x{vh} is not oriented like the displayed {w}x{h_} figure"
        assert marker in svg, f"/Rotate {rot}: corner marker {marker} absent"
        for text, want_rot in labels.items():
            el = next((c for c in svg.split("<text ") if f">{text}<" in c), None)
            assert el, f"/Rotate {rot}: label {text} lost"
            if want_rot is None:
                assert "rotate(" not in el, f"/Rotate {rot}: {text} must be upright, got {el}"
            else:
                assert want_rot in el, f"/Rotate {rot}: {text} wants {want_rot}, got {el}"


def test_separation_tints_render_as_colours_not_as_grey_levels():
    """``scn`` in a ``Separation``/``DeviceN`` space carries a TINT — "how much of this
    colorant" — which means nothing until it passes through the space's tint transform. The
    vector walk had no ``cs``/``CS`` arm at all, so a 1-operand ``scn`` was read as a grey
    level: ``.1 scn`` painted ``#1a1a1a`` and a pale (198,198,224) table header rendered
    near-BLACK.

    ``separation.pdf`` is one page per path through the evaluator:

      page 1  a Type 2 exponential ramp to the audit's own (198,198,224) — pale, and tint 1
              is exactly that colour;
      page 2  a 2-component ``DeviceN`` through a Type 0 sampled grid — which also pins the
              sample order end to end (``1 0 scn`` red, ``0 1 scn`` green, never swapped);
      page 3  a Type 4 PostScript calculator, deliberately NOT evaluated — the tint degrades
              to INK COVERAGE (``1 - t``), so a light tint stays light instead of inverting.
    """
    g = GT["separation.pdf"]
    h = html("separation.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == len(g["pages"]), f"one figure per page, got {len(svgs)}"
    sep, devn, type4 = svgs

    # Page 1: every spot fill is pale, and the solid tint is the transform's own colour.
    assert f'fill="{g["solid_tint_rgb"]}"' in sep, "tint 1 must be the tint transform's colour"
    fills = re.findall(r'fill="#([0-9a-f]{6})"', sep)
    assert len(fills) == 8, f"all 8 spot fills must survive, got {fills}"
    for hexval in fills:
        rgb = [int(hexval[i:i + 2], 16) for i in (0, 2, 4)]
        assert min(rgb) >= 190, f"spot fill #{hexval} is not pale — the grey-level reading is back"

    # Page 2: two colorants through a sampled transform, in the right order.
    for want in ("#ff0000", "#00ff00", "#0000ff", "#ffffff"):
        assert f'fill="{want}"' in devn, f"DeviceN tint pair must evaluate to {want}"

    # Page 3: the unevaluable transform degrades to coverage, not to an inversion.
    for tint, want in g["type4_coverage"].items():
        assert f'fill="{want}"' in type4, f"Type 4 tint {tint} must degrade to {want}"
    for wrong in g["grey_level_misreadings"]:
        assert wrong not in sep and wrong not in type4, \
            f"the grey-level misreading {wrong} is still emitted"


def test_a_transparency_groups_alpha_survives_into_the_svg():
    """A form XObject with ``/Group << /S /Transparency >>`` is composited as a unit: the
    ``ca``/``CA`` in force at its ``Do`` applies to the group's RESULT, and the group's own
    state starts opaque (§11.4.7.2, §11.6.6). The walk inherited the caller's alpha into the
    child state instead, where the child's own first ``gs`` — ``ca 1 CA 1``, which inside a
    group is just the initial value spelled out — overwrote it, so every element painted
    fully opaque. ``attention_1706.03762`` p13 emitted **zero** opacity attributes for 615
    weighted cells, and its 561 ``ca 0`` cells painted solid instead of vanishing.

    The second half: a paint below the old ``ALPHA_HIDDEN`` 0.04 bar is now rendered FAINT,
    not deleted. In an attention or density figure the alpha *is* the quantity, so
    thresholding it away removes the data."""
    g = GT["alpha_groups.pdf"]
    h = html("alpha_groups.pdf")
    svgs = re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    assert len(svgs) == 1, f"the box is one figure, got {len(svgs)}"
    svg = svgs[0]
    for want in g["group_opacities"] + [g["plain_form_opacity"]]:
        assert f'fill-opacity="{want}"' in svg, f"opacity {want} missing from {svg}"
    # `ca 0` is not faint, it is absent — and no faint paint is rounded away to it either.
    assert 'fill-opacity="0"' not in svg, "a transparent or faint paint reached the output as 0"
    assert len(re.findall(r"<path\b", svg)) == g["paths"], f"expected {g['paths']} paths in {svg}"


def test_dashed_strokes_render_dashed():
    """The ``d`` operator had no arm in the vector walk, so dash state was never read and
    every dashed stroke came out SOLID. That is destructive, not cosmetic: all six DAG
    figures in ``econ_EM_2606_02234`` mark **unobserved** variables by dashing the node, and
    say so in their captions; ``cs_DS_2606_02492`` p24 describes "edges shown in dashed light
    blue". A solid stroke silently asserts the opposite of what the figure means.

    ``dashes.pdf``'s five rules differ only in dash state, so every difference is ``d``'s."""
    g = GT["dashes.pdf"]
    svgs = re.findall(r"<svg\b.*?</svg>", html("dashes.pdf"), re.DOTALL)
    assert len(svgs) == 1, f"the rules and their frame are one figure, got {len(svgs)}"
    svg = svgs[0]
    assert len(re.findall(r"<path\b", svg)) == g["paths"]
    for want in (d for d in g["dasharrays"] if d):
        assert f'stroke-dasharray="{want}"' in svg, f"pattern {want} missing from {svg}"
    for want in (o for o in g["dashoffsets"] if o):
        assert f'stroke-dashoffset="{want}"' in svg, f"phase {want} missing from {svg}"
    # Solid, `[] d` (reset) and `[0 0] d` (invalid) all carry no dash whatsoever.
    n = sum(1 for d in g["dasharrays"] if d)
    assert len(re.findall(r"stroke-dasharray", svg)) == n, f"only {n} strokes may dash: {svg}"


# ----- Claim margins: a figure takes its own labels and nothing else -----
def _svg_text(h):
    return " ".join(
        " ".join(re.sub(r"<[^>]+>", " ", s).split())
        for s in re.findall(r"<svg\b.*?</svg>", h, re.DOTALL)
    )


def _body_text(h):
    return " ".join(re.sub(r"<[^>]+>", " ", re.sub(r"<svg\b.*?</svg>", " ", h, flags=re.DOTALL)).split())


def test_figure_claims_its_labels_and_nothing_else():
    """`caption_bleed.pdf` — three neighbours a figure used to swallow, one per audit case.

    Page 2: the running page header above the diagram (`arxiv_nerf.pdf` p18) and the
    continuation lines of its caption (`cs_LG_2606_02576` p1, `cs_DS_2606_02492` p34).
    Page 3: a data row whose numeric half is axis-shaped and whose label half is not
    (`econ_EM_2606_02234.pdf` p25 rendered the numbers with no row context).

    The diagram's own labels must still be on the SVG, and every trap must be in the body —
    exactly once, which is the half that also has to hold: refusing a row on the figure side
    while the body side still drops it as a label loses the row entirely.
    """
    from _fixtures import GT as _GT
    h = html("caption_bleed.pdf")
    g = _GT["caption_bleed.pdf"]
    svg, body = _svg_text(h), _body_text(h)
    for lbl in g["svg_labels"]:
        assert lbl in svg, f"the diagram lost its own label {lbl!r}"
    for trap in g["must_stay_in_body"]:
        assert trap not in svg, f"{trap!r} bled into the figure"
        assert trap in body, f"{trap!r} was emitted NEITHER on the figure nor in the body"


def test_char_spacing_inside_q_does_not_leak(): 
    """`textstate_q.pdf` — the text state is graphics state, so `Q` restores `Tc`.

    A leak widens every later glyph advance; on a label drawn one `Tj` per glyph and then
    repositioned by an absolute `Td`, the drift transposes the last two letters.
    """
    from _fixtures import GT as _GT, doc as _doc
    g = _GT["textstate_q.pdf"]
    t = _doc("textstate_q.pdf").extract_text()
    assert t.count(g["word"]) == g["occurrences"], f"expected {g['word']!r} twice, got {t!r}"


def test_a_turned_page_emits_its_body_prose():
    """``rotated_body.pdf`` — a ``/Rotate`` page whose content is TEXT, nothing else.

    ``layout::lines_of`` opened by discarding every rotated span, which on a ``/Rotate
    90``/``270`` page is *every* span: such a page emitted no prose at all. It stayed hidden
    because the one corpus document with turned pages fills them with a ruled table, whose
    vector figure carried the words into the output as SVG labels — so no count could see it.

    Four pages, one per rotation, showing the READER the identical page. The extracted body
    must therefore be identical too, in reading order — and the spine label, which is upright
    in page space but sideways to the reader, must still stay out of it.
    """
    from _fixtures import GT as _GT
    g = _GT["rotated_body.pdf"]
    h = html("rotated_body.pdf")
    pages = re.findall(r'<section data-page="(\d+)"[^>]*>(.*?)</section>', h, re.DOTALL)
    assert len(pages) == len(g["rotations"]), f"one page per rotation, got {len(pages)}"
    for (_, page), rot in zip(pages, g["rotations"]):
        body = text(page)
        assert g["heading"] in body, f"/Rotate {rot}: the heading is missing from {body!r}"
        at = [body.find(p) for p in g["paragraphs"]]
        for p, i in zip(g["paragraphs"], at):
            assert i >= 0, f"/Rotate {rot}: paragraph missing — {body!r}"
        assert at == sorted(at), f"/Rotate {rot}: paragraphs out of reading order"
    assert g["spine_label"] not in h, "text set sideways to the reader is not body prose"
