"""A re-tagged hybrid-reference file reads the same — and reads its *newest* revision —
through the shipped wheel on both engines.

``hybrid_xref_revision.pdf`` (``tests/gen_fixtures.py::gen_hybrid_xref_revision``) is three
revisions deep. The base tags ``Quarterly Totals`` as an ``/H2`` inside a ``/Sect`` and the
table's first row as three ``/TD`` cells; revision 2 supersedes those structure elements out
of a *second* ``/ObjStm``, turning the heading into a spanning ``/TH`` at the top of the
table and promoting the header row to ``/TH``; revision 3 appends only an ``/Info``
dictionary and — like the incremental update on top of a linearized government PDF that
this reproduces — carries no ``/XRefStm`` of its own.

Both engines used to read that wrong, in opposite directions, and the pair was the single
eager/indexed disagreement in a 196-document corpus sweep (``gov_usgs_usgs70277647.pdf``).
lopdf's eager bootstrap took ``/XRefStm`` from the newest trailer only, so a chain whose
newest section has none merged no supplement at all, kept no xref entry for the compressed
structure elements, and resolved them from whichever ``/ObjStm`` came first — the stale one:
the heading survived and the declared ``/TH`` cells did not. The indexed reader had the
mirror defect inside a revision, letting the classic section's mandatory free mask outrank
the supplement that lifts it, so it lost the declaration and fell back to inference. Both
now follow ISO 32000-1 7.5.8.4: a section's supplement supersedes that section, and the
section supersedes everything older.

The engine is chosen once per process from the environment, so each render runs in a fresh
child with the variable set explicitly — including the eager baseline, which must stay
pinned even when the whole suite runs under ``DISTILLPDF_ENGINE=indexed``.
"""
import hashlib
import json
import os
import subprocess
import sys

from _fixtures import FIX, GT, cells, text

FIXTURE = os.path.join(FIX, "hybrid_xref_revision.pdf")
_RENDER = (
    "import hashlib, json, sys, distillpdf;"
    "h = distillpdf.Pdf.open(sys.argv[1]).to_html(return_string=True, image_mode='drop');"
    "print(json.dumps({'sha': hashlib.sha256(h.encode()).hexdigest(), 'html': h}))"
)


def _render(engine):
    env = dict(os.environ)
    env.pop("DISTILLPDF_ENGINE", None)
    if engine:
        env["DISTILLPDF_ENGINE"] = engine
    out = subprocess.run([sys.executable, "-c", _RENDER, FIXTURE],
                         env=env, capture_output=True, text=True, check=True)
    return json.loads(out.stdout.strip().splitlines()[-1])


def test_fixture_is_the_hybrid_incremental_shape_it_claims():
    ground = GT["hybrid_xref_revision.pdf"]
    assert ground["revisions"] == 3 and ground["object_streams"] == 2
    raw = open(FIXTURE, "rb").read()
    assert raw.count(b"startxref") == ground["revisions"]
    # Two sections declare a supplement; the newest, deliberately, does not.
    assert raw.count(b"/XRefStm") == ground["revisions"] - 1
    assert raw.count(b"/Type /ObjStm") == ground["object_streams"]
    # Every superseded object is written twice, once per container.
    assert {pair[0] for pair in ground["superseded"]} == {9, 11, 12}


def test_newest_revision_wins_on_both_engines():
    """The two corpus symptoms, asserted on owned bytes: the declared ``/TH`` cells are
    present and the superseded heading is gone."""
    ground = GT["hybrid_xref_revision.pdf"]
    for engine in (None, "indexed"):
        html = _render(engine)["html"]
        name = engine or "eager"
        assert '<th scope="colgroup" colspan="3">Quarterly Totals</th>' in html, name
        assert cells(html, "th") == ["Quarterly Totals", "Region", "Units", "Revenue"], name
        assert "<h2>" not in html, f"{name} kept the superseded revision's heading"
        assert "sec-quarterly-totals" not in html, f"{name} kept the superseded anchor"
        body = text(html)
        for needle in ground["text_contains"]:
            assert needle in body, (name, needle)


def test_engines_render_byte_identical_output():
    eager, indexed = _render(None), _render("indexed")
    assert indexed["sha"] == eager["sha"], (
        "eager and indexed disagree on the hybrid-reference fixture"
    )
    assert hashlib.sha256(open(FIXTURE, "rb").read()).hexdigest()  # fixture is readable
