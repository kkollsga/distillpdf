//! OCR feature: recover content from image-only / poorly-extracted PDF pages.
//!
//! Pipeline (all pure Rust except the model inference, which is pluggable):
//!   1. `detect`  — flag pages that need OCR (image present, little/garbled text).
//!   2. `engine`  — an `OcrEngine` trait turns a page image into DocTags; the default
//!                  backend talks to a llama.cpp/granite server over HTTP.
//!   3. `doctags` — parse the model's DocTags into a typed `OcrPage` block model
//!                  (a clean-room port of docling-core's DocTags grammar).
//!   4. `render`  — turn `OcrPage` blocks into distillPDF's HTML, reusing the same
//!                  layout/heading/table rules as the born-digital path.
//!
//! The block model is also the source for the hybrid searchable-PDF writer (`to_pdf`).

#![allow(dead_code)] // wired into html.rs / lib.rs incrementally

pub(crate) mod detect;
pub mod doctags;
pub mod engine;
pub mod pdf;
pub mod render;
pub(crate) mod tess_synth;
#[cfg(feature = "tesseract")]
pub mod tesseract;

use lopdf::{Document, ObjectId};

/// The page's main raster (largest placed image): standard image bytes (PNG/JPEG) plus
/// the decoded image. Reuses `img::positioned_images`, which handles every PDF image
/// encoding by emitting a data URI. Used to feed the OCR engine and crop figure regions.
pub(crate) fn page_main_image(
    doc: &Document,
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
) -> Option<(Vec<u8>, image::DynamicImage)> {
    let placed = crate::img::positioned_images(doc, access, page_id, true);
    let best = placed
        .into_iter()
        .filter(|p| !p.uri.is_empty())
        .max_by(|a, b| {
            let area = |p: &crate::img::Placed| (p.x_right - p.x_left).abs() * (p.y_top - p.y_bottom).abs();
            area(a).partial_cmp(&area(b)).unwrap_or(std::cmp::Ordering::Equal)
        })?;
    let bytes = data_uri_bytes(&best.uri)?;
    let img = image::load_from_memory(&bytes).ok()?;
    Some((bytes, img))
}

/// Decode the base64 payload of a `data:...;base64,XXXX` URI.
fn data_uri_bytes(uri: &str) -> Option<Vec<u8>> {
    crate::textutil::decode_data_uri(uri).map(|(bytes, _)| bytes)
}

/// Page size in PDF points, from the one page-box walker ([`crate::pdfobj::page_box`]):
/// `/MediaBox` then `/CropBox`, inherited up `/Parent`, indirect extents resolved. Defaults
/// to [`crate::pdfobj::DEFAULT_PAGE_PTS`] only when the document states no box at all.
pub(crate) fn page_size_pts(access: &dyn crate::access::DocumentAccess, page_id: ObjectId) -> (f32, f32) {
    match crate::pdfobj::page_box(access, page_id) {
        Some([x0, y0, x1, y1]) => ((x1 - x0).abs().max(1.0), (y1 - y0).abs().max(1.0)),
        None => crate::pdfobj::DEFAULT_PAGE_PTS,
    }
}

/// Detect the dominant language of a text sample and map it to a bundled Tesseract code
/// (`eng`/`por`/`nor`). Returns `None` when detection is low-confidence or the language isn't
/// one we bundle — the caller then keeps the full bundled set. Pure-Rust (whatlang), so it is
/// only compiled with the `tesseract` feature (which owns the `whatlang` dependency). The
/// PyO3 wheel's `detect_language` pyfunction is a thin forward to this.
#[cfg(feature = "tesseract")]
pub fn detect_language(text: &str) -> Option<String> {
    let info = whatlang::detect(text)?;
    if !info.is_reliable() || info.confidence() < 0.55 {
        return None;
    }
    let code = match info.lang() {
        whatlang::Lang::Eng => "eng",
        whatlang::Lang::Por => "por",
        whatlang::Lang::Nob => "nor", // Norwegian Bokmål → the bundled `nor` model
        _ => return None,
    };
    Some(code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::test_adapter;

    fn page_size_pts(doc: &Document, page_id: ObjectId) -> (f32, f32) {
        super::page_size_pts(&test_adapter(doc), page_id)
    }

    /// The owned page-box fixture (`tests/gen_fixtures.py::gen_indirect_mediabox`).
    fn box_fixture() -> Document {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/indirect_mediabox.pdf");
        Document::load(path).expect("indirect_mediabox.pdf fixture must load")
    }

    #[test]
    fn page_size_resolves_an_inherited_media_box_with_indirect_entries() {
        let doc = box_fixture();
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        assert_eq!(page_size_pts(&doc, page_id), (1008.0, 612.0));
    }

    #[test]
    fn page_size_falls_back_to_the_crop_box() {
        // A page stating only a /CropBox (no /MediaBox anywhere up the tree) reported
        // US-Letter, so every OCR bbox mapped onto it was scaled from the wrong page.
        let doc = box_fixture();
        let page_id = *doc.get_pages().get(&2).expect("fixture has page 2");
        assert_eq!(page_size_pts(&doc, page_id), (400.0, 650.0), "/CropBox is the spec fallback");
    }

    #[test]
    fn page_size_defaults_to_letter_when_no_box_exists_anywhere() {
        // Degrade, never panic or return zero: a dangling page id has no box at all.
        let doc = box_fixture();
        assert_eq!(page_size_pts(&doc, (9_999, 0)), crate::pdfobj::DEFAULT_PAGE_PTS);
    }
}
