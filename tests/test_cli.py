"""The `distillpdf` command-line interface (distillpdf.cli): HTML/text/toc/section
rendering, the --no-images placeholder, stdout vs -o file vs batch directory output,
and non-zero exit on bad input. Drives cli.main() in-process (no subprocess)."""
import os

import pytest

from distillpdf import cli
from _fixtures import FIX

HEADINGS = os.path.join(FIX, "headings.pdf")
FIGURES = os.path.join(FIX, "figures.pdf")


def test_html_to_stdout(capsys):
    rc = cli.main([HEADINGS])
    assert rc == 0
    out = capsys.readouterr().out
    assert out.startswith("<!doctype html>") and "</html>" in out


def test_no_images_placeholder(capsys):
    cli.main([FIGURES, "--no-images"])
    out = capsys.readouterr().out
    assert "data:image" not in out and "<img " not in out
    assert "<image 1>" in out


def test_no_toc_omits_nav(capsys):
    cli.main([HEADINGS, "--no-toc"])
    out = capsys.readouterr().out
    assert "<nav>" not in out
    assert 'id="sec-' in out  # heading anchors still present


def test_text_mode(capsys):
    cli.main([HEADINGS, "--text"])
    out = capsys.readouterr().out
    assert "<html>" not in out
    assert "Introduction" in out


def test_toc_mode(capsys):
    cli.main([HEADINGS, "--toc"])
    out = capsys.readouterr().out
    assert "#sec-" in out and "(p1" in out


def test_output_file(tmp_path, capsys):
    dest = tmp_path / "out.html"
    rc = cli.main([HEADINGS, "-o", str(dest)])
    assert rc == 0
    assert dest.read_text().startswith("<!doctype html>")
    assert capsys.readouterr().out == ""  # nothing on stdout when writing a file


def test_batch_to_directory(tmp_path):
    outdir = tmp_path / "out"
    rc = cli.main([HEADINGS, FIGURES, "-o", str(outdir)])
    assert rc == 0
    assert (outdir / "headings.html").exists()
    assert (outdir / "figures.html").exists()


def test_bad_path_returns_nonzero(capsys):
    rc = cli.main([os.path.join(FIX, "does_not_exist.pdf")])
    assert rc == 1
    assert "does_not_exist.pdf" in capsys.readouterr().err


def test_missing_section_exits(capsys):
    with pytest.raises(SystemExit):
        cli.main([HEADINGS, "--section", "no_such_section_xyz"])
