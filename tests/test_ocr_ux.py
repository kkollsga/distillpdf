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
    # engine= also accepts an already-constructed backend (passed through unchanged)
    be = ocr.get_backend("granite-docling-gguf")
    assert ocr.backend_for(be) is be


def _clear_hf_env(mp):
    mp.delenv("HF_TOKEN", raising=False)
    mp.delenv("HUGGING_FACE_HUB_TOKEN", raising=False)


def test_hf_token_priority(monkeypatch, tmp_path):
    _clear_hf_env(monkeypatch)
    env = str(tmp_path / ".env")
    # explicit token wins
    assert ocr.resolve_hf_token(ocr.OcrConfig(hf_token="explicit", env_file=env)) == "explicit"
    # then the HF_TOKEN env var
    monkeypatch.setenv("HF_TOKEN", "fromenv")
    assert ocr.resolve_hf_token(ocr.OcrConfig(env_file=env)) == "fromenv"
    # nothing set, no file → None (fine for public models)
    _clear_hf_env(monkeypatch)
    assert ocr.resolve_hf_token(ocr.OcrConfig(env_file=str(tmp_path / "absent.env"))) is None


def test_hf_token_from_dotenv(monkeypatch, tmp_path):
    _clear_hf_env(monkeypatch)
    env = tmp_path / ".env"
    env.write_text('# secrets\nFOO=bar\nexport HF_TOKEN="from_dotenv"\n')
    assert ocr.resolve_hf_token(ocr.OcrConfig(env_file=str(env))) == "from_dotenv"
    # only HF_TOKEN is read, not the rest of the file
    import os
    assert os.environ.get("FOO") != "bar"


def test_store_token_writes_then_reads_back(monkeypatch, tmp_path):
    _clear_hf_env(monkeypatch)
    env = str(tmp_path / ".env")
    # store_token persists an explicit token to the .env
    assert ocr.resolve_hf_token(ocr.OcrConfig(hf_token="secret", store_token=True, env_file=env)) == "secret"
    assert "HF_TOKEN=secret" in open(env).read()
    # a later run with no explicit token picks it up from that .env
    _clear_hf_env(monkeypatch)
    assert ocr.resolve_hf_token(ocr.OcrConfig(env_file=env)) == "secret"


def test_accurate_default_is_pytorch_off_apple_silicon(monkeypatch):
    import platform
    monkeypatch.setattr(platform, "system", lambda: "Windows")
    monkeypatch.setattr(platform, "machine", lambda: "AMD64")
    assert ocr._accurate_backend_name() == "granite-docling-pytorch"
    monkeypatch.setattr(platform, "system", lambda: "Linux")
    monkeypatch.setattr(platform, "machine", lambda: "x86_64")
    assert ocr._accurate_backend_name() == "granite-docling-pytorch"
    monkeypatch.setattr(platform, "system", lambda: "Darwin")
    monkeypatch.setattr(platform, "machine", lambda: "arm64")
    assert ocr._accurate_backend_name() == "granite-docling"  # MLX


def test_pytorch_device_picker_prefers_gpu():
    from distillpdf._backends_pytorch import _pick_device

    class _Backends:
        class mps:
            avail = False
            @classmethod
            def is_available(cls):
                return cls.avail

    class _Torch:
        class cuda:
            avail = False
            @classmethod
            def is_available(cls):
                return cls.avail
        backends = _Backends

    t = _Torch()
    # explicit choices are honored (even mps, though it's not auto-selected)
    assert _pick_device(t, "cpu") == "cpu"
    assert _pick_device(t, "cuda") == "cuda"
    assert _pick_device(t, "mps") == "mps"
    # auto: CPU when no CUDA (MPS is deliberately NOT auto-selected — unreliable for this model)
    assert _pick_device(t, "auto") == "cpu"
    _Backends.mps.avail = True
    assert _pick_device(t, "auto") == "cpu"
    _Torch.cuda.avail = True
    assert _pick_device(t, "auto") == "cuda"


def test_setup_help_is_os_and_engine_specific(monkeypatch):
    import platform
    # Windows
    monkeypatch.setattr(platform, "system", lambda: "Windows")
    monkeypatch.setattr(platform, "machine", lambda: "AMD64")
    pt = ocr.setup_help("granite-docling-pytorch")
    assert "pip install torch" in pt and "download.pytorch.org/whl/cu" in pt   # direct torch + CUDA hint
    gg = ocr.setup_help("granite-docling-gguf")
    assert "llama-cpp-python" in gg and "abetlen.github.io" in gg              # direct pkg + prebuilt-wheel hint
    mlx = ocr.setup_help("granite-docling")
    assert "Apple Silicon" in mlx                                              # MLX not usable on Windows
    assert "docs/ocr-setup.md" in pt and "docs/ocr-setup.md" in gg            # links the full guide
    assert "distillpdf[ocr]" not in pt and "distillpdf[ocr]" not in gg        # no fictional extra
    # Apple Silicon: no CUDA hint; MLX install is the direct mlx-vlm package
    monkeypatch.setattr(platform, "system", lambda: "Darwin")
    monkeypatch.setattr(platform, "machine", lambda: "arm64")
    assert "cu124" not in ocr.setup_help("granite-docling-pytorch")
    mlx = ocr.setup_help("granite-docling")
    assert "mlx-vlm" in mlx
    # MLX model is public + mlx-vlm-managed: no ./ocr_model/, no HF/token noise
    assert "downloads automatically" in mlx
    assert "HF_TOKEN" not in mlx and "ocr_model" not in mlx
    # PyTorch/GGUF download to a visible ./ocr_model/ and surface the token rule
    assert "ocr_model" in gg and "HF_TOKEN" in gg                             # GGUF uses ./ocr_model/
    assert "ocr_model" in pt                                                   # PyTorch uses ./ocr_model/


def test_require_uses_hint(monkeypatch):
    # the dependency error embeds the OS/engine setup guidance
    with pytest.raises(ocr.OcrDependencyError) as ei:
        ocr._require("definitely_absent_mod", package="torch", hint=ocr.setup_help("granite-docling-pytorch"))
    msg = str(ei.value)
    assert "torch" in msg and "pip install" in msg and "docs/ocr-setup.md" in msg


def test_install_help_selector(monkeypatch):
    # the user-facing helper: fast tier is bundled (nothing); accurate resolves to a runtime
    assert "bundled" in ocr.install_help("fast").lower()
    assert "bundled" in ocr.install_help("tesseract").lower()
    acc = ocr.install_help("granite")
    assert "pip install" in acc and "docs/ocr-setup.md" in acc
    assert ocr.install_help("accurate") == ocr.install_help("granite")


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


def test_doctags_text_len_strips_tags():
    # a true image (no text) reads as 0; tags don't count toward the length
    assert ocr._doctags_text_len("") == 0
    assert ocr._doctags_text_len("<loc_1><loc_2><loc_3><loc_4>") == 0
    assert ocr._is_text_image("<loc_1><loc_2><loc_3><loc_4>Encaminhe-se a Reclamacao")
    assert not ocr._is_text_image("<loc_1><loc_2><loc_3><loc_4>x")  # one stray char


class _FakeGate:
    """Stand-in for the bundled Tesseract gate: returns canned (raw_words, conf_chars)."""
    def __init__(self, stats):
        self._stats = stats

    def classify(self, image):
        return self._stats


def test_gate_routes_text_vs_image(monkeypatch):
    # a page with confident text -> run the model
    assert ocr._gate_says_text(_FakeGate((400, 2500)), b"img")
    # many low-confidence words (a faded-but-legible scan) -> run the model
    assert ocr._gate_says_text(_FakeGate((120, 0)), b"img")
    # Tesseract reads nothing: the ink rescue decides. Substantial ink -> run the model
    monkeypatch.setattr(ocr, "_image_has_content", lambda img: True)
    assert ocr._gate_says_text(_FakeGate((0, 0)), b"img")
    # ...blank/near-blank -> skip (stays an image)
    monkeypatch.setattr(ocr, "_image_has_content", lambda img: False)
    assert not ocr._gate_says_text(_FakeGate((0, 0)), b"img")


def test_image_ink_permille_blank_vs_inked():
    # the rescue signal: a near-blank image is ~0, an inked one is well above the floor
    from distillpdf import _distillpdf
    import io
    try:
        from PIL import Image
    except Exception:
        import pytest
        pytest.skip("PIL not available to synthesize a test image")
    white = io.BytesIO(); Image.new("RGB", (64, 64), "white").save(white, "PNG")
    black = io.BytesIO(); Image.new("RGB", (64, 64), "black").save(black, "PNG")
    assert _distillpdf.image_ink_permille(white.getvalue()) < ocr._RESCUE_MIN_INK_PERMILLE
    assert _distillpdf.image_ink_permille(black.getvalue()) >= ocr._RESCUE_MIN_INK_PERMILLE
