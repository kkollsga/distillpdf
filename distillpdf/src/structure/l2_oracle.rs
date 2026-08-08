//! Resolver-migration targets that were intentionally not added to the admitted L1 oracle.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::markdown::ImgMode;
use crate::model::container;
use crate::{load_dpdf, DistillOptions, Mode, PdfDocument};

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

fn bytes_record(bytes: &[u8]) -> Value {
    json!({"kind": "bytes", "length": bytes.len(), "sha256": sha256(bytes)})
}

fn ok(stage: &str, value: Value) -> Value {
    json!({"outcome": "ok", "stage": stage, "value": value})
}

fn malformed_record(name: &str) -> Value {
    let path = fixture(&format!("adversarial/{name}"));
    let source = std::fs::read(&path).unwrap();
    let document = PdfDocument::open(path.to_str().unwrap()).unwrap();
    let html = document.render(Mode::Page, false, false);
    let (markdown, files) = crate::markdown::html_to_markdown(&html, false, ImgMode::Placeholder);
    assert!(files.is_empty());
    let issues = document
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

    let temp = std::env::temp_dir().join(format!(
        "distillpdf-l2-oracle-{}-{name}.dpdf",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temp);
    document
        .distill(temp.to_str(), &DistillOptions::text_only())
        .unwrap();
    let (mut model, _) = load_dpdf(&temp).unwrap();
    model.source.generated_at = "<normalized>".into();
    for space in &mut model.embedding_spaces {
        space.generated_at = "<normalized>".into();
    }
    let canonical_model = container::to_canonical_json(&model).unwrap();
    std::fs::remove_file(&temp).unwrap();

    json!({
        "source": bytes_record(&source),
        "page_count": ok("page_count", json!(document.page_count())),
        "text": ok("text_extract", bytes_record(document.extract_text().as_bytes())),
        "page_text": ok("page_text_extract", bytes_record(document.extract_page_text(1).unwrap().as_bytes())),
        "html": ok("html_render", bytes_record(html.as_bytes())),
        "markdown": ok("markdown_render", bytes_record(markdown.as_bytes())),
        "integrity": ok("stream_integrity", json!(issues)),
        "model": ok("model_build", bytes_record(&canonical_model)),
    })
}

fn manifest() -> Value {
    let core = PdfDocument::open(fixture("sec_structure.pdf").to_str().unwrap()).unwrap();
    let remove_raster = core
        .build_searchable_pdf(
            &HashMap::from([(1, "<text>oracle searchable</text>".to_string())]),
            true,
        )
        .unwrap();
    let malformed = BTreeMap::from([
        (
            "stream-missing-length.pdf",
            malformed_record("stream-missing-length.pdf"),
        ),
        (
            "stream-short-length.pdf",
            malformed_record("stream-short-length.pdf"),
        ),
    ]);
    json!({
        "format": 1,
        "layer": "rust",
        "searchable_pdf.remove_raster": bytes_record(&remove_raster),
        "searchable_pdf.build_failure_contract": {
            "kind": "unreachable-after-open",
            "stage": "searchable_pdf_build",
            "reason": "PdfDocument owns immutable successfully parsed bytes; the Rust build surface has no output I/O and no public source mutation hook"
        },
        "malformed_streams": malformed,
    })
}

#[test]
fn l2_eager_targets_match_frozen_oracle() {
    let actual = serde_json::to_string_pretty(&manifest()).unwrap() + "\n";
    let snapshot = workspace().join("tests/oracles/l2-eager-rust.json");
    if std::env::var_os("DISTILLPDF_UPDATE_L2_ORACLES").is_some() {
        std::fs::write(&snapshot, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(&snapshot).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; regenerate only during the L2.0 freeze",
            snapshot.display()
        )
    });
    assert_eq!(actual, expected, "Rust L2 eager target drift");
}
