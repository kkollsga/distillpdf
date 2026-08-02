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

use crate::pdfobj::num_deref;
use base64::Engine as _;
use lopdf::{Document, ObjectId};

/// The page's main raster (largest placed image): standard image bytes (PNG/JPEG) plus
/// the decoded image. Reuses `img::positioned_images`, which handles every PDF image
/// encoding by emitting a data URI. Used to feed the OCR engine and crop figure regions.
pub(crate) fn page_main_image(doc: &Document, page_id: ObjectId) -> Option<(Vec<u8>, image::DynamicImage)> {
    let placed = crate::img::positioned_images(doc, page_id, true);
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
    let comma = uri.find(',')?;
    base64::engine::general_purpose::STANDARD.decode(uri[comma + 1..].as_bytes()).ok()
}

/// Page size in PDF points, resolving an inherited MediaBox (default US-Letter).
pub(crate) fn page_size_pts(doc: &Document, page_id: ObjectId) -> (f32, f32) {
    fn mediabox(doc: &Document, id: ObjectId, depth: u8) -> Option<[f32; 4]> {
        if depth > 12 {
            return None;
        }
        let dict = doc.get_object(id).ok()?.as_dict().ok()?;
        if let Ok(mb) = dict.get(b"MediaBox") {
            let arr = match mb {
                lopdf::Object::Array(a) => a.clone(),
                lopdf::Object::Reference(r) => doc.get_object(*r).ok()?.as_array().ok()?.clone(),
                _ => return None,
            };
            if arr.len() == 4 {
                return Some([num_deref(doc, &arr[0]), num_deref(doc, &arr[1]), num_deref(doc, &arr[2]), num_deref(doc, &arr[3])]);
            }
        }
        let parent = dict.get(b"Parent").ok()?.as_reference().ok()?;
        mediabox(doc, parent, depth + 1)
    }
    match mediabox(doc, page_id, 0) {
        Some([x0, y0, x1, y1]) => ((x1 - x0).abs().max(1.0), (y1 - y0).abs().max(1.0)),
        None => (612.0, 792.0),
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
