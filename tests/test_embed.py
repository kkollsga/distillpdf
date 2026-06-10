"""Owned (no-model) tests for Wave 6: chunking, the embedding container member, search math,
and the CLI embed/search surface incl. the actionable no-dependency / no-space error paths.

The REAL-MODEL integration test lives at the bottom, skipped unless the bge-m3 weights AND the
ONNX runtime are present (mirrors the optional-dep skips in test_ocr_ux.py). Everything above it
runs in any environment — chunk derivation is a pure function of blocks, the container member is
exercised with hand-built fake vectors, and the CLI error paths are reached without any model.
"""
import importlib.util
import json
import os

import pytest

import distillpdf
from distillpdf import cli
from distillpdf import embed as E
from distillpdf.dpdf import (
    CHUNK_POLICY,
    CHUNK_TARGET_TOKENS,
    Model,
    derive_chunks,
)


# ---- helpers ----------------------------------------------------------------

def _blk(bid, page, section, text="", kind="para"):
    return {"id": bid, "kind": kind, "page": page, "section": section, "text": text,
            "confidence": 1.0}


def _build_model(blocks, sections=None, npages=None):
    """A minimal in-memory Model from blocks (indexes derived to satisfy from_dict)."""
    pages = sorted({b["page"] for b in blocks})
    npages = npages or (max(pages) if pages else 1)
    secs = sections or []
    page_idx, sec_idx, unsec = {}, {}, []
    for b in blocks:
        page_idx.setdefault(str(b["page"]), []).append(b["id"])
        if b.get("section"):
            sec_idx.setdefault(b["section"], []).append(b["id"])
        else:
            unsec.append(b["id"])
    data = {
        "schema_version": 0,
        "source": {"file": "x.pdf", "sha256": "ab", "pages": npages,
                   "distillpdf": "0", "generated_at": "t"},
        "metadata": {},
        "pages": [{"n": n, "width_pts": 612.0, "height_pts": 792.0} for n in range(1, npages + 1)],
        "sections": secs,
        "blocks": blocks,
        "indexes": {"pages": page_idx, "sections": sec_idx, "kinds": {},
                    "coverage": {"sectioned": 1.0, "unsectioned_blocks": unsec}},
    }
    return Model.from_dict(data)


# ---- chunk derivation: determinism + policy invariants ----------------------

def test_chunks_are_deterministic():
    blocks = [_blk(f"b{i:04d}", 1 + i // 3, f"sec-{i // 3}", "word " * 40) for i in range(9)]
    a = derive_chunks(blocks)
    b = derive_chunks(blocks)
    assert a == b
    assert a["policy"] == CHUNK_POLICY


def test_chunks_are_section_pure_and_contiguous_and_cover_all_blocks():
    blocks = (
        [_blk("b0001", 1, "sec-a", "alpha " * 30), _blk("b0002", 1, "sec-a", "beta " * 30)]
        + [_blk("b0003", 2, "sec-b", "gamma " * 30)]
        + [_blk("b0004", 2, None, "front " * 10)]
    )
    chunks = derive_chunks(blocks)["items"]
    seen = []
    order = {b["id"]: i for i, b in enumerate(blocks)}
    by_section = {b["id"]: b["section"] for b in blocks}
    for c in chunks:
        ids = c["block_ids"]
        seen += ids
        pos = [order[i] for i in ids]
        assert pos == list(range(pos[0], pos[0] + len(pos))), "chunk blocks must be contiguous"
        assert len({by_section[i] for i in ids}) == 1, "a chunk must not cross sections"
    assert seen == [b["id"] for b in blocks], "every block in exactly one chunk, in order"


def test_a_block_larger_than_target_is_its_own_chunk_never_split():
    big = "x" * (CHUNK_TARGET_TOKENS * 4 * 3)  # ~3× the target, in one block
    blocks = [_blk("b0001", 1, "sec-a", big), _blk("b0002", 1, "sec-a", "small")]
    chunks = derive_chunks(blocks)["items"]
    assert chunks[0]["block_ids"] == ["b0001"], "oversize block stands alone, unsplit"
    assert "b0002" in chunks[1]["block_ids"]


def test_chunk_ids_and_spans():
    blocks = [_blk("b0001", 3, "sec-a", "hi"), _blk("b0002", 5, "sec-a", "yo")]
    c = derive_chunks(blocks)["items"][0]
    assert c["id"] == "c0001"
    assert c["page_start"] == 3 and c["page_end"] == 5
    assert c["section"] == "sec-a"


def test_section_boundary_starts_a_new_chunk_even_when_small():
    blocks = [_blk("b0001", 1, "sec-a", "tiny"), _blk("b0002", 1, "sec-b", "tiny")]
    chunks = derive_chunks(blocks)["items"]
    assert len(chunks) == 2


def test_model_staleness_detects_block_change():
    blocks = [_blk("b0001", 1, "sec-a", "hello world")]
    m = _build_model(blocks)
    # No stored chunks → stale (nothing to trust yet).
    assert m.chunks_stale() is True
    # Stored chunks equal to a fresh derive → not stale.
    m._d["chunks"] = derive_chunks(blocks)
    m2 = Model.from_dict(m._d)
    assert m2.chunks_stale() is False
    # Mutating a block id makes the stored chunks drift → stale again.
    m2._d["blocks"][0]["id"] = "b0009"
    m2._block_by_id = {b["id"]: b for b in m2.blocks}
    assert m2.chunks_stale() is True


# ---- container member: encoding, byte-identity, staleness drop --------------

def _fake_space(chunk_ids, dim=4, space_id="e1"):
    space = {"id": space_id, "model": "BAAI/bge-m3", "dimension": dim, "normalized": True,
             "member": f"embeddings/{space_id}.bin", "chunk_ids": list(chunk_ids),
             "generated_at": "t", "distillpdf_version": "0"}
    vectors = [[float((i + j) % 5) for j in range(dim)] for i in range(len(chunk_ids))]
    return space, vectors


def test_binary_member_encoding_round_trips(tmp_path):
    blocks = [_blk("b0001", 1, "sec-a", "alpha"), _blk("b0002", 1, "sec-a", "beta")]
    m = _build_model(blocks)
    chunks = derive_chunks(blocks)
    cids = [c["id"] for c in chunks["items"]]
    space, vectors = _fake_space(cids)
    data = dict(m.raw)
    data["chunks"] = chunks
    data["embedding_spaces"] = [space]
    src = str(tmp_path / "src.dpdf")
    # We need a real container to re-save from; distill a tiny PDF is overkill — instead write
    # via save_dpdf from an existing container. Build one by distilling a fixture.
    _seed_container(src)
    dst = str(tmp_path / "out.dpdf")
    member = space["member"]
    bts = E.pack_vectors(vectors, 4)
    distillpdf.save_dpdf(src, dst, json.dumps(data), {member: bts})
    # The member round-trips byte-for-byte and decodes to the same vectors.
    raw = distillpdf.read_dpdf_member(dst, member)
    assert raw == bts
    assert E.unpack_vectors(raw, 4) == vectors


def test_save_load_save_byte_identical_with_embeddings(tmp_path):
    src = str(tmp_path / "src.dpdf")
    _seed_container(src)
    m = Model.load(src)
    chunks = m.derive_chunks()
    cids = [c["id"] for c in chunks["items"]]
    space, vectors = _fake_space(cids)
    data = dict(m.raw)
    data["chunks"] = chunks
    data["embedding_spaces"] = [space]
    a = str(tmp_path / "a.dpdf")
    distillpdf.save_dpdf(src, a, json.dumps(data), {space["member"]: E.pack_vectors(vectors, 4)})
    # save → load → save (carrying the member verbatim) must be byte-identical.
    b = str(tmp_path / "b.dpdf")
    distillpdf.save_dpdf(a, b, json.dumps(dict(Model.load(a).raw)), {})
    c = str(tmp_path / "c.dpdf")
    distillpdf.save_dpdf(b, c, json.dumps(dict(Model.load(b).raw)), {})
    assert open(b, "rb").read() == open(c, "rb").read()


def test_save_rejects_missized_member(tmp_path):
    src = str(tmp_path / "src.dpdf")
    _seed_container(src)
    m = Model.load(src)
    chunks = m.derive_chunks()
    cids = [c["id"] for c in chunks["items"]]
    space, _ = _fake_space(cids, dim=4)
    data = dict(m.raw)
    data["chunks"] = chunks
    data["embedding_spaces"] = [space]
    dst = str(tmp_path / "out.dpdf")
    # Member with the wrong number of floats → loud save error (honest coverage).
    bad = E.pack_vectors([[0.0, 0.0, 0.0]] * len(cids), 3)  # dim 3, space declares 4
    with pytest.raises(Exception) as ei:
        distillpdf.save_dpdf(src, dst, json.dumps(data), {space["member"]: bad})
    assert "expected" in str(ei.value).lower()


def _seed_container(path):
    """A small real .dpdf to re-save from (save_dpdf reads the source container)."""
    fix = os.path.join(os.path.dirname(__file__), "fixtures_pdf")
    pdf = next(f for f in sorted(os.listdir(fix)) if f.endswith(".pdf"))
    distillpdf.Pdf.open(os.path.join(fix, pdf)).distill(path)


# ---- search math (cosine ranking; pure-python path too) ---------------------

def test_pack_unpack_roundtrip_and_loud_truncation():
    v = [[0.1, 0.2, 0.3], [0.4, 0.5, 0.6]]
    data = E.pack_vectors(v, 3)
    assert len(data) == 2 * 3 * 4
    out = E.unpack_vectors(data, 3)
    assert all(abs(a - b) < 1e-6 for r1, r2 in zip(v, out) for a, b in zip(r1, r2))
    with pytest.raises(ValueError):
        E.unpack_vectors(data + b"\x00", 3)  # not a whole row → loud


def test_cosine_topk_ranks_by_similarity():
    import math

    def unit(x):
        n = math.sqrt(sum(i * i for i in x))
        return [i / n for i in x]
    matrix = [unit([1, 0, 0]), unit([0, 1, 0]), unit([1, 1, 0])]
    ranked = E.cosine_topk([1, 0.05, 0], matrix, 3)
    order = [i for i, _ in ranked]
    assert order[0] == 0  # nearest to the x-axis query
    assert order == sorted(order, key=lambda i: -ranked[[r[0] for r in ranked].index(i)][1])


def test_cosine_pure_python_matches_numpy(monkeypatch):
    import math

    def unit(x):
        n = math.sqrt(sum(i * i for i in x)) or 1.0
        return [i / n for i in x]
    matrix = [unit([1, 0, 0, 0]), unit([0, 1, 0, 0]), unit([0.7, 0.7, 0, 0]), unit([0, 0, 1, 0])]
    q = [0.9, 0.3, 0, 0]
    np_ranked = E.cosine_topk(q, matrix, 4)
    # Force the pure-python branch by hiding numpy from the helper's find_spec check.
    real = importlib.util.find_spec

    def fake(name, *a, **k):
        return None if name == "numpy" else real(name, *a, **k)
    monkeypatch.setattr(E.importlib.util, "find_spec", fake)
    py_ranked = E.cosine_topk(q, matrix, 4)
    assert [i for i, _ in np_ranked] == [i for i, _ in py_ranked]
    for (_, s1), (_, s2) in zip(np_ranked, py_ranked):
        assert abs(s1 - s2) < 1e-5


# ---- CLI error paths WITHOUT the embedding runtime --------------------------

def _no_runtime(monkeypatch):
    monkeypatch.setattr(E, "runtime_available", lambda: False)
    real = importlib.util.find_spec

    def fake(name, *a, **k):
        return None if name in E._REQUIRED else real(name, *a, **k)
    monkeypatch.setattr(E.importlib.util, "find_spec", fake)


def test_cli_embed_without_runtime_is_actionable(tmp_path, monkeypatch, capsys):
    src = str(tmp_path / "x.dpdf")
    _seed_container(src)
    _no_runtime(monkeypatch)
    rc = cli.main([src, "embed"])
    err = capsys.readouterr().err
    assert rc == 1
    assert "pip install onnxruntime tokenizers huggingface_hub" in err


def test_cli_search_without_space_is_actionable(tmp_path, capsys):
    src = str(tmp_path / "x.dpdf")
    _seed_container(src)
    rc = cli.main([src, "search", "anything"])
    err = capsys.readouterr().err
    assert rc == 1
    assert "run `distillpdf" in err and "embed" in err


def test_cli_search_without_space_and_without_runtime_includes_install_help(
    tmp_path, monkeypatch, capsys
):
    src = str(tmp_path / "x.dpdf")
    _seed_container(src)
    _no_runtime(monkeypatch)
    rc = cli.main([src, "search", "anything"])
    err = capsys.readouterr().err
    assert rc == 1
    assert "pip install onnxruntime" in err


def test_info_embeddings_line_when_none(tmp_path, capsys):
    src = str(tmp_path / "x.dpdf")
    _seed_container(src)
    cli.main([src, "info"])
    out = capsys.readouterr().out
    assert "embeddings: none" in out


# ---- --json validity (search over a hand-built space, no model needed) ------

def test_search_json_shape_with_fake_space(tmp_path, monkeypatch, capsys):
    src = str(tmp_path / "x.dpdf")
    _seed_container(src)
    m = Model.load(src)
    chunks = m.derive_chunks()
    cids = [c["id"] for c in chunks["items"]]
    space, vectors = _fake_space(cids, dim=E.DIMENSION)
    data = dict(m.raw)
    data["chunks"] = chunks
    data["embedding_spaces"] = [space]
    dst = str(tmp_path / "emb.dpdf")
    # vectors must be DIMENSION-wide to match the declared space.
    vectors = [[float((i + j) % 7) for j in range(E.DIMENSION)] for i in range(len(cids))]
    distillpdf.save_dpdf(src, dst, json.dumps(data),
                         {space["member"]: E.pack_vectors(vectors, E.DIMENSION)})

    # Stub the embedder so search runs WITHOUT the real model: a fixed query vector.
    class _Stub:
        def embed(self, texts):
            return [[1.0] + [0.0] * (E.DIMENSION - 1) for _ in texts]

        def release(self):
            pass
    monkeypatch.setattr(E, "make_embedder", lambda *a, **k: _Stub())

    rc = cli.main([dst, "search", "q", "--json", "--k", "2"])
    assert rc == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["model"] == "BAAI/bge-m3"
    assert payload["searched_chunks"] == len(cids)
    assert payload["stale"] is False
    assert len(payload["hits"]) <= 2
    for h in payload["hits"]:
        assert set(h) >= {"chunk_id", "score", "section", "block_ids", "snippet"}


# ---- REAL-MODEL integration (skipped unless weights + runtime present) ------

def _weights_present():
    """True when the bge-m3 ONNX weights resolve offline from HF_HOME/FASTEMBED_CACHE_PATH and
    the ONNX runtime is importable. Mirrors the optional-dep skip pattern in test_ocr_ux.py."""
    if not E.runtime_available():
        return False
    from pathlib import Path
    bases = []
    if os.environ.get("FASTEMBED_CACHE_PATH"):
        bases.append(Path(os.environ["FASTEMBED_CACHE_PATH"]))
    if os.environ.get("HF_HOME"):
        bases.append(Path(os.environ["HF_HOME"]) / "hub")
    bases.append(Path.home() / ".cache" / "fastembed")
    for base in bases:
        if any((base / "models--BAAI--bge-m3" / "snapshots").glob("*/onnx/model.onnx_data")):
            return True
    return False


@pytest.mark.skipif(not _weights_present(),
                    reason="bge-m3 weights / ONNX runtime not present (set HF_HOME or "
                           "FASTEMBED_CACHE_PATH at the weights and install onnxruntime+tokenizers)")
def test_real_model_semantic_ranking(tmp_path):
    """Embed three semantically distinct sections and assert the obvious ranking: a financial
    query ranks the financial chunk above the unrelated ones."""
    from reportlab.lib.pagesizes import letter
    from reportlab.lib.styles import getSampleStyleSheet
    from reportlab.platypus import Paragraph, SimpleDocTemplate, Spacer, PageBreak

    s = getSampleStyleSheet()
    story = []
    for title, body in [
        ("Financial Results", "Quarterly revenue rose to 4.2 billion dollars, up 18 percent, "
                              "with operating margin expanding and net income reaching 920 million."),
        ("Wildlife Migration", "Arctic terns undertake the longest annual migration, from the "
                               "Arctic breeding grounds to the Antarctic and back each year."),
        ("Cooking Techniques", "Searing meat at high temperature builds a flavorful crust via the "
                               "Maillard reaction before a slow finish in the oven."),
    ]:
        story.append(Paragraph(title, s["Heading1"]))
        story.append(Paragraph(body, s["BodyText"]))
        story.append(Spacer(1, 8))
        story.append(PageBreak())
    pdf = str(tmp_path / "sem.pdf")
    SimpleDocTemplate(pdf, pagesize=letter).build(story)
    dpdf = str(tmp_path / "sem.dpdf")
    distillpdf.Pdf.open(pdf).distill(dpdf)

    doc = distillpdf.load(dpdf)
    doc.embed("e1", progress=False)
    doc = distillpdf.load(dpdf)  # reload from disk (proves persistence)

    res = doc.search("company earnings and profit margins", k=3)
    assert not res.stale
    assert res.model == "BAAI/bge-m3"
    assert res.hits[0]["section"] == "sec-financial-results"
    # The financial chunk must beat both unrelated ones by a clear margin.
    top = res.hits[0]["score"]
    rest = [h["score"] for h in res.hits[1:]]
    assert all(top > r + 0.1 for r in rest), (top, rest)
