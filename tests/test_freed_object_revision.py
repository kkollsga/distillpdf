"""A revision that **frees** an object stays deleted — on both engines, through the wheel.

``freed_object_revision.pdf`` (``tests/gen_fixtures.py::gen_freed_object_revision``) is the
redaction shape: revision 1 defines two content streams for the page, revision 2 frees the
second one and touches nothing else. The page's ``/Contents`` array still names it, which is
not a defect — ISO 32000-1 7.3.10 makes a reference to a freed object a reference to the null
object — so the deleted sentence is gone by the rules of the format while its bytes are still
in the file, in the previous revision.

Neither of lopdf's eager cross-reference parsers recorded free entries: the classic table
dropped ``f`` rows and the stream decoder read and discarded type-0 rows. With no entry in the
newest section, ``Xref::merge`` let the base section's definition win, and the eager engine
printed the sentence the file deleted — on the default engine, on exactly the documents that
have been incrementally updated. The indexed reader always recorded them and masked correctly,
which is why this was also the last eager/indexed xref disagreement in the 196-document corpus
sweep (6 objects across 2 documents).

The engine is chosen once per process from the environment, so each render runs in a fresh
child with the variable set explicitly — including the eager baseline, which must stay pinned
even when the whole suite runs under ``DISTILLPDF_ENGINE=indexed``.
"""
import json
import os
import subprocess
import sys

from _fixtures import FIX, GT, text

FIXTURE = os.path.join(FIX, "freed_object_revision.pdf")
_RENDER = (
    "import hashlib, json, sys, distillpdf;"
    "d = distillpdf.Pdf.open(sys.argv[1]);"
    "h = d.to_html(return_string=True, image_mode='drop');"
    "print(json.dumps({'sha': hashlib.sha256(h.encode()).hexdigest(), 'html': h,"
    " 'pages': d.page_count(), 'engine': d.engine}))"
)


def _render(engine):
    env = dict(os.environ)
    env.pop("DISTILLPDF_ENGINE", None)
    if engine:
        env["DISTILLPDF_ENGINE"] = engine
    out = subprocess.run([sys.executable, "-c", _RENDER, FIXTURE],
                         env=env, capture_output=True, text=True, check=True)
    return json.loads(out.stdout.strip().splitlines()[-1])


def test_fixture_is_the_deletion_shape_it_claims():
    ground = GT["freed_object_revision.pdf"]
    raw = open(FIXTURE, "rb").read()
    assert raw.count(b"startxref") == ground["revisions"] == 2
    # The deleted object is still defined — once — and still referenced by the page.
    assert raw.count(b"\n%d 0 obj\n" % ground["freed_object"]) == 1
    assert b"/Contents [%d 0 R %d 0 R]" % (ground["kept_object"],
                                           ground["freed_object"]) in raw
    # The newest section's entry for it is free, with a non-65535 generation: a real
    # deletion, not the free-list head.
    assert b"0000000000 00001 f " in raw


def test_a_freed_object_stays_deleted_on_both_engines():
    ground = GT["freed_object_revision.pdf"]
    for engine in (None, "indexed"):
        name = engine or "eager"
        result = _render(engine)
        assert result["pages"] == 1, name
        body = text(result["html"])
        for needle in ground["text_contains"]:
            assert needle in body, (name, needle)
        for needle in ground["text_excludes"]:
            assert needle not in body, f"{name} resurrected the freed object"


def test_engines_render_byte_identical_output():
    eager, indexed = _render(None), _render("indexed")
    assert indexed["sha"] == eager["sha"], (
        "eager and indexed disagree on the freed-object fixture"
    )
