//! L1 eager behavior oracle for the pure-Rust public surface.

#[cfg(test)]
mod tests {

    use std::collections::{BTreeMap, HashMap};
    use std::path::{Path, PathBuf};

    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};

    use crate::markdown::ImgMode;
    use crate::model::container::{self, AssetBytes};
    use crate::model::render as model_render;
    use crate::{load_dpdf, DistillOptions, Error, Mode, PdfDocument};

    fn workspace() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn fixture(name: &str) -> PathBuf {
        workspace().join("tests/fixtures_pdf").join(name)
    }

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn bytes_record(stage: &str, bytes: &[u8]) -> Value {
        json!({"kind": "bytes", "stage": stage, "length": bytes.len(), "sha256": sha256(bytes)})
    }

    fn value_record(stage: &str, value: Value) -> Value {
        json!({"kind": "value", "stage": stage, "value": value})
    }

    fn error_name(error: &Error) -> &'static str {
        match error {
            Error::Read(_) => "Read",
            Error::Write(_) => "Write",
            Error::Mkdir(_) => "Mkdir",
            Error::Open(_) => "Open",
            Error::Parse(_) => "Parse",
            Error::Encrypted => "Encrypted",
            Error::InvalidMode(_) => "InvalidMode",
            Error::InvalidImageMode(_) => "InvalidImageMode",
            Error::InvalidEngine(_) => "InvalidEngine",
            Error::NoSourceDir => "NoSourceDir",
            Error::NoSourcePath => "NoSourcePath",
            Error::OcrPoisoned => "OcrPoisoned",
            Error::NoPage(_) => "NoPage",
            Error::Model(_) => "Model",
            Error::Ocr(_) => "Ocr",
            Error::ModelNotUtf8(_) => "ModelNotUtf8",
            Error::ParseModelJson(_) => "ParseModelJson",
        }
    }

    fn error_record(stage: &str, error: Error) -> Value {
        json!({
            "kind": "error",
            "stage": stage,
            "error_type": error_name(&error),
            "message": error.to_string(),
        })
    }

    fn source_record(path: &Path) -> Value {
        let bytes = std::fs::read(path).unwrap();
        json!({"bytes": bytes.len(), "sha256": sha256(&bytes)})
    }

    fn temp_dir() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("distillpdf-l1-rust-oracle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn manifest() -> Value {
        let mut sources = BTreeMap::new();
        for name in [
            "sec_structure.pdf",
            "figures.pdf",
            "typography.pdf",
            "tagged_table.pdf",
            "links.pdf",
            "frontmatter.pdf",
            "decode_jpeg.pdf",
            "damaged_streams.pdf",
            "encrypted/userpw.pdf",
        ] {
            sources.insert(name, source_record(&fixture(name)));
        }

        let mut surfaces = BTreeMap::new();
        let core_path = fixture("sec_structure.pdf");
        let core_bytes = std::fs::read(&core_path).unwrap();
        let core = PdfDocument::open(core_path.to_str().unwrap()).unwrap();
        let from_bytes = PdfDocument::from_bytes(&core_bytes).unwrap();
        surfaces.insert(
            "open.path_page_count",
            value_record("open", json!({"page_count": core.page_count()})),
        );
        surfaces.insert(
            "open.bytes_page_count",
            value_record("open", json!({"page_count": from_bytes.page_count()})),
        );

        surfaces.insert(
            "text.document",
            bytes_record("extract", core.extract_text().as_bytes()),
        );
        surfaces.insert(
            "text.page_1",
            bytes_record("extract", core.extract_page_text(1).unwrap().as_bytes()),
        );
        let spans = core
            .dbg_spans_xy(1)
            .unwrap()
            .into_iter()
            .map(|(text, x, y, width, size)| json!([text, x, y, width, size]))
            .collect::<Vec<_>>();
        surfaces.insert("text.spans_page_1", value_record("extract", json!(spans)));

        let figures = PdfDocument::open(fixture("figures.pdf").to_str().unwrap()).unwrap();
        let images = figures
            .extract_images()
            .into_iter()
            .map(|image| {
                json!({
                    "page": image.page,
                    "index": image.index,
                    "width": image.width,
                    "height": image.height,
                    "color_space": image.color_space,
                    "bits_per_component": image.bits_per_component,
                    "format": image.format,
                    "data_length": image.data.len(),
                    "data_sha256": sha256(&image.data),
                })
            })
            .collect::<Vec<_>>();
        surfaces.insert("images.all", value_record("extract", json!(images)));

        let typography = PdfDocument::open(fixture("typography.pdf").to_str().unwrap()).unwrap();
        let fonts = typography
            .extract_fonts()
            .into_iter()
            .map(|font| {
                json!({
                    "page": font.page,
                    "name": font.name,
                    "subtype": font.subtype,
                    "base_font": font.base_font,
                    "encoding": font.encoding,
                    "embedded": font.embedded,
                    "has_tounicode": font.has_tounicode,
                })
            })
            .collect::<Vec<_>>();
        surfaces.insert("fonts.all", value_record("extract", json!(fonts)));

        let tagged = PdfDocument::open(fixture("tagged_table.pdf").to_str().unwrap()).unwrap();
        let tables = tagged
            .extract_tables()
            .into_iter()
            .map(|table| json!({"page": table.page, "cells": table.cells}))
            .collect::<Vec<_>>();
        surfaces.insert("tables.all", value_record("extract", json!(tables)));

        let linked = PdfDocument::open(fixture("links.pdf").to_str().unwrap()).unwrap();
        let links = linked
            .extract_links()
            .into_iter()
            .map(|link| {
                json!({
                    "page": link.page,
                    "rect": link.rect,
                    "kind": link.kind(),
                    "uri": link.uri,
                    "dest_page": link.dest_page,
                    "dest_name": link.dest_name,
                    "remote_file": link.remote_file,
                })
            })
            .collect::<Vec<_>>();
        surfaces.insert("links.all", value_record("extract", json!(links)));
        surfaces.insert(
            "outline.pdf",
            value_record("navigate", json!(linked.outline())),
        );

        for (name, mode, images, toc) in [
            ("html.section_drop_toc", Mode::Section, false, true),
            ("html.page_embed_no_toc", Mode::Page, true, false),
        ] {
            surfaces.insert(
                name,
                bytes_record("render", core.render(mode, images, toc).as_bytes()),
            );
        }
        let page_html = core.render(Mode::Page, false, true);
        let (markdown, files) =
            crate::markdown::html_to_markdown(&page_html, true, ImgMode::Placeholder);
        surfaces.insert(
            "markdown.page_drop_toc",
            bytes_record("render", markdown.as_bytes()),
        );
        surfaces.insert(
            "markdown.page_drop_files",
            value_record("render", json!({"count": files.len()})),
        );
        surfaces.insert(
            "toc.section",
            value_record("navigate", json!(core.toc(Mode::Section))),
        );
        surfaces.insert(
            "toc.page",
            value_record("navigate", json!(core.toc(Mode::Page))),
        );
        surfaces.insert(
            "section.abstract",
            match core.section(Mode::Section, "abstract", false) {
                Some(section) => bytes_record("navigate", section.as_bytes()),
                None => value_record("navigate", Value::Null),
            },
        );

        let front = PdfDocument::open(fixture("frontmatter.pdf").to_str().unwrap())
            .unwrap()
            .front_matter();
        surfaces.insert(
            "front_matter",
            value_record(
                "extract",
                json!({
                    "title": front.title,
                    "authors": front.authors.into_iter().map(|author| json!({
                        "name": author.name,
                        "markers": author.markers,
                        "affiliation": author.affiliation,
                    })).collect::<Vec<_>>(),
                    "affiliations": front.affiliations,
                    "abstract": front.abstract_text,
                    "keywords": front.keywords,
                }),
            ),
        );

        let scan = PdfDocument::open(fixture("decode_jpeg.pdf").to_str().unwrap()).unwrap();
        let plan = scan
            .ocr_plan()
            .into_iter()
            .map(|entry| {
                json!({
                    "page": entry.page,
                    "needs_ocr": entry.needs_ocr,
                    "reason": entry.reason,
                    "width_pts": entry.width_pts,
                    "height_pts": entry.height_pts,
                    "image": entry.image.as_ref().map(|bytes| json!({
                        "length": bytes.len(), "sha256": sha256(bytes)
                    })),
                })
            })
            .collect::<Vec<_>>();
        surfaces.insert("ocr.plan", value_record("ocr_plan", json!(plan)));
        surfaces.insert(
            "ocr.cache_empty",
            value_record(
                "ocr_cache",
                json!({
                    "has_ocr": scan.has_ocr().unwrap(), "values": scan.get_ocr().unwrap()
                }),
            ),
        );
        surfaces.insert(
        "ocr.cache_set",
        value_record(
            "ocr_cache",
            json!({
                "count": scan.set_ocr(HashMap::from([(1, "<text>oracle</text>".to_string())])).unwrap(),
                "has_ocr": scan.has_ocr().unwrap(),
                "values": scan.get_ocr().unwrap(),
            }),
        ),
    );

        let damaged = PdfDocument::open(fixture("damaged_streams.pdf").to_str().unwrap()).unwrap();
        surfaces.insert(
            "malformed.text_suppression",
            bytes_record("extract", damaged.extract_text().as_bytes()),
        );
        let issues = damaged
            .stream_integrity()
            .into_iter()
            .map(|issue| {
                json!({
                    "object": issue.object,
                    "kind": issue.kind,
                    "filter": issue.filter,
                    "recovered": issue.recovered,
                })
            })
            .collect::<Vec<_>>();
        surfaces.insert("stream.integrity", value_record("integrity", json!(issues)));

        let searchable = core
            .build_searchable_pdf(
                &HashMap::from([(1, "<text>oracle searchable</text>".to_string())]),
                false,
            )
            .unwrap();
        surfaces.insert(
            "searchable_pdf.keep_raster",
            bytes_record("searchable_pdf", &searchable),
        );

        let temp = temp_dir();
        let dpdf = temp.join("model.dpdf");
        core.distill(dpdf.to_str(), &DistillOptions::text_only())
            .unwrap();
        let (mut model, members) = load_dpdf(&dpdf).unwrap();
        model.source.generated_at = "<normalized>".into();
        for space in &mut model.embedding_spaces {
            space.generated_at = "<normalized>".into();
        }
        let canonical = container::to_canonical_json(&model).unwrap();
        surfaces.insert("model.canonical", bytes_record("model_load", &canonical));
        surfaces.insert(
            "container.members",
            value_record(
                "container_load",
                json!(members
                    .iter()
                    .map(|(name, bytes)| json!({
                        "name": name, "length": bytes.len(), "sha256": sha256(bytes)
                    }))
                    .collect::<Vec<_>>()),
            ),
        );
        let resaved = temp.join("resaved.dpdf");
        container::save_with_members(&model, &resaved, &AssetBytes::new(), &members, None).unwrap();
        surfaces.insert(
            "container.normalized_resave",
            bytes_record("container_resave", &std::fs::read(&resaved).unwrap()),
        );
        surfaces.insert(
            "model.render_html",
            bytes_record(
                "model_render",
                model_render::render_html(&model, Mode::Section, true).as_bytes(),
            ),
        );
        surfaces.insert(
            "model.render_markdown",
            bytes_record(
                "model_render",
                model_render::render_markdown(&model, Mode::Section, true, "drop")
                    .unwrap()
                    .0
                    .as_bytes(),
            ),
        );
        surfaces.insert(
            "model.render_text",
            bytes_record(
                "model_render",
                model_render::extract_text(&model).as_bytes(),
            ),
        );

        surfaces.insert(
            "error.parse_bytes",
            error_record("open", PdfDocument::from_bytes(b"not a pdf").err().unwrap()),
        );
        surfaces.insert(
            "error.encrypted",
            error_record(
                "open",
                PdfDocument::open(fixture("encrypted/userpw.pdf").to_str().unwrap())
                    .err()
                    .unwrap(),
            ),
        );
        surfaces.insert(
            "error.page_missing",
            error_record("extract_page", core.extract_page_text(999).unwrap_err()),
        );
        surfaces.insert(
            "error.invalid_mode",
            error_record(
                "option_parse",
                crate::doc::parse_mode("bogus").err().unwrap(),
            ),
        );
        surfaces.insert(
            "error.invalid_image_mode",
            error_record(
                "option_parse",
                crate::doc::parse_image_mode("bogus", false, ImgMode::Placeholder)
                    .err()
                    .unwrap(),
            ),
        );
        surfaces.insert(
            "error.from_bytes_output_path",
            error_record(
                "output_resolve",
                from_bytes.resolve_out_path(None, "html").unwrap_err(),
            ),
        );
        let invalid_dpdf = temp.join("invalid.dpdf");
        std::fs::write(&invalid_dpdf, b"not a zip").unwrap();
        surfaces.insert(
            "error.container_invalid",
            error_record("container_load", load_dpdf(&invalid_dpdf).unwrap_err()),
        );
        std::fs::remove_dir_all(temp).unwrap();

        json!({
            "format": 1,
            "layer": "rust",
            "normalizations": [
                "source.generated_at=<normalized>",
                "embedding_spaces[*].generated_at=<normalized>"
            ],
            "sources": sources,
            "surfaces": surfaces,
        })
    }

    #[test]
    fn eager_public_surface_matches_frozen_oracle() {
        let actual = serde_json::to_string_pretty(&manifest()).unwrap() + "\n";
        let snapshot = workspace().join("tests/oracles/eager-rust.json");
        if std::env::var_os("DISTILLPDF_UPDATE_EAGER_ORACLES").is_some() {
            std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
            std::fs::write(&snapshot, &actual).unwrap();
        }
        let expected = std::fs::read_to_string(&snapshot).unwrap_or_else(|error| {
            panic!(
                "read {}: {error}; regenerate with DISTILLPDF_UPDATE_EAGER_ORACLES=1",
                snapshot.display()
            )
        });
        assert_eq!(
            actual, expected,
            "Rust eager oracle drift; inspect behavior before updating the snapshot"
        );
    }
}
