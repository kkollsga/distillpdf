"""DoS lock: a self-referential Form XObject must not hang the render path.

The three content-stream walkers (`img.rs`, `vector.rs`, `text.rs`) descend `/Subtype /Form`
XObjects. A depth cap alone (`MAX_FORM_DEPTH` = 40) bounds *nesting* but not *work*: a form
that invokes itself twice branches 2x per level, so 2^40 ~ 1.1e12 descents. A ~1 KB PDF
shaped that way made `to_html`/`to_markdown` — the flagship path — run forever, with no
error and no timeout. Measured before the fix: `form_image.pdf` rendered in 0.026 s while
`form_bomb.pdf` had not finished after 120 s.

The fix is a shared total-work budget (`crate::WalkBudget` / `MAX_FORM_WORK`), NOT a visited
set: the same form is legitimately drawn many times on a page, so deduplicating by
`ObjectId` would silently drop real content. `form_repeat.pdf` is the control that pins
that — see `test_a_repeated_form_still_paints_every_occurrence`.

Fixtures: tests/gen_fixtures.py::gen_form_bomb.
"""
import os
import re
import time

import distillpdf

from _fixtures import FIX

ADV = os.path.join(FIX, "adversarial")
BOMB = os.path.join(ADV, "form_bomb.pdf")
CONTROL = os.path.join(ADV, "form_repeat.pdf")

# Generous on purpose: the budget caps the bomb in well under a second locally, and the
# unfixed tree needs effectively forever, so anything in this range separates the two
# without being flaky on a loaded CI runner.
WALL_CLOCK_S = 20.0


def _timed(fn):
    t = time.perf_counter()
    out = fn()
    return out, time.perf_counter() - t


def test_form_bomb_to_html_terminates():
    h, secs = _timed(lambda: distillpdf.Pdf.open(BOMB).to_html(return_string=True))
    assert secs < WALL_CLOCK_S, f"form bomb took {secs:.1f}s — the walk budget is not bounding it"
    assert isinstance(h, str)


def test_form_bomb_to_markdown_terminates():
    md, secs = _timed(lambda: distillpdf.Pdf.open(BOMB).to_markdown(return_string=True))
    assert secs < WALL_CLOCK_S, f"form bomb took {secs:.1f}s — the walk budget is not bounding it"
    assert isinstance(md, str)


def test_a_repeated_form_still_paints_every_occurrence():
    """The control: one form invoked three times must be walked three times.

    This is what makes the fix a BUDGET and not a visited set — an `ObjectId` dedupe would
    render one tile and one "REPEAT" instead of three, losing content that is really on the
    page."""
    h = distillpdf.Pdf.open(CONTROL).to_html(return_string=True, image_mode="embed")
    assert len(re.findall(r"<img\b", h)) == 3, "a repeated form lost its raster occurrences"
    assert h.count("REPEAT") == 3, "a repeated form lost its text occurrences"


def test_the_control_is_not_itself_slow():
    """Guards the guard: if the control ever became slow, the bomb assertions above would
    stop distinguishing anything."""
    _, secs = _timed(lambda: distillpdf.Pdf.open(CONTROL).to_html(return_string=True))
    assert secs < WALL_CLOCK_S
