"""THE ROUND-TRIP REGRESSION (engine track, Wave 2) — the centerpiece.

A `.dpdf` model is the durable analysis snapshot; renderers are pure functions of it. The
contract this file pins, for EVERY owned fixture:

    to_html(parse(pdf))  ==  render_html(load(save(distill(pdf))))

with NO source PDF present on the render side — render runs from the `.dpdf` file alone. The
same for Markdown. This proves "renderers are pure functions of the model" by construction,
not by a parallel implementation that could drift.

THE EXACT INVARIANT (what's byte-equal, and the one documented residue):

* **HTML — BYTE-IDENTICAL.** For every owned fixture, every mode ("page"/"section") and every
  `toc` setting, `render_html(.dpdf)` equals `to_html(pdf, image_mode="drop")` byte-for-byte.
  This is exact because the model captures each page's verbatim PRE-id body
  (`Page.body_html`) at distill time and the model re-render runs the IDENTICAL `assemble`
  tail (id-minting + `<nav>` + image substitution) as the parse path — same code, only the
  body source differs.

* **Markdown — BYTE-IDENTICAL.** `to_markdown` is the HTML→Markdown transform; running it over
  the byte-identical HTML gives byte-identical Markdown.

* **Image mode is `"drop"`** on both sides. The Wave-1/2 born-digital model drops figure bytes
  (a regenerable stub — a named, reversible hole), so it carries `<image N>` placeholders, not
  pixel data. Comparing in `"drop"` mode is the honest, encoding-independent comparison;
  embed/external re-render awaits asset-byte capture (Wave 3/4). `to_html`'s DEFAULT
  (`image_mode="embed"`) is therefore not the comparison point — "drop" is, and it is the
  strongest byte-exact invariant the Wave-2 model supports.

* **Plain text — SELF-CONSISTENT with the rendered HTML (exact), NOT byte/token-equal to
  `Pdf.extract_text()` (the one documented residue).** `extract_text` is a separate POSITIONAL
  extractor (raw text spans, its own word-break/blank-line heuristics, its own reading order) —
  a different code path from rendering, with no HTML in between. It legitimately DIVERGES from
  any render-derived text in three named ways: (a) it does NOT de-hyphenate line-break hyphens
  the renderer rejoins (`architec`+`ture` vs `architecture`); (b) it surfaces SVG axis/label
  text the render treats as figure-internal; (c) it reads multi-column / tabular regions in a
  different order than the rendered structure, and sometimes omits table headers the render
  includes. So byte- or token-equality with `extract_text` is the WRONG target — the render is
  often the more faithful view. The exact invariant we DO assert is the meaningful one:
  `render_text(.dpdf)` carries the same token multiset as the visible text of the model's own
  rendered page-mode HTML — i.e. text is a consistent function of the same model as HTML — and
  is a deterministic pure function of the file. (Token agreement with `extract_text` does hold
  for the majority of fixtures; it is asserted as a sanity FLOOR, not exact equality.)
"""
import os
import re
from collections import Counter

import distillpdf
import pytest

import _htmlcheck as hc

OWNED = hc.owned_pdfs()
IDS = [os.path.basename(p) for p in OWNED]

MODES = ["section", "page"]
TOCS = [True, False]


def _visible_tokens(html):
    """Visible-text tokens of an HTML fragment, tokenized exactly as render_text is: SVG
    subtrees dropped, every tag → a boundary, `\\w+` tokens."""
    s = re.sub(r"<svg\b.*?</svg>", " ", html, flags=re.DOTALL)
    s = re.sub(r"<[^>]+>", " ", s)
    for a, b in (("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#39;", "'")):
        s = s.replace(a, b)
    return re.findall(r"\w+", s, re.UNICODE)


def _distill(pdf_path, tmp_path):
    return distillpdf.Pdf.open(pdf_path).distill(
        os.path.join(str(tmp_path), os.path.basename(pdf_path) + ".dpdf")
    )


# ---- HTML: byte-identical, every mode × toc, over every owned fixture --------

@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_html_roundtrip_byte_identical(path, tmp_path):
    d = distillpdf.Pdf.open(path)
    dpdf = _distill(path, tmp_path)
    for mode in MODES:
        for toc in TOCS:
            want = d.to_html(mode=mode, toc=toc, image_mode="drop", return_string=True)
            got = distillpdf.render_html(dpdf, mode=mode, toc=toc)
            assert want == got, (
                f"{os.path.basename(path)} mode={mode} toc={toc}: "
                f"model re-render diverged from to_html (len {len(want)} vs {len(got)})"
            )


# ---- Markdown: byte-identical (the same HTML transform over the same HTML) ----

@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_markdown_roundtrip_byte_identical(path, tmp_path):
    d = distillpdf.Pdf.open(path)
    dpdf = _distill(path, tmp_path)
    for mode in MODES:
        for toc in TOCS:
            want = d.to_markdown(mode=mode, toc=toc, image_mode="drop", return_string=True)
            got = distillpdf.render_markdown(dpdf, mode=mode, toc=toc, image_mode="drop")
            assert want == got, (
                f"{os.path.basename(path)} mode={mode} toc={toc}: "
                f"model Markdown diverged from to_markdown"
            )


# ---- Plain text: token-equivalent (the documented residue) -------------------

@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_text_is_consistent_function_of_the_model(path, tmp_path):
    """The EXACT, universal text invariant (see module docstring): render_text(.dpdf) carries
    the same token multiset as the visible text of the model's own rendered page-mode HTML —
    text and HTML are consistent views of the SAME model — and render_text is deterministic."""
    dpdf = _distill(path, tmp_path)
    rt = distillpdf.render_text(dpdf)
    assert rt == distillpdf.render_text(dpdf), "render_text must be a pure function of the file"

    page_html = distillpdf.render_html(dpdf, mode="page", toc=False)
    body = page_html.split("<body>", 1)[1].rsplit("</body>", 1)[0]
    assert Counter(re.findall(r"\w+", rt, re.UNICODE)) == Counter(_visible_tokens(body)), (
        f"{os.path.basename(path)}: render_text tokens diverge from the rendered HTML's visible "
        "text — text is not a consistent view of the same model"
    )


def test_text_token_overlap_with_extract_text_is_high():
    """SANITY FLOOR (not exact — see module docstring): across the owned corpus, render_text and
    the positional extract_text share the large majority of their token multiset. This catches a
    gross regression (e.g. render_text dropping all table cells) without asserting an equality
    that legitimately fails on de-hyphenation / SVG labels / tabular reading order."""
    import tempfile

    td = tempfile.mkdtemp()
    shared = total = 0
    for path in OWNED:
        d = distillpdf.Pdf.open(path)
        dpdf = d.distill(os.path.join(td, os.path.basename(path) + ".dpdf"))
        a = Counter(hc.words(d.extract_text()))
        b = Counter(hc.words(distillpdf.render_text(dpdf)))
        shared += sum((a & b).values())
        total += sum(a.values())
    assert total > 0
    assert shared / total >= 0.9, f"render_text shares only {shared / total:.0%} of extract_text tokens"


# ---- the model re-render needs NO source PDF (the durability claim) ----------

def test_render_needs_no_source_pdf(tmp_path):
    """The whole point: re-render from the .dpdf with the source PDF GONE."""
    import shutil

    src = tmp_path / "doc.pdf"
    shutil.copy(OWNED[0], src)
    d = distillpdf.Pdf.open(str(src))
    want = d.to_html(mode="section", toc=True, image_mode="drop", return_string=True)
    dpdf = d.distill(str(tmp_path / "doc.dpdf"))
    del d
    src.unlink()  # delete the source PDF — the model must stand alone
    assert not src.exists()
    got = distillpdf.render_html(dpdf, mode="section", toc=True)
    assert got == want, "render_html must reproduce to_html from the .dpdf alone"
