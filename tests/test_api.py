"""The Pdf public API surface, exercised on owned fixtures: open/from_bytes, page_count,
plain-text extraction (whole-doc + per-page), and the structured extractors
(fonts/images/tables/links) plus the diagnostic span dumps — shapes, types, and basic
invariants. Guards the API contract that downstream callers depend on."""
import io
import os
import re

import distillpdf
import pytest

from _fixtures import FIX, GT

HEADINGS = os.path.join(FIX, "headings.pdf")
FIGURES = os.path.join(FIX, "figures.pdf")
NUMERIC = os.path.join(FIX, "numeric.pdf")
LINKS = os.path.join(FIX, "links.pdf")
FORM_IMAGE = os.path.join(FIX, "form_image.pdf")
FORM_FONT = os.path.join(FIX, "form_font.pdf")
UNDRAWN_IMAGE = os.path.join(FIX, "undrawn_image.pdf")
COLORSPACE_IMAGES = os.path.join(FIX, "colorspace_images.pdf")
CMYK_JPEG = os.path.join(FIX, "cmyk_jpeg.pdf")
DECODE_JPEG = os.path.join(FIX, "decode_jpeg.pdf")
ANNOT_APPEARANCE = os.path.join(FIX, "annot_appearance.pdf")
RULED_BLANK = os.path.join(FIX, "ruled_blank_cells.pdf")


def test_open_and_page_count():
    assert distillpdf.Pdf.open(HEADINGS).page_count() == 1
    assert distillpdf.Pdf.open(LINKS).page_count() == 2


def test_module_level_open():
    """The top-level distillpdf.open()/from_bytes() return a Document (the Rust core plus the
    OCR workflow); for born-digital PDFs it renders identically to the Pdf classmethods, and
    delegates every core method to the wrapped Pdf."""
    doc = distillpdf.open(HEADINGS)
    assert isinstance(doc, distillpdf.Document)
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
    for key in ("page", "index", "width", "height", "color_space", "bits_per_component", "format", "data"):
        assert key in im, f"image dict missing {key!r}"
    assert im["width"] > 0 and im["height"] > 0
    assert isinstance(im["data"], (bytes, bytearray)) and len(im["data"]) > 0


def test_extract_images_resolves_colorspace_and_assembles_a_png():
    """The four colour-space resolution steps that used to leave `color_space` blank —
    a palette, an ICC profile's /N, an indirect /ColorSpace reference, and a name only
    the page's /ColorSpace resources define — plus the PNG assembly that turns a bare
    sample block into a file. Pixel values are the ones the fixture authored."""
    Image = pytest.importorskip("PIL.Image")
    imgs = distillpdf.Pdf.open(COLORSPACE_IMAGES).extract_images()
    assert [(i["color_space"], i["bits_per_component"], i["format"]) for i in imgs] == [
        ("Indexed", 4, "png"),
        ("ICCBased", 8, "png"),
        ("DeviceCMYK", 8, "png"),
        ("ICCBased", 8, "png"),
    ]
    px = [Image.open(io.BytesIO(i["data"])).convert("RGB") for i in imgs]
    assert px[0].size == (4, 2)
    assert px[0].getpixel((0, 0)) == (255, 0, 0) and px[0].getpixel((3, 0)) == (255, 255, 255)
    assert px[1].getpixel((0, 0)) == (10, 20, 30) and px[1].getpixel((1, 1)) == (100, 110, 120)
    assert px[2].getpixel((0, 0)) == (255, 255, 255) and px[2].getpixel((1, 0)) == (0, 255, 255)
    assert px[3].getpixel((0, 0)) == (0, 0, 0) and px[3].getpixel((1, 0)) == (255, 255, 255)


def test_cmyk_jpeg_is_normalized_to_the_authored_colour():
    """A DeviceCMYK JPEG is the one image whose bytes fail *silently*: PIL applies the
    Adobe complement and never sees the PDF's `/Decode`, so the raw stream reads as the
    inverse of the authored colour — the white band came back black."""
    Image = pytest.importorskip("PIL.Image")
    imgs = distillpdf.Pdf.open(CMYK_JPEG).extract_images()
    assert len(imgs) == 1 and imgs[0]["color_space"] == "DeviceCMYK"
    assert imgs[0]["format"] == "png", "CMYK JPEG must be normalized, not passed through"
    im = Image.open(io.BytesIO(imgs[0]["data"])).convert("RGB")
    assert im.size == (96, 48)
    for x, want in ((15, (255, 255, 255)), (47, (0, 255, 255)), (79, (255, 75, 255))):
        got = im.getpixel((x, 24))
        assert max(abs(a - b) for a, b in zip(got, want)) <= 8, f"band x={x}: {got} vs {want}"


def test_cmyk_jpeg_renders_in_the_authored_colour_in_html():
    """The render path had the same silent inversion as the extract path, keyed on the
    Adobe APP14 transform byte instead of `/Decode`: `to_html(image_mode="embed")` embedded
    this fixture's white/cyan/magenta bands as SOLID BLACK. Polarity is `/Decode`'s to
    state (PDF §7.4.8), so the decoder's standalone-JPEG complement is undone unless
    `/Decode` already inverts."""
    Image = pytest.importorskip("PIL.Image")
    import base64
    h = distillpdf.Pdf.open(CMYK_JPEG).to_html(mode="page", return_string=True, image_mode="embed")
    uris = re.findall(r"data:image/\w+;base64,([A-Za-z0-9+/=]+)", h)
    assert len(uris) == 1, "the fixture places exactly one image"
    im = Image.open(io.BytesIO(base64.b64decode(uris[0]))).convert("RGB")
    assert im.size == (96, 48)
    for x, want in ((15, (255, 255, 255)), (47, (0, 255, 255)), (79, (255, 75, 255))):
        got = im.getpixel((x, 24))
        assert max(abs(a - b) for a, b in zip(got, want)) <= 8, f"band x={x}: {got} vs {want}"


def test_gray_and_rgb_jpegs_honour_an_inverting_decode_in_html():
    """The `/Decode` half of the CMYK fix applied to CMYK only: a gray or RGB JPEG whose
    image dict says `/Decode [1 0 …]` was passed through to the `<img>` byte-for-byte, so
    `to_html` rendered the NEGATIVE of the authored colour. The RGB image's array is
    INDIRECT, which the render path's own reader could not follow at all."""
    Image = pytest.importorskip("PIL.Image")
    import base64
    h = distillpdf.Pdf.open(DECODE_JPEG).to_html(mode="page", return_string=True, image_mode="embed")
    uris = re.findall(r"data:image/\w+;base64,([A-Za-z0-9+/=]+)", h)
    assert len(uris) == 2, "the fixture places two images"
    for uri, want in zip(uris, ((215, 215, 215), (55, 225, 165))):
        im = Image.open(io.BytesIO(base64.b64decode(uri))).convert("RGB")
        assert im.size == (64, 48)
        got = im.getpixel((32, 24))
        assert max(abs(a - b) for a, b in zip(got, want)) <= 8, f"{got} vs {want}"


def _html_image_pixels(path, expect):
    """The RGB pixels of every image `to_html(image_mode="embed")` inlines, in page order."""
    Image = pytest.importorskip("PIL.Image")
    import base64
    h = distillpdf.Pdf.open(path).to_html(mode="page", return_string=True, image_mode="embed")
    uris = re.findall(r"data:image/\w+;base64,([A-Za-z0-9+/=]+)", h)
    assert len(uris) == expect, f"{os.path.basename(path)}: expected {expect} inline images, got {len(uris)}"
    assert len(re.findall(r"<img\b", h)) == expect, "one <img> per inlined image"
    return [Image.open(io.BytesIO(base64.b64decode(u))).convert("RGB") for u in uris]


def test_the_render_path_decodes_the_samples_extract_can_decode():
    """`to_html` carried its own, weaker sample decoder than `extract_images()`: 8 bpc only,
    and a channel count guessed from `len(samples) / (w*h)` for any colour space it did not
    model. An Indexed image's palette INDICES therefore rendered as gray levels (authored
    red/blue came out (0,0,0)/(1,1,1)) and a 4-bpc image was dropped outright."""
    idx, gray4 = _html_image_pixels(os.path.join(FIX, "render_samples.pdf"), 2)
    assert idx.size == (2, 1)
    assert idx.getpixel((0, 0)) == (255, 0, 0), "palette entry 0, not gray level 0"
    assert idx.getpixel((1, 0)) == (0, 0, 255), "palette entry 1, not gray level 1"
    assert gray4.getpixel((0, 0)) == (0, 0, 0) and gray4.getpixel((1, 0)) == (255, 255, 255)
    # The colour-space fixture is drawn on its page too, so all four of its rasters — the
    # 4-bpc Indexed one, the raw DeviceCMYK one, and the two that already worked — appear.
    assert len(_html_image_pixels(COLORSPACE_IMAGES, 4)) == 4


def test_a_separation_image_decodes_through_its_tint_transform():
    """An image in a `Separation`/`DeviceN` space carries TINTS, not intensities, and the
    raster stack refused those spaces outright: `extract_images()` reported `format:"raw"`
    (compressed samples, no container) and `to_html` emitted no `<img>` at all. Now the
    space's tint transform — the same evaluator the `scn` fix uses — maps each sample into
    the alternate space, so `separation.pdf` p4's 0/.5/1 tints decode to white, pale
    lavender, and the spot's own (198,198,224) instead of black, mid-grey and white."""
    g = GT["separation.pdf"]
    path = os.path.join(FIX, "separation.pdf")
    spot = [im for im in distillpdf.Pdf.open(path).extract_images() if im["page"] == 4]
    assert len(spot) == 1 and spot[0]["format"] == "png", f"the spot raster must assemble, got {spot}"
    (im,) = _html_image_pixels(path, 1)
    assert im.size == (3, 1)
    for xy, want in g["spot_image_px"].items():
        x, y = (int(v) for v in xy.split(","))
        assert list(im.getpixel((x, y))) == want, f"tint at {xy}: {im.getpixel((x, y))} vs {want}"
    for xy, wrong in g["spot_image_intensity_misreading"].items():
        x, y = (int(v) for v in xy.split(","))
        assert list(im.getpixel((x, y))) != wrong, f"the tint at {xy} is still read as an intensity"


def test_an_unfiltered_raster_reaches_the_html():
    """A stream with NO `/Filter` makes lopdf's `decompressed_content()` error, and the
    render path read its samples with `.ok()?` — so `extract_images()` returned two valid
    PNGs for this fixture while `to_html` emitted zero `<img>`."""
    pages = _html_image_pixels(UNDRAWN_IMAGE, 2)
    assert [p.size for p in pages] == [(40, 30), (42, 32)], "one per page: the drawn ones only"


def test_placeholder_mode_counts_the_images_embed_mode_emits():
    """`<image N>` stands in for an image inline mode would actually emit, so the two counts
    must agree. The placeholder gate used to be an independently-written `bpc == 8` guess:
    colorspace_images.pdf emitted 3 placeholders for 2 embedded images."""
    for name in ("colorspace_images.pdf", "render_samples.pdf", "undrawn_image.pdf", "figures.pdf"):
        doc = distillpdf.Pdf.open(os.path.join(FIX, name))
        embed = len(re.findall(r"<img\b", doc.to_html(mode="page", return_string=True, image_mode="embed")))
        drop = len(re.findall(r"<image\b", doc.to_html(mode="page", return_string=True, image_mode="drop")))
        assert drop == embed, f"{name}: {drop} placeholders vs {embed} embedded images"


def test_every_extracted_image_across_the_fixtures_opens():
    """The usability contract: on the owned corpus every returned blob is a file PIL can
    open. Only 44% of corpus blobs did before the bytes were assembled/unwrapped; a row we
    genuinely cannot assemble is allowed to stay `raw`, but must then say so AND carry the
    metadata to reassemble it."""
    Image = pytest.importorskip("PIL.Image")
    checked = 0
    for name in sorted(os.listdir(FIX)):
        if not name.endswith(".pdf"):
            continue
        for im in distillpdf.Pdf.open(os.path.join(FIX, name)).extract_images():
            where = f"{name} p{im['page']} #{im['index']} ({im['format']})"
            # `jpx` joins this list with `undecodable_codec.pdf`: JPEG 2000 decoding is
            # parked by decision, so the honest contract is the same one `raw`/`ccitt`/
            # `jbig2` already meet — say what the format is and carry the metadata to
            # reassemble it, rather than hand back bytes claiming to be an openable file.
            # (The render says so too: `to_html` emits a labelled placeholder naming the
            # codec, and `stream_integrity()` reports the stream as `codec-unsupported`.)
            if im["format"] in ("raw", "ccitt", "jbig2", "jpx"):
                assert im["color_space"] is not None or im["bits_per_component"] is not None, \
                    f"{where}: unassembled row must carry reassembly metadata"
                continue
            img = Image.open(io.BytesIO(im["data"]))
            img.load()
            assert img.size == (im["width"], im["height"]), f"{where}: dims disagree"
            checked += 1
    assert checked >= 8, f"expected the fixture set to exercise several images, saw {checked}"


def test_extract_fonts_recurses_into_form_xobjects():
    """The page's own /Font dict is empty and the only font lives in a form's /Resources —
    the template layout that made a 166-page corpus preprint report no fonts at all. The
    form is invoked under two names, so the row must be de-duplicated to exactly one."""
    fonts = distillpdf.Pdf.open(FORM_FONT).extract_fonts()
    assert len(fonts) == 1, f"form-nested font missing or duplicated: {fonts}"
    f = fonts[0]
    assert (f["page"], f["name"], f["subtype"], f["base_font"]) == (1, "FF1", "Type1", "Helvetica")
    assert f["encoding"] == "WinAnsiEncoding"
    assert f["embedded"] is False and f["has_tounicode"] is False


def test_extract_images_recurses_into_form_xobjects():
    """A raster whose only reference is inside a Form XObject must still be reported: the
    page's own /Resources names the form, not the image. Page 2 pins the ordering contract
    — a directly-referenced image keeps index 0 and the nested one is appended."""
    imgs = distillpdf.Pdf.open(FORM_IMAGE).extract_images()
    page1 = [i for i in imgs if i["page"] == 1]
    assert len(page1) == 1, f"form-nested image missing (got {page1})"
    assert (page1[0]["width"], page1[0]["height"]) == (240, 160)
    assert len(page1[0]["data"]) > 0

    page2 = [i for i in imgs if i["page"] == 2]
    assert [(i["index"], i["width"], i["height"]) for i in page2] == [(0, 120, 90), (1, 240, 160)]


def test_extract_images_reports_only_what_the_page_draws():
    """The fixture's two pages share ONE /Resources dict (inherited from /Pages) listing
    every image in the file — the iText layout that made a reachability walk report all
    338 images of a corpus preprint on each of its 166 pages. Only the images the page's
    content stream actually paints are its images, including through a form."""
    imgs = distillpdf.Pdf.open(UNDRAWN_IMAGE).extract_images()
    assert [(i["page"], i["index"], i["width"], i["height"]) for i in imgs] == [
        (1, 0, 40, 30),   # page 1 paints /ImDrawn
        (2, 0, 42, 32),   # page 2 paints only the form, whose content paints /ImInForm
    ]
    # /ImNever (41x31) and /ImFormNever (43x33) are listed in the same resource
    # dictionaries but painted by nobody.
    assert all((i["width"], i["height"]) not in {(41, 31), (43, 33)} for i in imgs)
    assert all(len(i["data"]) > 0 for i in imgs)


def test_extract_images_finds_annotation_appearance_streams():
    """``/Annots -> /AP /N`` is a content stream a viewer paints onto the page and that
    nothing in the pipeline used to walk, so an image living only inside a stamp's or a
    widget's appearance was reported by nobody. The fixture pins the selection rules with
    one distractor per rule."""
    imgs = distillpdf.Pdf.open(ANNOT_APPEARANCE).extract_images()
    assert [(i["page"], i["index"], i["width"], i["height"]) for i in imgs] == [
        (1, 0, 40, 30),   # painted by the page's own content: keeps index 0
        (1, 1, 10, 10),   # a /Stamp annotation's appearance stream
        (1, 2, 12, 12),   # a /Widget whose /AS selects this appearance state
        (1, 3, 15, 15),   # a state dictionary with no /AS: every state counts
        (1, 4, 16, 16),
    ]
    sizes = {(i["width"], i["height"]) for i in imgs}
    assert (11, 11) not in sizes, "in the appearance's /Resources but never drawn"
    assert (13, 13) not in sizes, "the appearance state /AS did not select"
    assert (14, 14) not in sizes, "a hidden (/F bit 2) annotation draws nothing"
    assert all(len(i["data"]) > 0 for i in imgs)


def test_extract_fonts_ignores_annotation_appearances():
    """The deliberate asymmetry with images: a widget's own tick font is a property of the
    form field, not of the page's text, and the parity reference does not report it."""
    fonts = distillpdf.Pdf.open(ANNOT_APPEARANCE).extract_fonts()
    assert [f["name"] for f in fonts] == ["F1"]


def test_extract_tables():
    tables = distillpdf.Pdf.open(NUMERIC).extract_tables()
    assert tables, "numeric table not extracted"
    t = tables[0]
    for key in ("page", "n_rows", "n_cols", "cells"):
        assert key in t, f"table dict missing {key!r}"
    flat = [c for row in t["cells"] for c in row]
    assert "System" in " ".join(flat) and "84.6" in " ".join(flat), "table cells wrong"


def test_analyze_tables_surface_and_document_delegation():
    direct = distillpdf.Pdf.open(NUMERIC).analyze_tables()
    assert direct == distillpdf.open(NUMERIC).analyze_tables()
    assert direct
    table_keys = {
        "page", "bbox_norm", "n_rows", "n_cols", "header_rows", "cells",
        "caption", "evidence",
    }
    cell_keys = {
        "text", "row", "col", "rowspan", "colspan", "bbox_norm", "role",
        "header_path",
    }
    for table in direct:
        assert set(table) == table_keys
        assert table["page"] >= 1
        assert table["bbox_norm"] is None or len(table["bbox_norm"]) == 4
        assert table["n_rows"] >= 2 and table["n_cols"] >= 2
        assert table["header_rows"] <= table["n_rows"]
        assert table["evidence"] and set(table["evidence"]) <= {"ruled", "aligned"}
        anchors = {(cell["row"], cell["col"]) for cell in table["cells"]}
        for cell in table["cells"]:
            assert set(cell) == cell_keys
            assert cell["rowspan"] >= 1 and cell["colspan"] >= 1
            assert cell["role"] in {"header", "data"}
            assert all(tuple(anchor) in anchors for anchor in cell["header_path"])
            assert cell["bbox_norm"] is None or len(cell["bbox_norm"]) == 4
            assert "confidence" not in cell and "rejection" not in cell
        assert "confidence" not in table and "rejections" not in table


def test_analyze_tables_only_claims_exact_ruled_cell_boundaries():
    tables = distillpdf.Pdf.open(RULED_BLANK).analyze_tables()
    assert len(tables) == 1
    assert "ruled" in tables[0]["evidence"]
    assert tables[0]["cells"]
    assert all(cell["bbox_norm"] is not None for cell in tables[0]["cells"])

    inferred = distillpdf.Pdf.open(NUMERIC).analyze_tables()
    assert any("aligned" in table["evidence"] for table in inferred)
    assert all(
        cell["bbox_norm"] is None
        for table in inferred if "ruled" not in table["evidence"]
        for cell in table["cells"]
    )


def test_extract_links_shapes():
    links = distillpdf.Pdf.open(LINKS).extract_links()
    assert len(links) >= 2
    for lk in links:
        for key in ("page", "rect", "kind", "uri", "dest_page", "dest_name", "remote_file"):
            assert key in lk, f"link dict missing {key!r}"
        assert lk["kind"] in ("uri", "internal", "remote")
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


def test_stream_integrity_names_a_damaged_stream_and_is_silent_on_a_healthy_one():
    """A decode failure is not always an error on lopdf 0.40: a TRUNCATED FlateDecode stream
    is reported ``Ok`` with the partial output, so a page renders short with no signal at
    all, and a filter chain lopdf cannot apply (``ASCIIHexDecode``) degrades to the encoded
    bytes verbatim. ``stream_integrity()`` is the answer to "is the page I just rendered the
    whole page?" — and it must stay quiet about intact streams, or it says nothing."""
    g = GT["damaged_streams.pdf"]
    d = distillpdf.Pdf.open(os.path.join(FIX, "damaged_streams.pdf"))
    # Degrade LOUDLY, not differently: the document still opens and still renders.
    assert d.page_count() == g["pages"]
    assert "<html" in d.to_html(return_string=True)
    issues = d.stream_integrity()
    assert {str(i["object"][0]): i["kind"] for i in issues} == g["issues"], issues
    for i in issues:
        assert set(i) == {"object", "kind", "filter", "recovered"}
        assert isinstance(i["recovered"], int) and i["recovered"] >= 0
    objs = [i["object"][0] for i in issues]
    assert g["intact_object"] not in objs, "an intact stream must not be reported"
    # A healthy document reports nothing whatsoever.
    assert distillpdf.Pdf.open(HEADINGS).stream_integrity() == []
