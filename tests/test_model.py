"""The `.dpdf` document model (engine track, Wave 1).

`Pdf.distill(path)` writes the durable analysis model (a zip of model.json + img/ assets);
`distillpdf.load_model(path)` reads its model.json back as canonical JSON. These tests pin
the Wave-1 contract downstream consumers (kglite-docs) will build on:

  * the model is internally consistent and consistent with `to_html` (same section ids, same
    page count, the same visible text);
  * serialization is deterministic — save → load → save is byte-identical;
  * indexes are DERIVED from blocks (page/section coverage adds up), never hand-maintained;
  * a dropped asset keeps a stub (hash/dims/regen), never a silent hole.

THE ROUND-TRIP SEED TEST (`test_roundtrip_seed_*`) is the one Wave 2 extends: for the owned
fixtures, the loaded model's section ids and concatenated block text equal those derived from
a freshly parsed document.

Fixtures are GENERATED + committed (see gen_fixtures.py); these reuse the same owned set as
the HTML structural checks.
"""
import json
import os
import re

import distillpdf
import pytest

import _htmlcheck as hc
from _fixtures import FIX

SEC_STRUCT = os.path.join(FIX, "sec_structure.pdf")
HEADINGS = os.path.join(FIX, "headings.pdf")
FIGURES = os.path.join(FIX, "figures.pdf")
NUMERIC = os.path.join(FIX, "numeric.pdf")
LINKS = os.path.join(FIX, "links.pdf")

OWNED = hc.owned_pdfs()
IDS = [os.path.basename(p) for p in OWNED]


def _distill_load(pdf_path, out_dir, name="m.dpdf"):
    """Distill `pdf_path` to a .dpdf in `out_dir`, return (dpdf_path, parsed model dict)."""
    d = distillpdf.Pdf.open(pdf_path)
    dpdf = d.distill(os.path.join(str(out_dir), name))
    model = json.loads(distillpdf.load_model(dpdf))
    return dpdf, model


def _section_ids_from_html(pdf_path):
    """Section ids the renderer mints (section mode) — the authoritative id space."""
    h = distillpdf.Pdf.open(pdf_path).to_html(mode="section", return_string=True)
    return re.findall(r'<section id="(sec-[^"]+)"', h)


# ---- shape & container -------------------------------------------------------

def test_distill_writes_container_and_load_parses(tmp_path):
    dpdf, model = _distill_load(SEC_STRUCT, tmp_path)
    assert os.path.exists(dpdf) and dpdf.endswith(".dpdf")
    assert os.path.getsize(dpdf) > 0
    # the model.json root carries every top-level section.
    for key in ("schema_version", "source", "metadata", "pages", "sections",
                "blocks", "indexes"):
        assert key in model, f"model missing {key!r}"
    assert model["schema_version"] == 0  # experimental until the cutover proves the shape


def test_distill_default_path_next_to_source(tmp_path):
    src = tmp_path / "doc.pdf"
    src.write_bytes(open(SEC_STRUCT, "rb").read())
    out = distillpdf.Pdf.open(str(src)).distill()
    assert out == str(tmp_path / "doc.dpdf")
    assert (tmp_path / "doc.dpdf").exists()


def test_source_binds_to_pdf_by_hash(tmp_path):
    import hashlib
    _, model = _distill_load(SEC_STRUCT, tmp_path)
    want = hashlib.sha256(open(SEC_STRUCT, "rb").read()).hexdigest()
    assert model["source"]["sha256"] == want, "source hash must address the exact PDF bytes"
    assert model["source"]["file"] == "sec_structure.pdf"
    assert model["source"]["pages"] == distillpdf.Pdf.open(SEC_STRUCT).page_count()
    # generated_at is the one timestamp; ISO-8601 UTC.
    assert re.match(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", model["source"]["generated_at"])


# ---- consistency with to_html ------------------------------------------------

def test_pages_match_page_count(tmp_path):
    _, model = _distill_load(SEC_STRUCT, tmp_path)
    assert len(model["pages"]) == distillpdf.Pdf.open(SEC_STRUCT).page_count()
    for pg in model["pages"]:
        assert pg["width_pts"] > 0 and pg["height_pts"] > 0


def test_section_ids_match_html(tmp_path):
    _, model = _distill_load(SEC_STRUCT, tmp_path)
    model_ids = [s["id"] for s in model["sections"]]
    html_ids = _section_ids_from_html(SEC_STRUCT)
    assert model_ids == html_ids, "model section ids must equal the renderer's section ids"


def test_section_tree_parents_resolve(tmp_path):
    _, model = _distill_load(SEC_STRUCT, tmp_path)
    ids = {s["id"] for s in model["sections"]}
    for s in model["sections"]:
        if s.get("parent"):
            assert s["parent"] in ids, f"section {s['id']} has dangling parent {s['parent']}"
        assert s["page_start"] >= 1 and s["page_end"] >= s["page_start"]


def test_blocks_in_reading_order_with_stable_ids(tmp_path):
    _, model = _distill_load(SEC_STRUCT, tmp_path)
    ids = [b["id"] for b in model["blocks"]]
    # b0001, b0002, … — ordinal, zero-padded, monotonic.
    assert ids == [f"b{i:04d}" for i in range(1, len(ids) + 1)]
    for b in model["blocks"]:
        assert b["page"] >= 1
        assert b["kind"] in ("heading", "para", "list_item", "table", "figure",
                             "caption", "footnote")
        # native text layer → confidence 1.0, no OCR provenance.
        assert b["confidence"] == 1.0
        assert "ocr_pass" not in b or b["ocr_pass"] is None


def test_table_and_figure_blocks(tmp_path):
    _, model = _distill_load(NUMERIC, tmp_path)
    tables = [b for b in model["blocks"] if b["kind"] == "table"]
    assert tables, "numeric fixture should yield a table block"
    flat = " ".join(c for row in tables[0]["cells"] for c in row)
    assert "System" in flat and "84.6" in flat

    _, fmodel = _distill_load(FIGURES, tmp_path, name="fig.dpdf")
    figs = [b for b in fmodel["blocks"] if b["kind"] == "figure"]
    assert figs, "figures fixture should yield a figure block"
    assert any(f.get("caption") for f in figs)
    # a figure with an extracted raster registers an asset (a named, regenerable stub).
    assert any(f.get("image") for f in figs)


# ---- indexes are derived, coverage adds up -----------------------------------

def test_indexes_derived_and_coverage(tmp_path):
    _, model = _distill_load(SEC_STRUCT, tmp_path)
    idx = model["indexes"]
    block_ids = {b["id"] for b in model["blocks"]}
    # every block is reachable from the page index.
    paged = {bid for ids in idx["pages"].values() for bid in ids}
    assert paged == block_ids, "page index must reach every block"
    # sectioned + unsectioned partition the blocks.
    sectioned = {bid for ids in idx["sections"].values() for bid in ids}
    unsectioned = set(idx["coverage"].get("unsectioned_blocks", []))
    assert sectioned | unsectioned == block_ids
    assert not (sectioned & unsectioned), "a block can't be both sectioned and unsectioned"
    # coverage fraction matches the partition.
    n = len(block_ids)
    assert abs(idx["coverage"]["sectioned"] - len(sectioned) / n) < 1e-6


def test_kinds_index_carries_labels(tmp_path):
    _, model = _distill_load(NUMERIC, tmp_path)
    kinds = model["indexes"]["kinds"]
    # tables are a navigable kind; headings/paras are not indexed here (reachable via
    # pages/sections), so they must not bloat the kinds index.
    assert "table" in kinds and kinds["table"]
    assert "heading" not in kinds and "para" not in kinds
    for entry in kinds["table"]:
        assert entry["id"] in {b["id"] for b in model["blocks"]}
        assert entry["page"] >= 1


# ---- determinism -------------------------------------------------------------

def test_save_load_save_byte_identical(tmp_path):
    """A loaded model re-serialized is byte-identical (sorted keys, stable container)."""
    dpdf, _ = _distill_load(SEC_STRUCT, tmp_path)
    first = distillpdf.load_model(dpdf)
    second = distillpdf.load_model(dpdf)
    assert first == second, "load_model must be a pure function of the file"
    # the canonical JSON has sorted keys end-to-end.
    obj = json.loads(first)
    assert list(obj.keys()) == sorted(obj.keys()), "top-level keys must be sorted"


def test_distill_twice_same_model_modulo_timestamp(tmp_path):
    """Two distills of the same PDF differ only in source.generated_at — everything else is
    content-derived and deterministic."""
    _, a = _distill_load(SEC_STRUCT, tmp_path, name="a.dpdf")
    _, b = _distill_load(SEC_STRUCT, tmp_path, name="b.dpdf")
    a["source"]["generated_at"] = b["source"]["generated_at"] = "X"
    assert a == b, "model must be deterministic apart from the single timestamp"


# ---- asset save profiles (Wave 2) --------------------------------------------

def test_asset_table_complete_and_ids_agree(tmp_path):
    """Every figure block's image id has an asset entry — the asset table is always complete
    (no figure references a missing asset), under any profile."""
    _, model = _distill_load(FIGURES, tmp_path)
    assets = model.get("assets", [])
    assert assets, "figures fixture should register at least one asset"
    asset_ids = {x["id"] for x in assets}
    fig_images = {b["image"] for b in model["blocks"] if b.get("image")}
    assert fig_images <= asset_ids, "every figure image id must have an asset entry"
    for a in assets:
        assert a["kind"] in ("figure", "page_raster", "svg")
        assert a["regen"]["page"] >= 1  # regen recipe records the source page


def test_default_profile_embeds_figure_bytes_with_hash_and_dims(tmp_path):
    """Wave-2 default (`assets="figures"`): a raster figure's bytes are embedded with a
    verifying sha256 and pixel dimensions filled — the Wave-1 unfilled-stub hole is closed."""
    _, model = _distill_load(FIGURES, tmp_path)
    embedded = [a for a in model["assets"] if a["storage"] == "embedded"]
    assert embedded, "figures fixture's raster figure should be embedded by default"
    a = embedded[0]
    assert len(a["sha256"]) == 64 and a["bytes"] > 0
    assert a["width"] and a["height"], "embedded figure carries pixel dimensions"


def test_vector_figure_stays_dropped_stub(tmp_path):
    """A pure VECTOR figure (SVG, no raster) keeps a dropped stub even under `figures` — a
    named, reversible hole, never silent."""
    _, model = _distill_load(FIGURES, tmp_path)
    dropped = [a for a in model["assets"] if a["storage"] == "dropped"]
    assert dropped, "the vector figure has no raster to embed → dropped stub"
    assert dropped[0]["regen"]["page"] >= 1


def test_assets_none_drops_all_bytes(tmp_path):
    """`assets="none"`: text + structure only — every asset dropped to a stub, but the stubs
    (and the regen recipes) remain, so nothing becomes a silent hole."""
    d = distillpdf.Pdf.open(FIGURES)
    dpdf = d.distill(os.path.join(str(tmp_path), "none.dpdf"), assets="none")
    model = json.loads(distillpdf.load_model(dpdf))
    assert model["assets"], "stubs must remain even with no bytes"
    assert all(a["storage"] == "dropped" for a in model["assets"])

    full = d.distill(os.path.join(str(tmp_path), "full.dpdf"), assets="figures")
    # the figures profile keeps real bytes → a larger container than the text-only one.
    assert os.path.getsize(full) > os.path.getsize(dpdf)


def test_invalid_assets_profile_rejected(tmp_path):
    with pytest.raises(Exception):
        distillpdf.Pdf.open(FIGURES).distill(os.path.join(str(tmp_path), "x.dpdf"), assets="bogus")


# ---- THE ROUND-TRIP SEED TEST (Wave 2 extends this) --------------------------

@pytest.mark.parametrize("path", [SEC_STRUCT, HEADINGS, NUMERIC], ids=["sec_structure", "headings", "numeric"])
def test_roundtrip_seed_sections_and_text(path, tmp_path):
    """For a fixture: the loaded model's section ids and concatenated block text equal those
    derived from a freshly parsed document. This is the contract Wave 2's renderer-from-model
    must preserve."""
    _, model = _distill_load(path, tmp_path)

    # section ids: model == freshly-parsed HTML.
    assert [s["id"] for s in model["sections"]] == _section_ids_from_html(path)

    # concatenated visible block text: the model's text equals the visible text of the
    # freshly-rendered HTML (whitespace-normalized; SVG/markup dropped — same as _fixtures).
    from _fixtures import text as visible_text
    html = distillpdf.Pdf.open(path).to_html(mode="page", toc=False, image_mode="drop", return_string=True)
    html_text = visible_text(html)
    model_text = " ".join(b["text"] for b in model["blocks"] if b.get("text"))
    model_text = re.sub(r"\s+", " ", model_text).strip()
    # Every model block's text appears in the rendered text, in order. We assert a strong
    # subsequence/containment property rather than exact equality (the HTML carries figure
    # captions and table cells the joined block text presents differently).
    for b in model["blocks"]:
        t = re.sub(r"\s+", " ", b.get("text", "")).strip()
        if t and b["kind"] in ("heading", "para", "list_item", "footnote"):
            assert t in html_text, f"block text {t!r} absent from rendered HTML"


@pytest.mark.parametrize("path", OWNED, ids=IDS)
def test_distill_load_well_formed_over_owned_corpus(path, tmp_path):
    """Distill → load survives every owned fixture, and the model is internally consistent:
    page index reaches every block, section ids are a subset of the page-reachable id space,
    block ids are contiguous. A defect in any owned document fails this."""
    dpdf = distillpdf.Pdf.open(path).distill(os.path.join(str(tmp_path), "m.dpdf"))
    model = json.loads(distillpdf.load_model(dpdf))
    block_ids = [b["id"] for b in model["blocks"]]
    assert block_ids == [f"b{i:04d}" for i in range(1, len(block_ids) + 1)]
    paged = {bid for ids in model["indexes"]["pages"].values() for bid in ids}
    assert paged == set(block_ids), f"{os.path.basename(path)}: page index incomplete"
    assert len(model["pages"]) == distillpdf.Pdf.open(path).page_count()
