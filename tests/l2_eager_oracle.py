"""Python-only L2 resolver targets, kept separate from the admitted L1 manifest."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

import distillpdf
from lazy_engine_fixtures import generate_small


FIX = Path(__file__).resolve().parent / "fixtures_pdf"


def _bytes(value: bytes | str) -> dict:
    if isinstance(value, str):
        value = value.encode()
    return {
        "kind": "bytes",
        "length": len(value),
        "sha256": hashlib.sha256(value).hexdigest(),
    }


def _error(call) -> dict:
    try:
        call()
    except Exception as error:
        return {
            "kind": "error",
            "error_type": type(error).__name__,
            "message": str(error),
        }
    raise AssertionError("expected call to fail")


def _ok(stage: str, value) -> dict:
    return {"outcome": "ok", "stage": stage, "value": value}


def _malformed_record(path: Path, temp: Path) -> dict:
    document = distillpdf.Pdf.open(str(path))
    model_path = temp / f"{path.name}.dpdf"
    document.distill(str(model_path), assets="none")
    model = json.loads(distillpdf.load_model(str(model_path)))
    model["source"]["generated_at"] = "<normalized>"
    for space in model.get("embedding_spaces", []):
        space["generated_at"] = "<normalized>"
    canonical = (json.dumps(model, indent=2, sort_keys=True) + "\n").encode()
    return {
        "source": _bytes(path.read_bytes()),
        "page_count": _ok("page_count", document.page_count()),
        "text": _ok("text_extract", _bytes(document.extract_text())),
        "page_text": _ok("page_text_extract", _bytes(document.extract_page_text(1))),
        "html": _ok("html_render", _bytes(
            document.to_html(
                return_string=True, mode="page", image_mode="drop", toc=False
            )
        )),
        "markdown": _ok("markdown_render", _bytes(
            document.to_markdown(
                return_string=True, mode="page", image_mode="drop", toc=False
            )
        )),
        "integrity": _ok("stream_integrity", document.stream_integrity()),
        "model": _ok("model_build", _bytes(canonical)),
    }


def build_manifest(temp: Path) -> dict:
    generated = temp / "small"
    generate_small(generated)
    core = distillpdf.Pdf.open(str(FIX / "sec_structure.pdf"))
    searchable = temp / "remove-raster.pdf"
    core.to_pdf(
        str(searchable),
        ocr={1: "<text>oracle searchable</text>"},
        remove_raster=True,
    )
    missing_parent = temp / "missing-parent" / "searchable.pdf"
    return {
        "format": 1,
        "layer": "python",
        "malformed_streams": {
            name: _malformed_record(generated / name, temp)
            for name in (
                "stream-missing-length.pdf",
                "stream-short-length.pdf",
            )
        },
        "searchable_pdf.remove_raster": _bytes(searchable.read_bytes()),
        "searchable_pdf.write_failure": _error(
            lambda: core.to_pdf(
                str(missing_parent),
                ocr={1: "<text>oracle searchable</text>"},
                remove_raster=True,
            )
        ),
    }
