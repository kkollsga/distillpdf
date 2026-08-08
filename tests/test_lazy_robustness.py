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
  under a PNG predictor and under TIFF Predictor 2, and Flate behind an ASCII85 or ASCIIHex
  prefix). The envelope used to be "no filter or a bare ``/FlateDecode``", so five of the six
  refused.

Neither change may move the fallback contract: a document the index genuinely refuses must
still read ``"lazy (eager fallback)"``, which ``test_engine_selection.py`` pins and this file
re-checks from the other side — a recovered open is a *lazy* open, not a fallback.
"""
import hashlib
import os

import pytest

import distillpdf
from _fixtures import FIX, GT, text

DAMAGED = os.path.join(FIX, "adversarial", "damaged_startxref.pdf")
INTACT = os.path.join(FIX, "adversarial", "damaged_startxref_intact.pdf")
OBJSTM_FORMS = os.path.join(FIX, "objstm_filter_forms.pdf")


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
    assert len(ground["filters"]) == ground["object_streams"] == 6
    lazy = distillpdf.Pdf.open(OBJSTM_FORMS, engine="lazy")
    assert lazy.engine == "lazy"
    assert lazy.page_count() == ground["pages"]

    assert _sha(_html(OBJSTM_FORMS, "lazy")) == _sha(_html(OBJSTM_FORMS, "eager"))
    body = text(_html(OBJSTM_FORMS, "lazy"))
    for needle in ground["text_contains"]:
        assert needle in body, needle
