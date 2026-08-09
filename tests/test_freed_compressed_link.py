"""A revision that frees a **compressed** object stays deleted — on both engines, through
the wheel.

``freed_compressed_link.pdf`` (``tests/gen_fixtures.py::gen_freed_compressed_link``) is the
object-stream half of ``test_freed_object_revision``'s redaction shape. Annotations, outline
items and form fields are precisely the small dictionaries a writer packs into an ``/ObjStm``,
so an incremental redaction of such a file writes a type-0 row for the member and leaves the
container it lived in untouched. The container keeps carrying the deleted dictionary, URI and
all, and the page's ``/Annots`` array keeps naming it — which ISO 32000-1 7.3.10 makes a
reference to the null object, not an error.

That gives a loader two contradicting sources of truth, and the eager one believed the wrong
one: it expanded every readable container and kept any member the merged table did not
explicitly place *elsewhere*, and a free entry names no container to disagree with. So the
default engine reported a hyperlink the file had deleted, while the indexed engine — which
resolves through the table — reported none: a redaction leak on the default route and an
engine disagreement on the same bytes. The fix makes the merged table the sole authority on
where an object lives (lopdf ``objstm_member_xref_authority_test``).

Two links sit side by side in one container so this cannot pass vacuously: the kept one must
still be reported and the freed one must not.

The engine is chosen once per process from the environment, so each render runs in a fresh
child with the variable set explicitly — including the eager baseline, which must stay pinned
even when the whole suite runs under ``DISTILLPDF_ENGINE=indexed``.
"""
import json
import os
import subprocess
import sys

from _fixtures import FIX, GT, text

FIXTURE = os.path.join(FIX, "freed_compressed_link.pdf")
_RENDER = (
    "import hashlib, json, sys, distillpdf;"
    "d = distillpdf.Pdf.open(sys.argv[1]);"
    "h = d.to_html(return_string=True, image_mode='drop');"
    "print(json.dumps({'sha': hashlib.sha256(h.encode()).hexdigest(), 'html': h,"
    " 'pages': d.page_count(), 'engine': d.engine, 'links': d.extract_links()}))"
)


def _render(engine):
    env = dict(os.environ)
    env.pop("DISTILLPDF_ENGINE", None)
    if engine:
        env["DISTILLPDF_ENGINE"] = engine
    out = subprocess.run([sys.executable, "-c", _RENDER, FIXTURE],
                         env=env, capture_output=True, text=True, check=True)
    return json.loads(out.stdout.strip().splitlines()[-1])


def test_fixture_is_the_compressed_deletion_shape_it_claims():
    ground = GT["freed_compressed_link.pdf"]
    raw = open(FIXTURE, "rb").read()
    assert raw.count(b"startxref") == ground["revisions"] == 2
    # The container is a plain object with both links inside it, still readable, and it is
    # NOT rewritten by the deleting revision — so the leaked URI is physically in the file.
    assert b"/Type /ObjStm" in raw
    assert raw.count(b"\n%d 0 obj\n" % ground["container_object"]) == 1
    assert ground["freed_uri"].encode() in raw
    assert ground["kept_uri"].encode() in raw
    # The page still names the freed annotation, first, alongside the one that survives.
    assert b"/Annots [%d 0 R %d 0 R]" % (ground["freed_object"],
                                         ground["kept_object"]) in raw
    # Neither link is a plain object: both exist only as members of the container.
    for num in (ground["freed_object"], ground["kept_object"]):
        assert b"\n%d 0 obj\n" % num not in raw


def test_a_freed_compressed_link_stays_deleted_on_both_engines():
    ground = GT["freed_compressed_link.pdf"]
    for engine in (None, "indexed"):
        name = engine or "eager"
        result = _render(engine)
        assert result["pages"] == 1, name
        uris = [link.get("uri") for link in result["links"]]
        assert uris == [ground["kept_uri"]], (
            f"{name} reported {uris}; the freed annotation must be gone and the kept one kept"
        )
        # Both anchor sentences still render: the page's text is untouched by the deletion,
        # so a missing link cannot be explained away as a missing page.
        body = text(result["html"])
        for needle in ground["text_contains"]:
            assert needle in body, (name, needle)
        for needle in ground["uri_excludes"]:
            assert needle not in result["html"], f"{name} resurrected the freed annotation"


def test_engines_render_byte_identical_output():
    eager, indexed = _render(None), _render("indexed")
    assert indexed["sha"] == eager["sha"], (
        "eager and indexed disagree on the freed-compressed-member fixture"
    )
