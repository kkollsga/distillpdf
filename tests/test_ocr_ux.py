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
