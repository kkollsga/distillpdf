"""Damage the lazy engine now survives *without* surrendering the document to eager.

Two fixture families, both from ``tests/gen_fixtures.py``:

* ``damaged_startxref.pdf`` — a file whose cross-reference machinery is destroyed and whose
  body is intact, the shape a truncated download or a botched incremental append leaves. Both
  of lopdf's readers used to fail closed on it, so it did not open on any engine and the lazy
  route's counted eager fallback had nothing to fall back *to*. The indexed reader now rebuilds
  its index from one forward scan of the body for ``N G obj`` headers. Its intact twin is
  byte-identical apart from the ten digits of the ``startxref`` operand, so "the recovered
  index is the index the intact table describes" is checkable as identical rendered output.

* ``objstm_filter_forms.pdf`` — one ``/ObjStm`` container per encoding the indexed reader's
  bounded decode envelope admits (bare Flate, Flate in a one-element ``/Filter`` array, Flate
  under a PNG predictor and under TIFF Predictor 2, Flate behind an ASCII85 or ASCIIHex
  prefix, and the two forms that name their predictor in an **array** ``/DecodeParms``
  parallel to an array ``/Filter``). The envelope used to be "no filter or a bare
  ``/FlateDecode``", so seven of the eight refused.

* ``array_decode_parms.pdf`` — the same array ``/DecodeParms`` shape on the stream kinds the
  envelope never sees: a page content stream and an image XObject. lopdf read the key with
  "as a dictionary", so the array reached the Flate layer as no parameters at all and the
  page decoded to bytes no content parser can read — rendering **blank**, on both engines,
  with nothing reported.

Neither change may move the fallback contract: a document the index genuinely refuses must
still read ``"lazy (eager fallback)"``, which ``test_engine_selection.py`` pins and this file
re-checks from the other side — a recovered open is a *lazy* open, not a fallback.
"""
import hashlib
import io
import os

import pytest

import distillpdf
from _fixtures import FIX, GT, text

DAMAGED = os.path.join(FIX, "adversarial", "damaged_startxref.pdf")
INTACT = os.path.join(FIX, "adversarial", "damaged_startxref_intact.pdf")
OBJSTM_FORMS = os.path.join(FIX, "objstm_filter_forms.pdf")
ARRAY_PARMS = os.path.join(FIX, "array_decode_parms.pdf")


def _html(path, engine):
    return distillpdf.Pdf.open(path, engine=engine).to_html(
        return_string=True, image_mode="drop"
    )


def _sha(value):
    return hashlib.sha256(value.encode()).hexdigest()


def test_the_twins_differ_only_in_the_startxref_operand():
    ground = GT["adversarial/damaged_startxref.pdf"]
    damaged = open(DAMAGED, "rb").read()
    intact = open(INTACT, "rb").read()
    assert len(damaged) == len(intact)
    start = ground["startxref_digits_offset"]
    assert damaged[:start] == intact[:start]
    assert damaged[start + 10:] == intact[start + 10:]
    assert intact[start:start + 10] == b"%010d" % ground["intact_startxref"]
    # The damaged operand is in bounds and parses; it simply does not name a section.
    assert damaged[start:start + 10].isdigit()
    assert int(damaged[start:start + 10]) < len(damaged)


def test_eager_still_cannot_open_a_destroyed_cross_reference():
    """The recovery is the lazy reader's, and only the lazy reader's.

    Kept as an assertion rather than a comment because it is what makes the lazy route's
    value here concrete: there is no fallback that would have served this document."""
    with pytest.raises(Exception):
        distillpdf.Pdf.open(DAMAGED, engine="eager")


def test_a_destroyed_cross_reference_stays_lazy_and_renders_the_intact_output():
    ground = GT["adversarial/damaged_startxref.pdf"]
    recovered = distillpdf.Pdf.open(DAMAGED, engine="lazy")
    # A recovered open is an indexed open: not a counted fallback, and not a claim the
    # caller has to take on trust.
    assert recovered.engine == "lazy"
    assert recovered.page_count() == ground["pages"]

    expected = _sha(_html(INTACT, "eager"))
    assert _sha(_html(INTACT, "lazy")) == expected
    assert _sha(_html(DAMAGED, "lazy")) == expected

    body = text(_html(DAMAGED, "lazy"))
    for needle in ground["text_contains"]:
        assert needle in body, needle


def test_every_admitted_object_stream_encoding_stays_lazy_and_matches_eager():
    ground = GT["objstm_filter_forms.pdf"]
    assert len(ground["filters"]) == ground["object_streams"] == 8
    lazy = distillpdf.Pdf.open(OBJSTM_FORMS, engine="lazy")
    assert lazy.engine == "lazy"
    assert lazy.page_count() == ground["pages"]

    assert _sha(_html(OBJSTM_FORMS, "lazy")) == _sha(_html(OBJSTM_FORMS, "eager"))
    body = text(_html(OBJSTM_FORMS, "lazy"))
    for needle in ground["text_contains"]:
        assert needle in body, needle


@pytest.mark.parametrize("engine", ["eager", "lazy"])
def test_array_decode_parms_decode_on_both_engines(engine):
    """A predictor named in an array ``/DecodeParms`` must reach its filter layer.

    The page renders blank when it does not, so every needle here is a byte the decoder
    would otherwise have silently thrown away."""
    ground = GT["array_decode_parms.pdf"]
    pdf = distillpdf.Pdf.open(ARRAY_PARMS, engine=engine)
    assert pdf.page_count() == ground["pages"]
    body = text(_html(ARRAY_PARMS, engine))
    for needle in ground["text_contains"]:
        assert needle in body, needle


def test_array_decode_parms_render_identically_on_both_engines():
    assert _sha(_html(ARRAY_PARMS, "lazy")) == _sha(_html(ARRAY_PARMS, "eager"))


def test_array_decode_parms_image_survives_its_predictor():
    """The figure's RGB ramp: a dropped predictor turns it into noise no text check sees."""
    Image = pytest.importorskip("PIL.Image")
    ground = GT["array_decode_parms.pdf"]
    images = distillpdf.Pdf.open(ARRAY_PARMS).extract_images()
    assert len(images) == 1
    image = images[0]
    assert (image["width"], image["height"]) == (ground["image_width"], ground["image_height"])
    assert image["format"] == "png"
    png = Image.open(io.BytesIO(image["data"])).convert("RGB")
    assert list(png.getpixel((0, 0))) == ground["image_first_pixel"]
    assert list(png.getpixel((ground["image_width"] - 1, ground["image_height"] - 1))) == (
        ground["image_last_pixel"]
    )
