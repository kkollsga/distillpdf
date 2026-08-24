"""The Pdf public API surface, exercised on owned fixtures: open/from_bytes, page_count,
plain-text extraction (whole-doc + per-page), and the structured extractors
(fonts/images/tables/links) plus the diagnostic span dumps — shapes, types, and basic
invariants. Guards the API contract that downstream callers depend on."""
import io
import hashlib
import json
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
TABLE_CORPUS = os.path.join(os.path.dirname(__file__), "table_corpus")


# `model["source"]` carries two fields that are volatile *by construction* and therefore
# never part of what a frozen digest locks: `generated_at` (wall clock) and `distillpdf`
# (the package version, stamped from CARGO_PKG_VERSION — it changes on every release bump).
# Neutralise both to a fixed token before comparing or digesting; everything else about the
# model stays byte-locked. Mutates and returns the model for use inline.
MODEL_SOURCE_NORMALIZED = "<normalized>"


def _normalize_model_source(model):
    source = model["source"]
    source.pop("generated_at", None)
    source["distillpdf"] = MODEL_SOURCE_NORMALIZED
    return model


# SHA-256 locks derived from the preregistered frozen parent owned-surface artifact at
# dadad4fa8cc7e31bfd08671a87082e05e59d4cab. Tests contain the constants and never depend on
# that local artifact. Order: raw, analyzed, HTML, Markdown, canonical model JSON.
# The model digests were re-locked once (2026-08-08) when `source.distillpdf` started being
# normalized above: previously they hashed the literal package version, so every release
# bump drifted them. The four non-model digests are byte-identical to the frozen parent —
# only the version token changed, and it is now neutral for good.
# The HTML digests (index 2) were re-locked 2026-08-24 when the doc shell's <style> gained a
# font-family stack, line-height, and table borders/padding; only the constant
# `DOC_SHELL_HEAD` prelude changed, so every surface other than HTML is untouched below.
GROUP_HEADER_HARD_NEGATIVE_SURFACES = {
    "t1_booktabs_in_prose.pdf": ("1ef8e39c16d5baff549ed094c21035732723e2ef35ea9667e62e6b09ae682722", "1948d1777589d8dd1424f1cebcb7b3075f6d8d0213e91e0936059b8dc1d8f8cc", "ca147db7348f002b36675ccd8b959d41c171a5973899648e22b614bef2492fb6", "9f2ed8ced0e395e377aeee756dc3908ab16227a6dc0553665bf5388e2a636866", "ecfb59b249a8c9107b4b37d4cce237b52eb292be4f672ae5908515298a82d518"),
    "t1_booktabs_small.pdf": ("0249d11443229afb38d14acad716bcc33bdcf13cd661e5dd5774c3389939c736", "47112ca224cd3da2593dbc9854ecf4aff4d56cecbb878e6bd72b2f931ec1fef4", "2e3ddb966b35990101eb14807b161c8f5dd11e859e3f797aeb18b6ca04c9692b", "5e3a704f440b06db4909d221e042025a8aa85b3c6f449e95b372c36cd28b96b9", "662519d45d59f0b44c055adc5b16f3ada6af57cf6aca68fd1a8a68b7af212d8b"),
    "t1_booktabs_source_median.pdf": ("6a557ffe681de641a726ca99204294e383e8b0aebc596053ba9c2e44c7c8e9be", "e95c3ebc979600d6d6db9a58b00c7f8cea9c83bb5b389d9575170b7133c6f02a", "4ce3493b6451e74b417f97bf4aa8cede2876ec0c1011849bd1ac190fde32dd2d", "69165b292f10b20e8de166547594833bba8ace0d4b186cea6f8d17978c8ab8a2", "0643bdf2d14853acddeea18381d7a316606006f7f0076b15c64674415da20e2f"),
    # HTML / Markdown / model re-locked 2026-08-08 with `fix(headings): stop form and grid
    # furniture reading as sections`. This file's only prominent line is its table caption
    # ("Table 1. booktabs, declared"), so `find_document_title` published it as the document
    # `<h1>` — the same words emitted twice, once as a title and once beside the grid. It is
    # now the table's own `<caption>` (and the table carries the caption's `tab-1` anchor), so
    # the caption appears exactly once on all three surfaces. The two table surfaces (raw,
    # analyzed) are byte-identical to the frozen parent and are unchanged below.
    "t1_booktabs_tagged.pdf": ("b2986b61dfc8f50ca8605a13720f99263b6c06fa207f855988fbfe68b238b648", "ea2fe0bd8ea85e506073735c3de7a0608154646232cfc6256a41b7101e6ad38a", "1cc4ba4b7f3cb44b4f8c3d9b4441c0062ed95a2b8fa4ad2c1ce7e114fca64488", "3ab9f34ba023ed650b903ff3e274a27307cf8f4930b7d0cff874586e68d0dc12", "a5282eac012c2c8d168ef5fe0852fbc7ae1880c5553ac0b9dabdfde2472b7fa8"),
    "t2_blank_cells_grid_10pct.pdf": ("35be0fe770d111f20f3b8dbfd166383dc346848d081708b8a3e734501625b7c3", "8dcc4cad594ebe0e99a7fe8cd7c0dd88eb747804fd8c401054f7992eb13c7622", "1e03bb90a20a19c1e490f8e815848fb4852c342514adfb3831c78420eea5d999", "e282904302fad8bc0843c6779c39e9145aae0fc207d103b8ee6f0c99e1fff1be", "3c79dd1e9b93624b3e307bd6184e6911a498ebeadf6493aeaf6ddcc2ff8296a3"),
    "t2_blank_cells_grid_30pct.pdf": ("81ccf3442cca9f7491906a770bf6f7e539fe21ff15a362172b14b79c2c1bb1a2", "999a277f3322821ff7ff3d32f50c9d15698edb1b4ea9c5cac9e69eae37aca8bb", "8f487481274776e6b67abc85392b97ba009c206dc55abe2afe3c6908241f2d40", "bbfd960a129e038e9c9976f3b9f747636857e27fbd556fbb503c727b109a01a9", "16e436d012e5fb1db56c9c002a8ae2cab7935113f56d20512b58fd70cc43bf73"),
    "t2_multitier_header_four_tier.pdf": ("6b319958f476eb544d9dcf789edc35f6807eb7e76a5e370183b999fa031a23bd", "0f06ec64040bbcbea6b1e6a6926f4a5d4ac7765cb87a4a94fe193dde92d3ca61", "4522c264cec413df08589064deba7c32e651b55a31119db6f6db3de0be4814f1", "0c7d1bcdc5f3c266fd6d43330b0d97c653486bb905d7f0071f0702039c140253", "cc1ee555d6b2a250da6a7a6054910a30cb6d1ee59ef22ef3accb90fd20d208c7"),
    "t2_multitier_header_two_tier.pdf": ("b61d9be65bd334726fa3d5d241116dcdf09278ea772b80771b2f63f09e575538", "5a493d9be8b72420fd654413a003afb57584db3f7af249315063624cce66c5cb", "86c940d78b90eb56ea0ad7a7fdc8b8c93d77693965646de01db3cbbc4b8e5fac", "560c5cc3cf12f3f488bebe98d83093a81c91c8de3c932eb15b0aa17d54d4ed7e", "bad689aa6044227ef9c2a8ccc8d2d98f9796aa9477826b74b7ba227f194ef57d"),
    "t2_no_header_borderless.pdf": ("9003e063ce6009af150f03153aab3f1464d5b3b7bd8c022b1fed265709b4e7dd", "fba82dcae0ddd045e299ca95399c0a5b211013890eaea3ed4fa28adcfcd62482", "8d2fc91383dfe69baa2adc25b1bc240f51b69e1b51e1a238b5d6c82944fe3cec", "68b63ded276afd13725e6411a6cd44176a2b297c0c148e8ea64f4d6bc71bc48d", "ab0e4ba9c982046829772486fef83dc5e1ac96a65a083e279f29720931cf8f59"),
    "t2_no_header_grid.pdf": ("c843aa61246ed38dd59b8012b8b0d5a54d2dab112eaa8e6f59265873e5c2e137", "38530b8093bde7774c22ad184aa32a86b5b717c1432c40706c5261a4f4bf8aa8", "3313ee72f68afa83bb6ee7eafeab70fcec40802688f884973501f8bce17b340e", "ee65731df6d6fc94e75128e06a1bc31a6edb5c6c9407e69f0e7bc6b6aa4753b5", "d9eac27e9694ea9dae1aef4b7191fc884fbe92f979199396de3a845806175ef9"),
    "t3_blank_borderless.pdf": ("d08def3b19655ea33396daa3c65d7f2663e4d74cb15bb522c6fa63f63940984d", "7aed89fecf24e9e80f00ebb7dd9466af681ffaecbadf037c2e0125230ebad1f0", "f4eb118e95b973f3ce1205adb8ca8927d76ee1442eec7645f31d33ab193fc8d8", "25e541b48ab6c398161c0625245a30be3b627cefc3cc766bb019b40068a8d7f4", "89add2af2f179b0038f3656e2beeb53a252c4e738ca0337dcdfca69488e1fb50"),
    "t3_spacer_cols.pdf": ("d7db0d8caa46dadd225cefcaf5dbe445688d71dccf3518a5d2bc11d87c86e0df", "8739e1c9728a3175fe439f1df7a27493113e394e00319cd04b7fb695d0eca209", "92d5ba202be9f3816d1cd3d5be0de6e07bc3bef5fb7f5b5098ba05b4ff5788ff", "3c0fb5bfbc2cd953e8b812df5b2c57423369c3b9ea02c304ea4790f309d43ddc", "5e518ac543ed891098285046dabafcfb0d9b41d9290ccd022dc5eb7008030b45"),
}


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


def test_inline_images_are_extracted_and_rendered():
    """Inline images (BI…ID…EI, §8.9.7) inside a Form XObject: the abbreviated-key RGB
    image and the 1-bpc stencil both extract as PNG rows and render as raster data URIs,
    while the FILTERED inline image the parser cannot take leaves a labelled SVG
    placeholder — an honest failure, never a silently blank figure."""
    gt = GT["inline_image.pdf"]
    pdf = distillpdf.Pdf.open(os.path.join(FIX, "inline_image.pdf"))
    rows = [(i["page"], i["width"], i["height"], i["format"]) for i in pdf.extract_images()]
    assert (1, gt["rgb"][0], gt["rgb"][1], "png") in rows
    assert (1, gt["stencil"][0], gt["stencil"][1], "png") in rows
    assert not any(page == 2 for page, *_ in rows), "the filtered inline image has no samples"
    html = pdf.to_html(image_mode="embed", return_string=True)
    assert html.count("data:image/png") >= 2, "inline rasters missing from the render"
    assert html.count("data:image/svg+xml") == 1, "filtered inline image needs its placeholder"
    assert gt["caption"] in html


def test_medium_weight_face_is_not_bold():
    """A spelled-out 'Medium' BaseFont (CSS weight 500, a body weight) must render as
    regular text, while a genuinely bold face and the Nimbus '-Medi' heading
    abbreviation keep their weight — the classification is by NAME only here, since
    none of the fixture's faces are embedded."""
    gt = GT["medium_weight.pdf"]
    html = distillpdf.Pdf.open(os.path.join(FIX, "medium_weight.pdf")).to_html(
        image_mode="drop", return_string=True)
    assert gt["regular"] in html
    assert f"<b>{gt['regular']}" not in html and not re.search(
        r"<b>[^<]*" + re.escape(gt["regular"]), html), "Medium body face rendered bold"
    assert re.search(r"<b>[^<]*" + re.escape(gt["bold"]), html)
    assert re.search(r"<(b|h\d)>(<b>)?[^<]*" + re.escape(gt["medi_heading"]), html)


def test_hscale_grid_honours_tz_and_keeps_the_cut_guard():
    """The Tz (horizontal scaling) fixture: page 1's 60%-scaled grid and page 2's
    naturally fitting twin both yield the full 12x8 ruled lattice with exact cell text,
    while page 3's genuinely overrunning grid must NOT be admitted as one — so the fix
    is pinned to the Th advance term, not to a loosened LATTICE_CUT_PCT."""
    gt = GT["hscale_grid.pdf"]
    tables = distillpdf.Pdf.open(os.path.join(FIX, "hscale_grid.pdf")).analyze_tables()
    by_page = {}
    for t in tables:
        by_page.setdefault(t["page"], []).append(t)
    for page in (1, 2):
        full = [t for t in by_page.get(page, [])
                if (t["n_rows"], t["n_cols"]) == (gt["rows"], gt["cols"])]
        assert full, f"page {page}: full {gt['rows']}x{gt['cols']} grid not recovered"
        assert "ruled" in full[0]["evidence"]
        cells = {(c["row"], c["col"]): c["text"] for c in full[0]["cells"]}
        if page == 1:
            assert cells[(0, 0)] == gt["cell_0_0"]
            assert cells[(0, 1)] == gt["cell_0_1"], "neighbour values fused"
    assert not any((t["n_rows"], t["n_cols"]) == (gt["rows"], gt["cols"])
                   for t in by_page.get(3, [])), "overrunning grid wrongly admitted"


def test_analyze_tables_recovers_aligned_group_header_topology_only(tmp_path):
    path = os.path.join(TABLE_CORPUS, "t2_merged_colspan_over_booktabs.pdf")
    pdf = distillpdf.Pdf.open(path)
    tables = pdf.analyze_tables()
    assert len(tables) == 1
    table = tables[0]
    assert (table["n_rows"], table["n_cols"], table["header_rows"]) == (8, 6, 2)
    assert table["evidence"] == ["aligned"]
    assert len(table["cells"]) == 44
    assert [
        (cell["col"], cell["colspan"], cell["text"])
        for cell in table["cells"] if cell["row"] == 0
    ] == [(0, 3, "Geochemistry"), (3, 3, "Location")]
    assert next(
        cell for cell in table["cells"] if (cell["row"], cell["col"]) == (2, 2)
    )["header_path"] == [[0, 0], [1, 2]]
    assert next(
        cell for cell in table["cells"] if (cell["row"], cell["col"]) == (2, 4)
    )["header_path"] == [[0, 3], [1, 4]]

    # The logical annotation must not alter the legacy raw projection. Semantic HTML consumes
    # the canonical anchors, suppresses covered slots, and identifies grouped column headers.
    assert pdf.extract_tables()[0]["cells"][0] == [
        "Geochemistry", "", "", "Location", "", "",
    ]
    html = pdf.to_html(return_string=True)
    html_table = "<table" + html.split("<table", 1)[1].split(
        "</table>", 1
    )[0] + "</table>"
    assert html_table.startswith(
        '<table data-dpdf-semantic-spans><tr><th scope="colgroup" colspan="3">Geochemistry</th>'
        '<th scope="colgroup" colspan="3">Location</th></tr>'
    )
    markdown = pdf.to_markdown(return_string=True)
    assert markdown.startswith(
        "| Geochemistry |  |  | Location |  |  |\n"
        "| --- | --- | --- | --- | --- | --- |\n"
    )

    dpdf = pdf.distill(str(tmp_path / "aligned-semantic.dpdf"))
    assert distillpdf.render_html(dpdf) == html
    assert distillpdf.render_markdown(dpdf, image_mode="drop") == markdown
    block = json.loads(distillpdf.load_model(dpdf))["blocks"][0]
    assert block["el_html"] == html_table
    assert "table_semantic_spans" not in block


def test_ruled_group_header_moves_into_table_prose_and_model_atomically(tmp_path):
    path = os.path.join(TABLE_CORPUS, "t2_merged_colspan_over_grid.pdf")
    pdf = distillpdf.Pdf.open(path)
    tables = pdf.analyze_tables()
    assert len(tables) == 1
    table = tables[0]
    assert (table["n_rows"], table["n_cols"], table["header_rows"]) == (8, 6, 2)
    assert table["evidence"] == ["ruled", "aligned"]
    assert len(table["cells"]) == 44
    assert [
        (cell["col"], cell["colspan"], cell["text"], cell["role"])
        for cell in table["cells"] if cell["row"] == 0
    ] == [
        (0, 3, "Geochemistry", "header"),
        (3, 3, "Location", "header"),
    ]
    data = next(cell for cell in table["cells"] if (cell["row"], cell["col"]) == (2, 4))
    assert data["role"] == "data"
    assert data["header_path"] == [[0, 3], [1, 4]]
    assert all(cell["bbox_norm"] is not None for cell in table["cells"])

    # The exact proven tier is physically part of this ruled table's legacy grid too. Covered
    # colspan slots stay explicit and blank; ordinary detached headers remain unchanged.
    legacy = pdf.extract_tables()[0]["cells"]
    assert (len(legacy), len(legacy[0])) == (8, 6)
    assert legacy[0] == ["Geochemistry", "", "", "Location", "", ""]

    html = pdf.to_html(
        mode="page", toc=False, image_mode="drop", return_string=True,
    )
    html_table = "<table" + html.split("<table", 1)[1].split("</table>", 1)[0] + "</table>"
    assert html_table.startswith(
        '<table data-dpdf-proven-leading-tier><tr>'
        '<th scope="colgroup" colspan="3">Geochemistry</th>'
        '<th scope="colgroup" colspan="3">Location</th></tr>'
    )
    for label in ("Geochemistry", "Location"):
        assert html.count(label) == 1
        assert label in html_table

    markdown = pdf.to_markdown(
        mode="page", toc=False, image_mode="drop", return_string=True,
    )
    assert (
        "| Geochemistry |  |  | Location |  |  |\n"
        "| --- | --- | --- | --- | --- | --- |\n"
        "| Sample | Depth | Grade | Lat | Lon | Zone |"
    ) in markdown
    for label in ("Geochemistry", "Location"):
        assert markdown.count(label) == 1

    dpdf = pdf.distill(str(tmp_path / "ruled-group.dpdf"))
    assert distillpdf.render_html(dpdf, mode="page", toc=False) == html
    assert distillpdf.render_markdown(
        dpdf, mode="page", toc=False, image_mode="drop",
    ) == markdown

    model = json.loads(distillpdf.load_model(dpdf))
    assert len(model["blocks"]) == 1
    block = model["blocks"][0]
    assert (block["id"], block["kind"], block["page"]) == ("b0001", "table", 1)
    assert block["bbox"] == [
        75.5999984741211, 600.4000244140625,
        536.3999633789062, 728.4000244140625,
    ]
    assert len(block["cells"]) == 8 and all(len(row) == 6 for row in block["cells"])
    assert block["cells"][0] == [
        "Geochemistry", "Geochemistry", "Geochemistry",
        "Location", "Location", "Location",
    ]
    assert block["table_header"] == [
        [["Geochemistry", 3], ["Location", 3]],
    ]
    assert block["table_header_rows"] == 2
    assert block["table_proven_leading_tier"] is True
    assert block["table_grid"] == legacy[1:]
    assert "el_html" not in block
    assert "table_semantic_spans" not in block
    assert not any(item["kind"] == "heading" for item in model["blocks"])
    assert model["sections"] == []
    assert model["toc"] == []
    assert model["indexes"] == {
        "coverage": {"sectioned": 0.0, "unsectioned_blocks": ["b0001"]},
        "kinds": {"table": [{"id": "b0001", "page": 1}]},
        "pages": {"1": ["b0001"]},
        "sections": {},
    }
    assert model["links"] == model["assets"] == model["named_dests"] == []
    assert model["ocr_passes"] == []
    assert model["metadata"] == {}
    assert model["pages"] == [{"height_pts": 792.0, "n": 1, "width_pts": 612.0}]
    assert _normalize_model_source(model)["source"] == {
        "distillpdf": MODEL_SOURCE_NORMALIZED,
        "file": "t2_merged_colspan_over_grid.pdf",
        "pages": 1,
        "sha256": "6d28e5570ff2708b21a047f882bc2423d97647d6645ad5aa00256fccdf5082bb",
    }


@pytest.mark.parametrize(
    "name, expected",
    [
        ("t2_blank_cells_grid_10pct.pdf", [(10, 6, 1, ("ruled", "aligned"), 60, 0)]),
        ("t2_blank_cells_grid_30pct.pdf", [(10, 6, 1, ("ruled", "aligned"), 60, 0)]),
        ("t3_blank_borderless.pdf", [(8, 5, 1, ("aligned",), 40, 0)]),
        ("t3_spacer_cols.pdf", [(6, 11, 1, ("aligned",), 66, 0)]),
        ("t1_booktabs_in_prose.pdf", [(5, 4, 1, ("aligned",), 20, 0)]),
        ("t1_booktabs_small.pdf", [(5, 4, 1, ("aligned",), 20, 0)]),
        ("t1_booktabs_tagged.pdf", [(5, 5, 1, ("aligned",), 25, 0)]),
        ("t1_booktabs_source_median.pdf", [(6, 5, 1, ("aligned",), 28, 1)]),
        ("t2_multitier_header_two_tier.pdf", [(7, 7, 2, ("ruled", "aligned"), 49, 0)]),
        ("t2_multitier_header_three_tier.pdf", [(8, 9, 3, ("ruled", "aligned"), 72, 0)]),
        ("t2_multitier_header_four_tier.pdf", [(9, 9, 4, ("ruled", "aligned"), 81, 0)]),
        ("t2_no_header_borderless.pdf", [(27, 4, 1, ("aligned",), 108, 0)]),
        ("t2_no_header_grid.pdf", [(28, 4, 1, ("ruled", "aligned"), 112, 0)]),
    ],
)
def test_group_header_topology_proof_leaves_hard_negatives_unchanged(name, expected):
    tables = distillpdf.Pdf.open(os.path.join(TABLE_CORPUS, name)).analyze_tables()
    signature = [
        (
            table["n_rows"],
            table["n_cols"],
            table["header_rows"],
            tuple(table["evidence"]),
            len(table["cells"]),
            sum(cell["rowspan"] > 1 or cell["colspan"] > 1 for cell in table["cells"]),
        )
        for table in tables
    ]
    assert signature == expected


@pytest.mark.parametrize(
    "name, rows",
    [
        (
            "t2_footnote_markers_asterisks.pdf",
            [
                ["Model", "Params", "BLEU", "Notes"],
                ["Alpha-1*", "80M", "31.2", "seed 0"],
                ["Calcite-2**", "68M", "21.2", "seed 1"],
                ["East-3***", "36M", "26.5", "seed 2"],
                ["Quartz-4*", "17M", "25.1", "seed 3"],
            ],
        ),
        (
            "t2_footnote_markers_daggers.pdf",
            [
                ["Model", "Params", "BLEU", "Notes"],
                ["Layer-1†", "22M", "37.9", "seed 0"],
                ["Shale-2‡", "90M", "31.0", "seed 1"],
                ["East-3§", "16M", "21.7", "seed 2"],
                ["East-4†", "6M", "36.4", "seed 3"],
            ],
        ),
    ],
)
def test_persistent_internal_gutter_targets_are_exact_on_every_public_projection(
    name, rows, tmp_path,
):
    frozen = {
        "t2_footnote_markers_asterisks.pdf": {
            "file_sha": "61ccf865eebce7d0e51380150267e587d68e42ed9b2cd7b77a7b3976f82033b2",
            "spans_sha": "33f55098914ba5efb91156ae281faa988e676e8d2715ca507221886c83684173",
        },
        "t2_footnote_markers_daggers.pdf": {
            "file_sha": "b59575e5213e1f50815cf9870f2826702d86aff92cefe74831bad3252074b181",
            "spans_sha": "d71d1096b3124ab496daa28a0043ce9f5723b685f51f4d2e228c9699b2f2c45b",
        },
    }[name]
    pdf = distillpdf.Pdf.open(os.path.join(TABLE_CORPUS, name))
    assert pdf.extract_tables() == [
        {"page": 1, "n_rows": 5, "n_cols": 4, "cells": rows}
    ]

    analyzed = pdf.analyze_tables()
    assert len(analyzed) == 1
    table = analyzed[0]
    assert table["bbox_norm"] == [
        0.13006535172462463,
        0.09356057643890381,
        0.7364656925201416,
        0.17436867952346802,
    ]
    assert (
        table["n_rows"],
        table["n_cols"],
        table["header_rows"],
        table["evidence"],
        table["caption"],
    ) == (5, 4, 1, ["aligned"], None)
    assert [cell["text"] for cell in table["cells"]] == sum(rows, [])
    assert all(cell["bbox_norm"] is None for cell in table["cells"])
    assert all(cell["rowspan"] == cell["colspan"] == 1 for cell in table["cells"])
    assert [cell["role"] for cell in table["cells"]] == ["header"] * 4 + ["data"] * 16
    assert [cell["header_path"] for cell in table["cells"][:4]] == [[], [], [], []]
    assert [cell["header_path"] for cell in table["cells"][4:8]] == [
        [[0, 0]], [[0, 1]], [[0, 2]], [[0, 3]],
    ]

    expected_html = "<table>" + "".join(
        "<tr>" + "".join(
            (f'<th scope="col">{text}</th>' if row == 0 else f"<td>{text}</td>")
            for text in cells
        ) + "</tr>"
        for row, cells in enumerate(rows)
    ) + "</table>"
    html = pdf.to_html(image_mode="drop", return_string=True)
    html_table = "<table" + html.split("<table", 1)[1].split("</table>", 1)[0] + "</table>"
    assert html_table == expected_html
    assert html == (
        '<!doctype html>\n<html>\n<head>\n<meta charset="utf-8">\n<style>\n'
        'body{max-width:48rem;margin:auto;padding:1rem;'
        'font-family:system-ui,-apple-system,"Segoe UI",Roboto,"Helvetica Neue",Arial,sans-serif;'
        'line-height:1.55}\n'
        'img,svg{max-width:100%;height:auto}\n'
        'table{border-collapse:collapse;margin:1rem 0}\n'
        'th,td{border:1px solid #9994;padding:.3rem .6rem;text-align:left}\n</style>\n</head>\n<body>\n'
        + expected_html
        + '<ul><li>trained for 100 epochs. ** ablation. *** ours.</li></ul>'
        '</body>\n</html>\n'
    )

    expected_markdown = "\n".join(
        [
            "| " + " | ".join(rows[0]) + " |",
            "| --- | --- | --- | --- |",
            *("| " + " | ".join(row) + " |" for row in rows[1:]),
        ]
    )
    markdown = pdf.to_markdown(image_mode="drop", return_string=True)
    assert markdown == (
        expected_markdown
        + "\n\n- trained for 100 epochs. ** ablation. *** ours.\n"
    )
    assert _surface_sha(pdf._dbg_spans_xy(1)) == frozen["spans_sha"]

    dpdf = pdf.distill(str(tmp_path / f"{name}.dpdf"))
    model = _normalize_model_source(json.loads(distillpdf.load_model(dpdf)))
    table_block = {
        "bbox": [
            79.5999984741211,
            653.9000244140625,
            450.7170104980469,
            717.9000244140625,
        ],
        "cells": rows,
        "confidence": 1.0,
        "id": "b0001",
        "kind": "table",
        "page": 1,
        "table_grid": rows,
        "table_header": [],
        "table_header_rows": 1,
    }
    prose_block = {
        "bbox": [
            78.0,
            632.4000244140625,
            237.6399688720703,
            640.4000244140625,
        ],
        "confidence": 1.0,
        "el_group": 1,
        "id": "b0002",
        "kind": "list_item",
        "list_ordered": False,
        "page": 1,
        "text": "trained for 100 epochs. ** ablation. *** ours.",
    }
    assert model == {
        "assets": [],
        "blocks": [table_block, prose_block],
        "indexes": {
            "coverage": {"sectioned": 0.0, "unsectioned_blocks": ["b0001", "b0002"]},
            "kinds": {"table": [{"id": "b0001", "page": 1}]},
            "pages": {"1": ["b0001", "b0002"]},
            "sections": {},
        },
        "links": [],
        "metadata": {"title": "BLEU Notes"},
        "named_dests": [],
        "ocr_passes": [],
        "pages": [{"height_pts": 792.0, "n": 1, "width_pts": 612.0}],
        "schema_version": 0,
        "sections": [],
        "source": {
            "distillpdf": MODEL_SOURCE_NORMALIZED,
            "file": name,
            "pages": 1,
            "sha256": frozen["file_sha"],
        },
        "toc": [],
    }
    assert distillpdf.render_html(dpdf) == html
    assert distillpdf.render_markdown(dpdf, image_mode="drop") == markdown


def _surface_sha(value):
    if not isinstance(value, str):
        value = json.dumps(
            value, sort_keys=True, separators=(",", ":"), ensure_ascii=False,
        )
    else:
        # Phase 8 intentionally enriches every proven column header. The exact hard-negative
        # locks continue to protect all pre-existing bytes after removing only that separately
        # tested semantic attribute; their registered hashes do not need a baseline rewrite.
        value = re.sub(r' scope="(?:col|colgroup)"', "", value)
    return hashlib.sha256(value.encode()).hexdigest()


@pytest.mark.parametrize(
    "name, expected",
    GROUP_HEADER_HARD_NEGATIVE_SURFACES.items(),
)
def test_group_header_hard_negatives_keep_all_parent_public_surfaces_exact(
    name, expected, tmp_path,
):
    pdf = distillpdf.Pdf.open(os.path.join(TABLE_CORPUS, name))
    dpdf = pdf.distill(str(tmp_path / f"{name}.dpdf"))
    model = _normalize_model_source(json.loads(distillpdf.load_model(dpdf)))
    surfaces = (
        pdf.extract_tables(),
        pdf.analyze_tables(),
        pdf.to_html(image_mode="drop", return_string=True),
        pdf.to_markdown(image_mode="drop", return_string=True),
        model,
    )
    assert tuple(map(_surface_sha, surfaces)) == expected
    assert "data-dpdf-proven-leading-tier" not in surfaces[2]
    assert all(
        block.get("table_proven_leading_tier") is None
        for block in surfaces[4]["blocks"]
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
