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
pub(crate) mod doctags;
pub(crate) mod engine;
pub(crate) mod pdf;
pub(crate) mod render;
