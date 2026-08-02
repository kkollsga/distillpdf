"""``to_html`` is a pure function of the PDF: the same document must render the same bytes,
always.

This was not true. ``img::cluster`` grouped a page's raster tiles into a
``HashMap<root, members>`` and returned ``into_values()``; Rust's ``RandomState`` seeds every
map INSTANCE separately, so the group order differed between two renders in the same process.
That order reaches output — the emitter's raster/vector absorption is first-match-wins, and
images sharing a top edge get identical sort boxes so nothing downstream re-sorts them. A real
9-page document rendered 20 distinct HTML outputs in 40 renders, two of them differing by a
whole 37 KB embedded image; a ±1 swing in the corpus gate's image count was tolerated as noise
for months because of it.

``image_order.pdf`` (``tests/gen_fixtures.py::gen_image_order``) is the owned repro: six
separate single-tile clusters on one row, sharing one top edge, in six distinct colours — 720
possible orderings, so the assertion below cannot pass by luck.
"""
import hashlib
import os

import pytest

from _fixtures import FIX, doc

RENDERS = 25


def _sha(s):
    return hashlib.sha256(s.encode()).hexdigest()


@pytest.mark.parametrize("image_mode", ["embed", "drop"])
def test_repeated_renders_are_byte_identical(image_mode):
    d = doc("image_order.pdf")
    first = d.to_html(mode="page", return_string=True, image_mode=image_mode)
    assert first.count("<img") == (6 if image_mode == "embed" else 0)
    hashes = {_sha(d.to_html(mode="page", return_string=True, image_mode=image_mode))
              for _ in range(RENDERS)}
    assert hashes == {_sha(first)}, (
        f"{len(hashes)} distinct outputs over {RENDERS} renders — to_html is not deterministic"
    )


def test_a_fresh_handle_renders_the_same_bytes():
    """Re-opening the file must not change the output either: the nondeterminism was
    per-map-instance, so a fresh `Pdf` handle re-hit it."""
    path = os.path.join(FIX, "image_order.pdf")
    import distillpdf

    hashes = {_sha(distillpdf.Pdf.open(path).to_html(mode="page", return_string=True,
                                                     image_mode="embed"))
              for _ in range(RENDERS)}
    assert len(hashes) == 1, f"{len(hashes)} distinct outputs over {RENDERS} fresh opens"


def test_the_raster_figure_fixtures_are_stable_too():
    """A wider net over the owned fixtures that actually carry rasters and vectors — the
    determinism policy is not specific to one page shape."""
    for name in ("figures.pdf", "figures_onepage.pdf", "xobject_figure.pdf",
                 "colorspace_images.pdf", "small_vector_fig.pdf"):
        d = doc(name)
        hashes = {_sha(d.to_html(mode="page", return_string=True, image_mode="embed"))
                  for _ in range(10)}
        assert len(hashes) == 1, f"{name}: {len(hashes)} distinct outputs over 10 renders"
