//! distillpdf — pure-Rust PDF extraction on lopdf.
//!
//! The pyo3-free **core**: open a PDF ([`PdfDocument`]), distill it into the typed `.dpdf`
//! [`DocModel`], and render HTML / Markdown / text. The Python wheel (`distillpdf-python`)
//! is a thin PyO3 wrapper over this crate, and any Rust embedder (kglite's `knowledge_tree`)
//! consumes it directly — so **no pyo3 appears here**. The small root re-export set below is
//! the KGLite consumer contract (`dev-docs/designs/kglite-consumer-contract.md`); the wire
//! structs live under [`mod@model`].

// Public API surface consumed by the binding crate + Rust embedders.
pub mod doc;
pub mod error;
pub mod markdown;
pub mod model;
pub mod ocr;

// Crate-internal extraction / layout / render machinery. Their handful of already-`pub`
// result types are surfaced by the root re-exports below rather than by opening the whole
// module (which would drag their `pub` helper fns into the public API).
mod afm;
mod captions;
mod elem_passes;
mod extract;
mod frontmatter;
mod headings;
mod html;
mod img;
mod layout;
mod links;
mod nav;
mod postprocess;
mod profile;
mod text;
mod vector;

/// Maximum Form-XObject / content-stream recursion depth. Bounds runaway recursion and
/// cyclic Form references while allowing legitimately deep nesting.
pub(crate) const MAX_FORM_DEPTH: u32 = 40;

// ---- root re-exports: the KGLite consumer contract's small surface ----
// Document handle, options, error, the typed model, the asset profile, and the `.dpdf`
// load convenience. Wire structs (`Block`, `Section`, `Page`, …) stay under `model`.
pub use doc::{load_dpdf, DistillOptions, PdfDocument};
pub use error::Error;
pub use model::{AssetProfile, DocModel};

// ---- binding-facing extraction result types ----
// Returned by `PdfDocument` methods (`extract_images`/`extract_fonts`/`extract_tables`/
// `extract_links`/`front_matter`) and by the render `mode` conveniences. Re-exported so
// those public method signatures are legal (the types are already `pub`, just held in
// crate-private modules) without exposing the modules' internal helper fns.
pub use extract::{FontInfo, ImageInfo, TableInfo};
pub use frontmatter::{Author, FrontMatter};
pub use html::Mode;
pub use links::Link;
