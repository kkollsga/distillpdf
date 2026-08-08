"""L1 eager behavior oracle for the public Python surface.

This collector intentionally talks only to the installed Python API.  Its snapshot is
separate from the Rust oracle: neither layer is used as the other's expected value.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

import distillpdf


HERE = Path(__file__).resolve().parent
FIX = HERE / "fixtures_pdf"


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _bytes(stage: str, data: bytes | str) -> dict:
    if isinstance(data, str):
        data = data.encode()
    return {"kind": "bytes", "stage": stage, "length": len(data), "sha256": _sha256(data)}


def _value(stage: str, value) -> dict:
    return {"kind": "value", "stage": stage, "value": value}


def _error(stage: str, call) -> dict:
    try:
        call()
    except Exception as error:
        return {
            "kind": "error",
            "stage": stage,
            "error_type": type(error).__name__,
            "message": str(error),
        }
    raise AssertionError(f"expected {stage} call to fail")


def _source(name: str) -> dict:
    data = (FIX / name).read_bytes()
    return {"bytes": len(data), "sha256": _sha256(data)}


def _image(image: dict) -> dict:
    return {
        key: value
        for key, value in image.items()
        if key != "data"
    } | {
        "data_length": len(image["data"]),
        "data_sha256": _sha256(image["data"]),
    }


def _ocr_entry(entry: dict) -> dict:
    result = {key: value for key, value in entry.items() if key != "image"}
    image = entry["image"]
    result["image"] = None if image is None else {"length": len(image), "sha256": _sha256(image)}
    return result


def build_manifest(temp: Path) -> dict:
    names = [
        "sec_structure.pdf",
        "figures.pdf",
        "typography.pdf",
        "tagged_table.pdf",
        "links.pdf",
        "frontmatter.pdf",
        "decode_jpeg.pdf",
        "damaged_streams.pdf",
        "encrypted/userpw.pdf",
    ]
    sources = {name: _source(name) for name in names}
    surfaces = {}

    core_path = FIX / "sec_structure.pdf"
    core_data = core_path.read_bytes()
    core = distillpdf.Pdf.open(str(core_path))
    from_bytes = distillpdf.Pdf.from_bytes(core_data)
    surfaces["open.path_page_count"] = _value("open", {"page_count": core.page_count()})
    surfaces["open.bytes_page_count"] = _value("open", {"page_count": from_bytes.page_count()})
    surfaces["text.document"] = _bytes("extract", core.extract_text())
    surfaces["text.page_1"] = _bytes("extract", core.extract_page_text(1))
    surfaces["text.spans_page_1"] = _value("extract", core._dbg_spans_xy(1))

    figures = distillpdf.Pdf.open(str(FIX / "figures.pdf"))
    surfaces["images.all"] = _value("extract", [_image(image) for image in figures.extract_images()])
    typography = distillpdf.Pdf.open(str(FIX / "typography.pdf"))
    surfaces["fonts.all"] = _value("extract", typography.extract_fonts())
    tagged = distillpdf.Pdf.open(str(FIX / "tagged_table.pdf"))
    surfaces["tables.all"] = _value("extract", tagged.extract_tables())
    linked = distillpdf.Pdf.open(str(FIX / "links.pdf"))
    surfaces["links.all"] = _value("extract", linked.extract_links())
    surfaces["outline.pdf"] = _value("navigate", linked.outline())

    surfaces["html.section_drop_toc"] = _bytes(
        "render", core.to_html(return_string=True, mode="section", image_mode="drop", toc=True)
    )
    surfaces["html.page_embed_no_toc"] = _bytes(
        "render", core.to_html(return_string=True, mode="page", image_mode="embed", toc=False)
    )
    surfaces["markdown.page_drop_toc"] = _bytes(
        "render", core.to_markdown(return_string=True, mode="page", image_mode="drop", toc=True)
    )
    surfaces["toc.section"] = _value("navigate", core.toc(mode="section"))
    surfaces["toc.page"] = _value("navigate", core.toc(mode="page"))
    section = core.section("abstract", mode="section", image_mode="drop")
    surfaces["section.abstract"] = _value("navigate", None) if section is None else _bytes("navigate", section)
    surfaces["front_matter"] = _value(
        "extract", distillpdf.Pdf.open(str(FIX / "frontmatter.pdf")).metadata()
    )

    scan = distillpdf.Pdf.open(str(FIX / "decode_jpeg.pdf"))
    surfaces["ocr.plan"] = _value("ocr_plan", [_ocr_entry(entry) for entry in scan.ocr_plan()])
    surfaces["ocr.cache_empty"] = _value(
        "ocr_cache", {"has_ocr": scan.has_ocr(), "values": scan.get_ocr()}
    )
    surfaces["ocr.cache_set"] = _value(
        "ocr_cache",
        {
            "count": scan.set_ocr({1: "<text>oracle</text>"}),
            "has_ocr": scan.has_ocr(),
            "values": scan.get_ocr(),
        },
    )

    damaged = distillpdf.Pdf.open(str(FIX / "damaged_streams.pdf"))
    surfaces["malformed.text_suppression"] = _bytes("extract", damaged.extract_text())
    surfaces["stream.integrity"] = _value("integrity", damaged.stream_integrity())

    searchable = temp / "searchable.pdf"
    core.to_pdf(str(searchable), ocr={1: "<text>oracle searchable</text>"}, remove_raster=False)
    surfaces["searchable_pdf.keep_raster"] = _bytes("searchable_pdf", searchable.read_bytes())

    dpdf = temp / "model.dpdf"
    core.distill(str(dpdf), assets="none")
    model = json.loads(distillpdf.load_model(str(dpdf)))
    model["source"]["generated_at"] = "<normalized>"
    # The generator version is volatile by construction: `source.distillpdf` carries the
    # package version, so every release bump would otherwise re-hash this frozen surface.
    # Neutralise the token only — structure and content stay byte-locked.
    model["source"]["distillpdf"] = "<normalized>"
    for space in model.get("embedding_spaces", []):
        space["generated_at"] = "<normalized>"
    canonical = (json.dumps(model, indent=2, sort_keys=True) + "\n").encode()
    surfaces["model.canonical"] = _bytes("model_load", canonical)
    surfaces["container.member_missing"] = _value(
        "container_load", distillpdf.read_dpdf_member(str(dpdf), "missing.bin")
    )
    resaved = temp / "resaved.dpdf"
    distillpdf.save_dpdf(str(dpdf), str(resaved), canonical.decode(), {})
    surfaces["container.normalized_resave"] = _bytes("container_resave", resaved.read_bytes())
    surfaces["model.render_html"] = _bytes(
        "model_render", distillpdf.render_html(str(resaved), mode="section", toc=True)
    )
    surfaces["model.render_markdown"] = _bytes(
        "model_render",
        distillpdf.render_markdown(str(resaved), mode="section", toc=True, image_mode="drop"),
    )
    surfaces["model.render_text"] = _bytes("model_render", distillpdf.render_text(str(resaved)))

    doc = distillpdf.load(str(resaved))
    surfaces["doc.summary"] = _value(
        "doc_load",
        {
            "schema_version": doc.schema_version,
            "pages": len(doc.pages),
            "sections": len(doc.sections),
            "blocks": len(doc.blocks()),
            "toc": doc.toc(),
            "tables": doc.tables(),
            "figures": doc.figures(),
        },
    )
    found = doc.find("abstract")
    surfaces["doc.find"] = _value(
        "doc_query",
        {
            "hits": found.hits,
            "searched_blocks": found.searched_blocks,
            "searched_pages": found.searched_pages,
            "no_text_pages": found.no_text_pages,
        },
    )

    surfaces["error.parse_bytes"] = _error("open", lambda: distillpdf.Pdf.from_bytes(b"not a pdf"))
    surfaces["error.encrypted"] = _error(
        "open", lambda: distillpdf.Pdf.open(str(FIX / "encrypted/userpw.pdf"))
    )
    surfaces["error.page_negative"] = _error("page_arg", lambda: core.extract_page_text(-1))
    surfaces["error.page_missing"] = _error("extract_page", lambda: core.extract_page_text(999))
    surfaces["error.invalid_mode"] = _error(
        "option_parse", lambda: core.to_html(return_string=True, mode="bogus")
    )
    surfaces["error.invalid_image_mode"] = _error(
        "option_parse", lambda: core.to_html(return_string=True, image_mode="bogus")
    )
    surfaces["error.from_bytes_output_path"] = _error("output_resolve", lambda: from_bytes.to_html())
    invalid_dpdf = temp / "invalid.dpdf"
    invalid_dpdf.write_bytes(b"not a zip")
    surfaces["error.container_invalid"] = _error(
        "container_load", lambda: distillpdf.load_model(str(invalid_dpdf))
    )
    surfaces["error.doc_unknown_block"] = _error("doc_query", lambda: doc.block("missing"))

    return {
        "format": 1,
        "layer": "python",
        "normalizations": [
            "source.generated_at=<normalized>",
            "source.distillpdf=<normalized>",
            "embedding_spaces[*].generated_at=<normalized>",
        ],
        "sources": sources,
        "surfaces": dict(sorted(surfaces.items())),
    }
