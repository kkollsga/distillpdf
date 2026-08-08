"""The public ``engine=`` keyword and the ``Pdf.engine`` property.

``engine=`` is the supported way to ask for the lazy (indexed) access engine;
``DISTILLPDF_ENGINE`` remains internal and unstable and only decides what an open that named
*no* engine does. The contract this file pins:

* ``engine="lazy"`` renders byte-identically to ``engine="eager"`` — the engine is a memory
  strategy, never a fidelity choice;
* ``Pdf.engine`` reports the route that **actually ran**, so a document the index refuses reads
  ``"lazy (eager fallback)"`` rather than claiming a lazy open that never happened;
* an explicit engine wins over the environment, which is why every assertion here holds under
  ``DISTILLPDF_ENGINE=indexed`` too;
* a misspelled engine is a ``ValueError`` that names the values that would have worked.

The fallback fixture is generated here rather than committed: every fixture under
``tests/fixtures_pdf/`` opens indexed, so a refusal has to be constructed. A catalog object
padded past the indexed reader's 4 MiB per-object decode envelope is the smallest honest one —
a perfectly valid PDF that the eager engine reads without complaint.
"""
import os

import pytest

import distillpdf
from _fixtures import FIX
from _rawpdf import HELV, assemble_pdf, stream, text_op

HEADINGS = os.path.join(FIX, "headings.pdf")

#: What an open that names no engine must route to, given this process's environment. The
#: public default is eager; the internal selector is the only thing that can move it.
ENV_DEFAULT = "lazy" if os.environ.get("DISTILLPDF_ENGINE") == "indexed" else "eager"

_PAD_BYTES = 5 * 1024 * 1024  # > the indexed reader's 4 MiB per-object envelope


@pytest.fixture(scope="module")
def oversized_catalog(tmp_path_factory):
    """A valid PDF whose /Catalog is too large for the indexed reader to decode."""
    path = str(tmp_path_factory.mktemp("engine") / "oversized-catalog.pdf")
    assemble_pdf({
        1: b"<< /Type /Catalog /Pages 2 0 R /Pad (" + b"x" * _PAD_BYTES + b") >>",
        2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        3: (b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"),
        4: stream(text_op(72, 700, "fallback page")),
        5: HELV,
    }, path)
    return path


def test_default_open_names_no_engine_and_follows_the_environment():
    assert distillpdf.Pdf.open(HEADINGS).engine == ENV_DEFAULT
    with open(HEADINGS, "rb") as f:
        assert distillpdf.Pdf.from_bytes(f.read()).engine == ENV_DEFAULT


def test_lazy_engine_opens_lazily_and_renders_what_eager_renders():
    lazy = distillpdf.Pdf.open(HEADINGS, engine="lazy")
    eager = distillpdf.Pdf.open(HEADINGS, engine="eager")
    assert lazy.engine == "lazy"
    assert eager.engine == "eager"
    assert lazy.page_count() == eager.page_count()
    assert lazy.extract_text() == eager.extract_text()
    assert lazy.to_html(return_string=True) == eager.to_html(return_string=True)


def test_an_explicit_engine_wins_over_the_environment():
    """Both directions, so this holds whichever mode the suite is being run in."""
    assert distillpdf.Pdf.open(HEADINGS, engine="eager").engine == "eager"
    assert distillpdf.Pdf.open(HEADINGS, engine="lazy").engine == "lazy"


def test_lazy_engine_takes_the_same_bytes_as_a_path():
    with open(HEADINGS, "rb") as f:
        data = f.read()
    assert distillpdf.Pdf.from_bytes(data, engine="lazy").engine == "lazy"
    assert (distillpdf.Pdf.from_bytes(data, engine="lazy").extract_text()
            == distillpdf.Pdf.from_bytes(data, engine="eager").extract_text())


def test_a_refused_lazy_open_falls_back_to_eager_and_says_so(oversized_catalog):
    """The fallback is a route change, not a content change — and it is never silent."""
    doc = distillpdf.Pdf.open(oversized_catalog, engine="lazy")
    assert doc.engine == "lazy (eager fallback)"
    assert doc.page_count() == 1
    assert "fallback page" in doc.extract_text()
    # Same document, same output, honest label: eager was not *asked* for, so it is not claimed.
    eager = distillpdf.Pdf.open(oversized_catalog, engine="eager")
    assert eager.engine == "eager"
    assert doc.extract_text() == eager.extract_text()


def test_a_refused_lazy_open_from_bytes_also_falls_back(oversized_catalog):
    with open(oversized_catalog, "rb") as f:
        data = f.read()
    assert distillpdf.Pdf.from_bytes(data, engine="lazy").engine == "lazy (eager fallback)"


@pytest.mark.parametrize("bad", ["indexed", "indexed-strict", "lazy-strict", "LAZY", "", "fast"])
def test_an_unknown_engine_raises_value_error_listing_the_allowed_values(bad):
    for call in (lambda: distillpdf.Pdf.open(HEADINGS, engine=bad),
                 lambda: distillpdf.Pdf.from_bytes(b"%PDF-1.4\n", engine=bad)):
        with pytest.raises(ValueError) as exc:
            call()
        assert 'expected "eager" or "lazy"' in str(exc.value)
        assert repr(bad).strip("'") in str(exc.value) or f'"{bad}"' in str(exc.value)


def test_engine_is_keyword_only_so_it_cannot_be_passed_by_accident():
    with pytest.raises(TypeError):
        distillpdf.Pdf.open(HEADINGS, "lazy")


def test_the_public_wrappers_take_the_same_keyword():
    """``distillpdf.open`` / ``.from_bytes`` return a Document, which delegates ``engine``."""
    assert distillpdf.open(HEADINGS, engine="lazy").engine == "lazy"
    with open(HEADINGS, "rb") as f:
        assert distillpdf.from_bytes(f.read(), engine="lazy").engine == "lazy"
    with pytest.raises(ValueError):
        distillpdf.open(HEADINGS, engine="indexed")
