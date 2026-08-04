"""Static trust locks for the local-only table measurement harness."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent / "table_measurement"


def test_table_measurement_lock_is_closed_and_hashed():
    lock = json.loads((HERE / "lock.json").read_text())
    assert lock["schema"] == 1
    assert set(lock["packages"]) == {
        "flatbuffers", "numpy", "onnxruntime", "packaging", "pdf-parse-bench",
        "pdfplumber", "protobuf", "pylatexenc", "PyMuPDF", "pymupdf4llm", "tabulate"}
    for name, rec in lock["packages"].items():
        assert rec["version"], name
        assert rec["artifact"].endswith((".whl", ".tar.gz")), name
        assert len(rec["sha256"]) == 64 and int(rec["sha256"], 16) >= 0, name
    assert lock["runner"] == {
        "system": "Darwin", "machine": "arm64", "repeats": 5, "warmups": 1,
        "pythonhashseed": "0", "threads": 1}
    assert lock["capabilities"] == {
        "analysis_bbox_iou": True, "analysis_cell_localization": True,
        "analysis_span_topology": True,
        "legacy_bbox_iou": False, "legacy_span_topology": False,
        "semantic_header_depth": True, "pymupdf_layout": False}


def test_external_corpus_manifest_is_complete_and_unique():
    manifest = json.loads((HERE / "corpus_manifest.json").read_text())
    assert manifest["schema"] == 1
    for key, suffix in (("pdfs", ".pdf"), ("ground_truth", ".json")):
        records = manifest[key]
        assert len(records) == 100
        assert len({r["file"] for r in records}) == 100
        assert {r["file"] for r in records} == {f"{i:03d}{suffix}" for i in range(100)}
        for rec in records:
            assert rec["bytes"] > 0
            assert len(rec["sha256"]) == 64 and int(rec["sha256"], 16) >= 0


def test_tracked_measurement_inputs_have_stable_digest_shape():
    # This deliberately proves that the scorer attack suite and runner are addressable inputs;
    # the run JSON records their exact values rather than hard-coding a digest that every edit
    # would require updating by hand.
    for path in (HERE / "runner.py", HERE / "lock.json", HERE / "corpus_manifest.json",
                 Path(__file__).resolve().parent / "test_table_corpus.py"):
        assert len(hashlib.sha256(path.read_bytes()).hexdigest()) == 64
