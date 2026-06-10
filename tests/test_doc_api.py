"""The public ``Doc`` API (engine track, Wave 4): ``distillpdf.load("case.dpdf") -> Doc``.

``Doc`` is the single-document reading surface (Tier 2 of docs/datamodel-design.md) and a
THIN WRAPPER over the internal ``distillpdf.dpdf.Model`` the CLI shell uses — so the two must
not drift. These tests pin that contract:

  * every documented accessor returns the shape the design promises (pages / sections / toc /
    section / blocks / block / tables / figures / find / info / ocr_passes / ocr_status);
  * the NO-DRIFT properties: ``Doc.section(id)`` equals the CLI ``read`` markdown, and
    ``Doc.find`` matches the CLI ``find`` (same Model.find underneath);
  * the FIDELITY renders are byte-identical to ``distillpdf.render_html/markdown/text`` (the
    renderers are pure functions of the model — Doc just delegates);
  * the /PageLabels fixture distills and a label resolves to its physical page end-to-end,
    through both Doc and the CLI (the Wave-3 gap, now an owned fixture);
  * ``load`` on a non-.dpdf is an actionable error.

Fixtures are the owned, committed PDFs distilled to a .dpdf in tmp_path (like test_model.py /
test_dpdf_cli.py).
"""
import io
import json
import os
import sys

import pytest

import distillpdf
from distillpdf import cli
from distillpdf.dpdf import DpdfError
from _fixtures import FIX

SEC_STRUCT = os.path.join(FIX, "sec_structure.pdf")
HEADINGS = os.path.join(FIX, "headings.pdf")
NUMERIC = os.path.join(FIX, "numeric.pdf")
FIGURES = os.path.join(FIX, "figures.pdf")
PAGELABELS = os.path.join(FIX, "pagelabels.pdf")


def _dpdf(pdf, tmp_path, name="m.dpdf"):
    return distillpdf.Pdf.open(pdf).distill(os.path.join(str(tmp_path), name))


def _capture_cli(argv):
    """Run cli.main(argv) capturing stdout, return (rc, stdout)."""
    buf = io.StringIO()
    old = sys.stdout
    sys.stdout = buf
    try:
        rc = cli.main(argv)
    finally:
        sys.stdout = old
    return rc, buf.getvalue()


# ---- load / construct -------------------------------------------------------

def test_load_returns_doc_with_source_binding(tmp_path):
    d = _dpdf(SEC_STRUCT, tmp_path)
    doc = distillpdf.load(d)
    assert isinstance(doc, distillpdf.Doc)
    assert doc.source["file"] == "sec_structure.pdf"
    assert len(doc.source["sha256"]) == 64
    assert doc.schema_version == 0  # experimental
    assert "sec_structure.pdf" in repr(doc)


def test_doc_load_classmethod_equivalent(tmp_path):
    d = _dpdf(SEC_STRUCT, tmp_path)
    assert distillpdf.Doc.load(d).source == distillpdf.load(d).source


def test_load_on_non_dpdf_is_actionable(tmp_path):
    """Passing a PDF (or any non-container) to load raises DpdfError with a message — never a
    silent or opaque failure."""
    with pytest.raises(DpdfError):
        distillpdf.load(SEC_STRUCT)  # a real PDF, not a .dpdf container
    with pytest.raises(DpdfError):
        distillpdf.load(os.path.join(str(tmp_path), "nope.dpdf"))  # missing file


# ---- pages / sections / toc -------------------------------------------------

def test_pages_accessor(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    assert len(doc.pages) == distillpdf.Pdf.open(SEC_STRUCT).page_count()
    for p in doc.pages:
        assert p["n"] >= 1 and p["width_pts"] > 0 and p["height_pts"] > 0


def test_sections_accessor(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    assert doc.sections, "sec_structure has sections"
    for s in doc.sections:
        assert {"id", "level", "title", "page_start", "page_end"} <= set(s)


def test_toc_tuple_shape(tmp_path):
    """toc() yields (level, title, page_start, section-id) — the last element is the id that
    threads back into section(), not an HTML anchor."""
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    toc = doc.toc()
    assert toc
    for lvl, title, page, sid in toc:
        assert isinstance(lvl, int) and isinstance(title, str)
        assert isinstance(page, int)
        assert doc.model.section_by_id(sid) is not None  # the id resolves
    # and every id reads back
    assert doc.section(toc[0][3]).strip()


# ---- section (QUERY view) == CLI read (NO-DRIFT) ----------------------------

def test_section_equals_cli_read_markdown(tmp_path):
    """The one-source-of-truth guard: Doc.section(id) is byte-identical to what the CLI
    ``read <id>`` emits as its markdown body. Reading logic lives in Model; Doc and the CLI
    must not drift."""
    d = _dpdf(HEADINGS, tmp_path)
    doc = distillpdf.load(d)
    for sid in (s["id"] for s in doc.sections):
        ds = doc.section(sid)
        _, out = _capture_cli([d, "read", sid, "--json", "--max-chars", "10000000"])
        cli_md = json.loads(out)["markdown"]
        assert ds == cli_md, f"Doc.section({sid}) drifted from CLI read"


def test_section_unknown_id_actionable(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    with pytest.raises(DpdfError) as e:
        doc.section("sec-not-a-real-section")
    assert "unknown section" in str(e.value)


# ---- blocks / block ---------------------------------------------------------

def test_blocks_no_filter_is_all(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    assert len(doc.blocks()) == len(doc.model.blocks)


def test_blocks_filters_compose(tmp_path):
    doc = distillpdf.load(_dpdf(NUMERIC, tmp_path))
    tables = doc.blocks(kind="table")
    assert tables and all(b["kind"] == "table" for b in tables)
    # page filter
    p1 = doc.blocks(pages=[1])
    assert p1 and all(b["page"] == 1 for b in p1)


def test_blocks_section_filter_and_unknown(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    sid = doc.sections[0]["id"]
    scoped = doc.blocks(section=sid)
    assert scoped and 0 < len(scoped) <= len(doc.blocks())
    with pytest.raises(DpdfError):
        doc.blocks(section="sec-bogus")


def test_block_by_id_and_markdown(tmp_path):
    doc = distillpdf.load(_dpdf(NUMERIC, tmp_path))
    tid = doc.tables()[0]["id"]
    b = doc.block(tid)
    assert b["id"] == tid and b["kind"] == "table"
    md = doc.block_markdown(tid)
    assert "| System |" in md  # table → pipe markdown
    with pytest.raises(DpdfError):
        doc.block("b999999")


# ---- tables / figures -------------------------------------------------------

def test_tables_index(tmp_path):
    doc = distillpdf.load(_dpdf(NUMERIC, tmp_path))
    tables = doc.tables()
    assert tables
    for t in tables:
        assert {"id", "label", "page", "page_label", "caption"} <= set(t)
        assert doc.model.block_by_id(t["id"])["kind"] == "table"


def test_figures_index(tmp_path):
    doc = distillpdf.load(_dpdf(FIGURES, tmp_path))
    figs = doc.figures()
    assert figs and all(f["page"] >= 1 for f in figs)
    assert any(f.get("label") for f in figs)


def test_tables_match_cli_rows(tmp_path):
    """Doc.tables() carries the same id/label/page/caption rows the ``tables`` CLI verb emits
    (the CLI listing JSON is the Doc rows)."""
    d = _dpdf(NUMERIC, tmp_path)
    doc = distillpdf.load(d)
    _, out = _capture_cli([d, "tables", "--json"])
    cli_rows = json.loads(out)["tables"]
    assert cli_rows == doc.tables()


# ---- find (coverage-honest) == CLI find -------------------------------------

def test_find_returns_findresult_with_coverage(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    res = doc.find("business")
    assert res.hits
    assert res.searched_blocks > 0 and res.searched_pages > 0
    assert isinstance(res.no_text_pages, list)
    h = res.hits[0]
    assert h.block_id and h.page >= 1


def test_find_not_found_is_measured(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    res = doc.find("zzz_no_such_phrase_qqq")
    assert res.hits == []
    assert res.searched_blocks > 0  # "not found" paired with how much was searched


def test_find_regex_and_scopes(tmp_path):
    doc = distillpdf.load(_dpdf(NUMERIC, tmp_path))
    assert doc.find(r"\d+\.\d", regex=True).hits  # decimals
    assert all(h.block["kind"] == "table" for h in doc.find("84.6", kind="table").hits)


def test_find_limit_truncates_hits_not_coverage(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    full = doc.find("e")
    limited = doc.find("e", limit=1)
    assert len(limited.hits) == 1 < len(full.hits)
    # coverage is the full search either way (limit only bounds the returned hit list)
    assert limited.searched_blocks == full.searched_blocks


def test_find_unknown_section_actionable(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    with pytest.raises(DpdfError):
        doc.find("x", section="sec-bogus")


def test_find_parity_with_cli(tmp_path):
    """Doc.find and the CLI ``find`` run the same Model.find — the hit block ids and the
    coverage counts agree."""
    d = _dpdf(SEC_STRUCT, tmp_path)
    doc = distillpdf.load(d)
    res = doc.find("the")
    _, out = _capture_cli([d, "find", "the", "--json", "--limit", "1000000"])
    obj = json.loads(out)
    assert obj["total_matches"] == len(res.hits)
    assert obj["coverage"]["searched_blocks"] == res.searched_blocks
    assert obj["coverage"]["searched_pages"] == res.searched_pages
    assert [m["block_id"] for m in obj["matches"]] == [h.block_id for h in res.hits]


# ---- info / ocr -------------------------------------------------------------

def test_info_shape_and_cli_parity(tmp_path):
    d = _dpdf(NUMERIC, tmp_path)
    doc = distillpdf.load(d)
    info = doc.info()
    assert info["schema_version"] == 0
    assert info["pages"] == len(doc.pages)
    assert info["tables"] == 1
    assert len(info["source"]["sha256_prefix"]) == 12
    # the ``info --json`` CLI verb emits exactly this dict (one source of truth)
    _, out = _capture_cli([d, "info", "--json"])
    assert json.loads(out) == info


def test_ocr_passes_and_status_empty_for_born_digital(tmp_path):
    doc = distillpdf.load(_dpdf(SEC_STRUCT, tmp_path))
    assert doc.ocr_passes() == []
    status = doc.ocr_status()
    assert status == {"pages": [], "passes": []}  # born-digital → nothing recorded


def test_ocr_status_cli_parity(tmp_path):
    d = _dpdf(SEC_STRUCT, tmp_path)
    doc = distillpdf.load(d)
    _, out = _capture_cli([d, "ocr-status", "--json"])
    assert json.loads(out) == doc.ocr_status()


# ---- fidelity renders byte-equal to render_* --------------------------------

@pytest.mark.parametrize("pdf", [SEC_STRUCT, HEADINGS, NUMERIC, FIGURES],
                         ids=["sec_structure", "headings", "numeric", "figures"])
def test_to_html_byte_equals_render_html(pdf, tmp_path):
    d = _dpdf(pdf, tmp_path)
    doc = distillpdf.load(d)
    assert doc.to_html(return_string=True) == distillpdf.render_html(d)
    # page mode / no-toc options thread through identically
    assert doc.to_html(return_string=True, mode="page", toc=False) == \
        distillpdf.render_html(d, "page", False)


@pytest.mark.parametrize("pdf", [SEC_STRUCT, HEADINGS, NUMERIC, FIGURES],
                         ids=["sec_structure", "headings", "numeric", "figures"])
def test_to_markdown_byte_equals_render_markdown(pdf, tmp_path):
    d = _dpdf(pdf, tmp_path)
    doc = distillpdf.load(d)
    assert doc.to_markdown(return_string=True) == distillpdf.render_markdown(d)


def test_text_byte_equals_render_text(tmp_path):
    d = _dpdf(HEADINGS, tmp_path)
    doc = distillpdf.load(d)
    assert doc.text() == distillpdf.render_text(d)


def test_fidelity_render_writes_file_and_returns_path(tmp_path):
    d = _dpdf(HEADINGS, tmp_path)
    doc = distillpdf.load(d)
    out = doc.to_html()  # derives <stem>.html beside the .dpdf
    assert out.endswith("m.html") and os.path.exists(out)
    assert open(out, encoding="utf-8").read() == distillpdf.render_html(d)
    mdout = doc.to_markdown(os.path.join(str(tmp_path), "x.md"))
    assert mdout.endswith("x.md") and os.path.exists(mdout)


def test_fidelity_render_requires_path_when_in_memory():
    """A Doc built from an in-memory Model (no path) can't re-render the fidelity views — they
    read the container from disk. The error says how to fix it."""
    from distillpdf.dpdf import Model
    m = Model.from_dict({
        "source": {"file": "x.pdf", "sha256": "a", "pages": 0, "distillpdf": "0", "generated_at": "Z"},
        "metadata": {}, "pages": [], "sections": [], "blocks": [],
        "indexes": {"pages": {}, "sections": {}, "kinds": {}, "coverage": {"sectioned": 1.0}},
    })
    doc = distillpdf.Doc.from_model(m)
    with pytest.raises(DpdfError) as e:
        doc.to_html(return_string=True)
    assert "distillpdf.load" in str(e.value)
    # but the query-side accessors work fine without a path
    assert doc.info()["pages"] == 0


# ---- /PageLabels fixture end-to-end -----------------------------------------

def test_pagelabels_fixture_distills_with_labels(tmp_path):
    """The committed /PageLabels fixture distills and the model carries the roman/arabic page
    labels (front matter i, ii then body 1, 2)."""
    doc = distillpdf.load(_dpdf(PAGELABELS, tmp_path))
    assert doc.page_label(1) == "i"
    assert doc.page_label(2) == "ii"
    assert doc.page_label(3) == "1"  # arabic restart
    assert doc.page_label(4) == "2"


def test_pagelabels_resolve_label_to_physical_page(tmp_path):
    """A LABEL token resolves to its physical page — both via Doc and the CLI ``read --pages``.
    Roman labels are unambiguous (a bare integer is read as a physical page first)."""
    d = _dpdf(PAGELABELS, tmp_path)
    doc = distillpdf.load(d)
    assert doc.resolve_pages("ii") == [2]
    assert doc.resolve_pages("i-ii") == [1, 2]
    # the blocks on label page ii are physical page 2's blocks
    assert all(b["page"] == 2 for b in doc.blocks(pages=doc.resolve_pages("ii")))
    # CLI read --pages <label> resolves the same page
    _, out = _capture_cli([d, "read", "--pages", "ii", "--max-chars", "100000"])
    assert "front matter on roman page ii" in out


def test_pagelabels_unresolvable_is_loud(tmp_path):
    doc = distillpdf.load(_dpdf(PAGELABELS, tmp_path))
    with pytest.raises(DpdfError):
        doc.resolve_pages("x-99")  # neither token resolves


def test_pagelabels_cli_find_shows_label_suffix(tmp_path):
    """find on the labelled doc surfaces the page label beside the physical page (citations)."""
    d = _dpdf(PAGELABELS, tmp_path)
    _, out = _capture_cli([d, "find", "roman page ii"])
    assert "(ii)" in out  # the label is shown next to the physical page


# ---- the convert CLI distills a .dpdf output --------------------------------

def test_cli_distill_via_dpdf_output(tmp_path):
    """``distillpdf case.pdf -o case.dpdf`` distills the model (README quickstart) instead of
    rendering HTML to a .dpdf-named file."""
    out = os.path.join(str(tmp_path), "case.dpdf")
    rc = cli.main([SEC_STRUCT, "-o", out])
    assert rc == 0 and os.path.exists(out)
    # and it's a real loadable model
    doc = distillpdf.load(out)
    assert doc.source["file"] == "sec_structure.pdf"
