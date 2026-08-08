"""The internal lazy (indexed) engine renders a pure function of the PDF.

``DISTILLPDF_ENGINE=indexed`` is internal and unstable — the public default is eager — but
whatever it renders must be *the same thing*, every run and on every machine. It was not:
compressed-object resolution drew its allowance from one process-wide 64 MiB budget with
"however many bytes happen to be free right now", so at one rayon thread every object
resolved and at N threads concurrent page workers took race-dependent slices, resolution
failed with a resource limit, and the consumers' legacy empty-page/empty-resources
suppression silently *shortened* the page. ``objstm_pages.pdf`` rendered 25,044 / 31,867 /
31,929 / 31,960 bytes in four consecutive runs against 93,708 from the eager route.

``objstm_pages.pdf`` (``tests/gen_fixtures.py::gen_objstm_pages``) is the owned repro: 40
pages whose page dictionaries, ``/Resources`` and indirect ``/MediaBox`` arrays all live in
two ``/ObjStm`` containers behind an xref stream, so every page costs several compressed
resolutions and the pages are extracted in parallel.

The engine is selected once per process from the environment, so each render runs in a fresh
child with the variable set explicitly — including the eager baseline, which must be pinned
even when the whole suite is being run under ``DISTILLPDF_ENGINE=indexed``.
"""
import hashlib
import json
import os
import subprocess
import sys

from _fixtures import FIX, GT, doc, text

FIXTURE = os.path.join(FIX, "objstm_pages.pdf")
RUNS = 3
_RENDER = (
    "import hashlib, json, sys, distillpdf;"
    "h = distillpdf.Pdf.open(sys.argv[1])"
    ".to_html(mode='page', return_string=True, image_mode='drop').encode();"
    "print(json.dumps({'bytes': len(h), 'sha': hashlib.sha256(h).hexdigest()}))"
)


def _render(engine, threads=None):
    env = dict(os.environ)
    env.pop("DISTILLPDF_ENGINE", None)
    env.pop("RAYON_NUM_THREADS", None)
    if engine:
        env["DISTILLPDF_ENGINE"] = engine
    if threads:
        env["RAYON_NUM_THREADS"] = str(threads)
    out = subprocess.run([sys.executable, "-c", _RENDER, FIXTURE],
                         env=env, capture_output=True, text=True, check=True)
    return json.loads(out.stdout.strip().splitlines()[-1])


def test_objstm_fixture_is_intact():
    """The committed fixture really is the many-compressed-object shape the rest asserts on."""
    ground = GT["objstm_pages.pdf"]
    d = doc("objstm_pages.pdf")
    assert d.page_count() == ground["pages"]
    body = text(d.to_html(mode="page", return_string=True, image_mode="drop"))
    for needle in ground["text_contains"]:
        assert needle in body, needle
    assert ground["compressed_objects"] > 100 and ground["object_streams"] == 2


def test_indexed_render_is_stable_and_matches_eager():
    eager = _render(None)
    indexed = [_render("indexed", threads=4) for _ in range(RUNS)]
    assert {row["sha"] for row in indexed} == {indexed[0]["sha"]}, (
        f"indexed rendered {len({row['sha'] for row in indexed})} distinct outputs over "
        f"{RUNS} runs at 4 threads: {[row['bytes'] for row in indexed]}"
    )
    assert indexed[0]["sha"] == eager["sha"], (
        f"indexed {indexed[0]['bytes']} bytes vs eager {eager['bytes']} bytes"
    )
    # …and the thread count is not what makes it agree.
    assert _render("indexed", threads=1)["sha"] == eager["sha"]


def test_eager_baseline_is_itself_stable():
    """Guards the comparison above: a flaky eager side would hide an indexed regression."""
    digests = {_render(None)["sha"] for _ in range(2)}
    assert len(digests) == 1
    assert hashlib.sha256(open(FIXTURE, "rb").read()).hexdigest()  # fixture is readable
