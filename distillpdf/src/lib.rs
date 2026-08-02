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
mod pdfobj;
mod postprocess;
mod profile;
mod text;
mod vector;

/// Maximum Form-XObject / content-stream recursion depth. Bounds runaway recursion and
/// cyclic Form references while allowing legitimately deep nesting.
pub(crate) const MAX_FORM_DEPTH: u32 = 40;

/// Total work one page's content-stream walk may perform, across ALL nesting levels.
///
/// A depth cap alone does not bound work: a form that invokes itself *twice* branches 2x
/// per level, so `MAX_FORM_DEPTH` = 40 still permits ~2^40 (1.1e12) descents. A ~1 KB PDF
/// shaped that way hung `to_html`/`to_markdown` indefinitely (fixture
/// `tests/fixtures_pdf/adversarial/form_bomb.pdf`). The budget below bounds the walk
/// independently of depth.
///
/// The number is measured, not chosen. Instrumenting `WalkBudget` and rendering all 91
/// local documents (the 54-doc corpus + the 37 owned fixtures) gave these worst per-page
/// walk costs: 1_140_248 (`geology_usgs_fs.pdf`, 719 form descents), 1_078_050
/// (`attention_1706.03762.pdf`, 1016 descents), 500_857
/// (`geology_usgs_volcanic_hazards_california.pdf` — no forms at all, the pure-operator
/// page Phase 9 sized [`vector::MAX_OPS`] against), then 369k, 119k, 72k and below.
/// 8_000_000 is ~7x the observed worst legitimate page, so no real document comes near it,
/// while the bomb — which pays [`FORM_DESCENT_COST`] per branch — is cut off after ~15.6k
/// descents, in well under a second. Note this budget counts work at EVERY nesting level,
/// whereas `MAX_OPS` truncates only a page's top-level operator list.
pub(crate) const MAX_FORM_WORK: usize = 8_000_000;

/// Cost charged for descending into a Form XObject, on top of 1 per operator processed.
/// A descent clones the inherited resource maps, decodes the form's content stream and
/// recurses — worth far more than one operator — and a bomb pays exactly this per branch,
/// so billing it is what actually bounds the attack, rather than the two or three operators
/// inside each tiny form. The caller adds the size of the resource map it is about to
/// clone, which on form-heavy real documents is the dominant term (the corpus's worst page
/// carries ~717 XObject entries per descent).
pub(crate) const FORM_DESCENT_COST: usize = 512;

/// A shared, monotonically decrementing work budget for one page's content-stream walk.
///
/// Deliberately **not** a visited set. The three walkers are renderers: the same form is
/// legitimately drawn many times on one page (a repeated logo, a table-cell template) and
/// every occurrence must be painted, so deduplicating by `ObjectId` would silently drop
/// real content (see `tests/fixtures_pdf/adversarial/form_repeat.pdf`). Only
/// `extract::page_resource_dicts` can dedupe, because it is a *collector* answering
/// "which images exist", not a renderer.
///
/// An exhausted budget makes a walk stop and return what it has — pages DEGRADE, they do
/// not vanish (the precedent `vector::positioned_vectors_capped` set for `MAX_OPS`).
pub(crate) struct WalkBudget {
    left: usize,
}

impl WalkBudget {
    pub(crate) fn new(total: usize) -> Self {
        Self { left: total }
    }

    /// Charge `n` units of work. Returns `false` once the budget is spent, at which point
    /// the caller must stop walking (and keep whatever it has already collected).
    pub(crate) fn spend(&mut self, n: usize) -> bool {
        match self.left.checked_sub(n) {
            Some(rest) => {
                self.left = rest;
                true
            }
            None => {
                self.left = 0;
                false
            }
        }
    }
}

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
