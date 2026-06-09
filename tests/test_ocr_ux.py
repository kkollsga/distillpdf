"""OCR usability surface that needs NO model / no [ocr] extra (CI-safe):

- the default tqdm progress is silent on a non-TTY (so pipes/CI/tests stay clean) and
  active under a TTY;
- the large-embed HTML warning fires only for image_mode='embed' above the size cap;
- the `--ocr` CLI flag parses and wires up (driven on a born-digital fixture, which has
  zero scanned pages, so the model is never loaded).

The actual model run is exercised manually/locally (it needs the [ocr] extra + a download).
"""
import os
import types
import warnings

import pytest

import distillpdf
from distillpdf import cli, ocr
from distillpdf.document import _LARGE_HTML
from _fixtures import FIX

HEADINGS = os.path.join(FIX, "headings.pdf")  # born-digital: 0 scanned pages


# -- default progress --------------------------------------------------------

def test_auto_progress_silent_on_non_tty():
    # Under pytest stderr is not a TTY, so the default progress must be a no-op (None, None).
    cb, closer = ocr._auto_progress(10)
    assert cb is None and closer is None


def test_auto_progress_zero_pages_is_noop():
    assert ocr._auto_progress(0) == (None, None)


def test_auto_progress_bar_on_tty(monkeypatch):
    tqdm = pytest.importorskip("tqdm")  # ships with huggingface-hub; skip if base-only env
    monkeypatch.setattr("sys.stderr.isatty", lambda: True)
    cb, closer = ocr._auto_progress(3)
    assert callable(cb) and callable(closer)
    cb(1, 1, 3)  # first page flips the label off "loading model"; must not raise
    cb(2, 2, 3)
    closer()


# -- large-embed warning -----------------------------------------------------

def _doc():
    return distillpdf.open(HEADINGS)


def test_large_embed_warns_on_big_string():
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        _doc()._warn_large_embed(None, "x" * (_LARGE_HTML + 1), "embed")
    assert any("embedded inline" in str(x.message) for x in w)


def test_large_embed_silent_when_external():
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        _doc()._warn_large_embed(None, "x" * (_LARGE_HTML + 1), "external")
    assert not w


def test_large_embed_silent_when_small():
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        _doc()._warn_large_embed(None, "x" * 1000, "embed")
    assert not w


def test_large_embed_sizes_the_file_not_the_path(tmp_path):
    # When writing to disk the result IS the path string; the warning must size the FILE.
    big = tmp_path / "big.html"
    big.write_text("x" * (_LARGE_HTML + 1024))
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        _doc()._warn_large_embed(str(big), str(big), "embed")
    assert any("embedded inline" in str(x.message) for x in w)


# -- CLI --ocr ---------------------------------------------------------------

def _args(output=None, markdown=False):
    return types.SimpleNamespace(output=output, markdown=markdown)


def test_ocr_fmt_default_is_pdf():
    assert cli._ocr_fmt(_args(), None) == "pdf"


def test_ocr_fmt_from_extension():
    assert cli._ocr_fmt(_args(), "out.pdf") == "pdf"
    assert cli._ocr_fmt(_args(), "out.html") == "html"
    assert cli._ocr_fmt(_args(), "out.md") == "md"


def test_ocr_fmt_markdown_flag():
    assert cli._ocr_fmt(_args(markdown=True), None) == "md"


def test_ocr_out_path_beside_source():
    p = cli._ocr_out_path("/some/dir/scan.pdf", _args(), multiple=False, fmt="pdf")
    assert p == os.path.join("/some/dir", "scan.searchable.pdf")


def test_ocr_out_path_explicit_output():
    p = cli._ocr_out_path("scan.pdf", _args(output="custom.pdf"), multiple=False, fmt="pdf")
    assert p == "custom.pdf"


def test_cli_ocr_on_born_digital_pdf_is_noop(capsys):
    # headings.pdf has no scanned pages → the CLI reports nothing to OCR and never touches a
    # model, so this is safe without the [ocr] extra.
    rc = cli.main([HEADINGS, "--ocr"])
    assert rc == 0
    err = capsys.readouterr().err
    assert "nothing to OCR" in err


# -- engine interface: capabilities, tiers, native registry ------------------

def test_default_tier_falls_back_to_accurate_without_fast(monkeypatch):
    # With no "tesseract" engine registered, the fast tier falls back to the platform
    # granite (accurate) backend — preserving today's behavior.
    monkeypatch.delitem(ocr._BACKENDS, "tesseract", raising=False)
    assert ocr.default_backend_name() == ocr._accurate_backend_name()
    assert ocr.default_backend_name("fast") == ocr._accurate_backend_name()


def test_default_tier_is_fast_when_engine_present(monkeypatch):
    # Register a throwaway "tesseract" backend; the default tier 'fast' must pick it.
    class _Dummy(ocr.OcrBackend):
        name = "tesseract"; tier = "fast"; bundled = True
        def ocr_page(self, image):  # pragma: no cover - never called
            return ""
    monkeypatch.setitem(ocr._BACKENDS, "tesseract", _Dummy)
    assert ocr.default_backend_name() == "tesseract"
    assert ocr.default_backend_name("fast") == "tesseract"
    # explicit accurate tier still bypasses it
    assert ocr.default_backend_name("accurate") == ocr._accurate_backend_name()


def test_unknown_tier_raises():
    with pytest.raises(ValueError):
        ocr.default_backend_name("turbo")


def test_get_backend_name_overrides_tier():
    # an explicit name wins over any tier
    be = ocr.get_backend(name="granite-docling-gguf", tier="fast")
    assert be.name == "granite-docling-gguf"


def test_legacy_names_still_resolve():
    for nm in ("granite-docling", "granite-docling-gguf"):
        assert ocr.get_backend(nm).name == nm


def test_backend_descriptors_shape_and_import_light():
    # Reading capabilities must not import any heavy backend dep.
    import sys
    for mod in ("mlx_vlm", "llama_cpp", "torch"):
        assert mod not in sys.modules or True  # tolerate pre-imported; we assert no NEW import
    before = set(sys.modules)
    rows = ocr.backend_descriptors()
    after = set(sys.modules)
    assert not ({"mlx_vlm", "llama_cpp", "torch"} & (after - before))
    assert rows and all(isinstance(d, ocr.OcrCapabilities) for d in rows)
    d = {r.name: r for r in rows}
    # the granite backends self-describe as accurate + structure-aware + not bundled
    assert d["granite-docling"].tier == "accurate"
    assert d["granite-docling"].structure_aware is True
    assert d["granite-docling"].bundled is False
    assert d["granite-docling"].output == "doctags"


def test_backend_for_engine_selector():
    # the user-facing engine= selector: tier words, the 'granite' alias, and explicit names
    assert ocr.backend_for("granite-docling-gguf").name == "granite-docling-gguf"
    assert ocr.backend_for("granite").name == ocr.backend_for("accurate").name
    assert ocr.backend_for("accurate").tier == "accurate"
    # default (None / "fast") resolves to whatever the fast tier is in this build
    assert ocr.backend_for().name == ocr.default_backend_name("fast")
    assert ocr.backend_for("fast").name == ocr.default_backend_name("fast")


def test_native_server_engine_registered():
    # The native "server" engine is compiled in and surfaces in the unified registry.
    from distillpdf import _distillpdf as core
    assert "server" in core.native_engines()
    assert "server" in ocr.available_backends()
    be = ocr.get_backend("server")
    assert be.tier == "accurate" and be.engine == "server"


def test_cli_list_ocr_engines(capsys):
    rc = cli.main(["x.pdf", "--list-ocr-engines"])
    assert rc == 0
    out = capsys.readouterr().out
    assert "name" in out and "tier" in out and "granite-docling" in out


def _tesseract_compiled_in():
    from distillpdf import _distillpdf as core
    return "tesseract" in core.native_engines()


@pytest.mark.skipif(not _tesseract_compiled_in(), reason="tesseract feature not in this build")
def test_language_detection_maps_to_bundled_codes():
    from distillpdf._distillpdf import detect_language
    assert detect_language("The quick brown fox jumps over the lazy dog every morning today") == "eng"
    assert detect_language("Os promotores de justiça apresentaram a denúncia ao tribunal hoje") == "por"
    assert detect_language("xy 12 ...") is None  # too short / unreliable → keep all bundled


@pytest.mark.skipif(not _tesseract_compiled_in(), reason="tesseract feature not in this build")
def test_prepare_respects_explicit_languages():
    # an explicit languages config is never overridden by detection
    be = ocr.get_backend("tesseract", config=ocr.OcrConfig(languages=["eng"]))
    be.prepare([])  # no samples; must not change the explicit choice
    assert be.config.languages == ["eng"]


@pytest.mark.skipif(not _tesseract_compiled_in(), reason="tesseract feature not in this build")
def test_tesseract_is_the_fast_default_when_compiled_in():
    # In a wheel built with the tesseract feature, the fast tier (and the bare default) is it.
    assert "tesseract" in ocr.available_backends()
    assert ocr.default_backend_name() == "tesseract"
    assert ocr.default_backend_name("fast") == "tesseract"
    be = ocr.get_backend()
    assert be.name == "tesseract" and be.tier == "fast"
    d = {r.name: r for r in ocr.backend_descriptors()}["tesseract"]
    assert d.tier == "fast" and d.bundled and d.offline and not d.structure_aware
