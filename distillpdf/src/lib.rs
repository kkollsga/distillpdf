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
mod geom;
mod headings;
mod html;
mod img;
mod layout;
mod links;
mod nav;
mod pdfobj;
mod postprocess;
mod profile;
mod raster;
mod text;
mod textutil;
mod vector;
mod walker;

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
/// The page size to assume when a caller has no page geometry at all (US Letter, in points).
/// Exported so the binding crate's OCR entry point states the same default as the core does
/// instead of open-coding `612.0`/`792.0` a fifth time.
pub use pdfobj::DEFAULT_PAGE_PTS;

/// **The fortified standard, made greppable.**
///
/// Ten phases of centralization are only worth the churn if the next hand-rolled copy is
/// caught by a test instead of by a bug report two releases later. This module reads the
/// crate's own source and fails on the patterns that *were* the defects — not on style.
/// Every rule below names the bug it prevents; a rule that cannot name one does not belong
/// here.
///
/// It is a `--lib` unit test on purpose: it runs in the same `cargo test` every phase gate
/// already runs, with no extra script to forget.
#[cfg(test)]
mod structure {
    use std::path::{Path, PathBuf};

    fn core_src() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    /// Every `.rs` file under `dir`, as `(display path, source)`, recursively.
    fn rust_files(dir: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).expect("source tree is readable") {
                let p = entry.expect("dir entry").path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    let rel = p.strip_prefix(dir).unwrap_or(&p).display().to_string();
                    let src = std::fs::read_to_string(&p).expect("readable source");
                    out.push((rel, production_code(&src)));
                }
            }
        }
        out.sort();
        assert!(out.len() > 10, "the walk found no source — the test would pass vacuously");
        out
    }

    /// `src` up to its `#[cfg(test)]` module. These rules govern production code: a test may
    /// name a banned spelling in order to *exercise* it (`pdfobj`'s tests call the real
    /// `deref`; this module's own rules are written out as string literals). Every file in
    /// this crate puts its test module last and has at most one, which is asserted rather
    /// than assumed — a second one would mean this truncation silently dropped real code.
    fn production_code(src: &str) -> String {
        // A top-level test module's attribute is the whole line, at column 0 — which is what
        // distinguishes it from this module's own prose and string literals naming it.
        let marks: Vec<usize> = src.lines().enumerate().filter(|(_, l)| *l == TEST_ATTR).map(|(i, _)| i).collect();
        assert!(marks.len() <= 1, "a file with two top-level test modules would truncate wrongly");
        match marks.first() {
            Some(&i) => src.lines().take(i).collect::<Vec<_>>().join("\n"),
            None => src.to_string(),
        }
    }

    /// Spelled as a constant so this module's own source does not contain the literal line.
    const TEST_ATTR: &str = concat!("#[cfg", "(test)]");

    /// `src` with `//`-comments and doc comments dropped, so a rule that bans a *spelling*
    /// is not tripped by a comment that explains why the spelling is banned. String literals
    /// are left alone: nothing here bans a pattern that legitimately appears in one.
    fn code_only(src: &str) -> String {
        src.lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Lines of `code_only(src)` containing `needle`, numbered from 1.
    fn hits(src: &str, needle: &str) -> Vec<String> {
        code_only(src)
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(needle))
            .map(|(i, l)| format!("{}: {}", i + 1, l.trim()))
            .collect()
    }

    #[test]
    fn the_primitives_have_exactly_one_definition_each() {
        // `deref` had five byte-identical copies, `num` four plus a divergent fifth that was
        // the only one following an indirect reference, `xobjects_of` three. Each now lives
        // in one module; a redefinition anywhere else is the drift starting over.
        for (name, owner) in [("fn deref", "pdfobj.rs"), ("fn num(", "pdfobj.rs"), ("fn num_deref", "pdfobj.rs"), ("fn xobjects_of", "walker.rs")] {
            for (file, src) in rust_files(&core_src()) {
                // The owner's own definition and any module's `use`/test may name it.
                if file == owner {
                    continue;
                }
                let found = hits(&src, name);
                assert!(found.is_empty(), "{name} is owned by {owner}; redefined in {file}:\n  {}", found.join("\n  "));
            }
        }
    }

    #[test]
    fn no_stream_is_read_around_content_bytes() {
        // THE defect, in both its spellings. lopdf's `decompressed_content()` returns an
        // ERROR for a stream with no `/Filter`, so `.unwrap_or_default()` decoded every
        // unfiltered content stream as EMPTY — two of four walkers silently lost the text
        // and vector ink inside such a form. `.unwrap_or_else(|_| s.content.clone())` is the
        // correct fallback, but it is `pdfobj::content_bytes`'s body hand-copied at four
        // sites, which is how a weak copy gets written next to a strong one in the first
        // place. Both are banned; read the stream through the helper.
        //
        // Deliberately NOT banned: binding the `Result` and inspecting it. `text::debug_page`
        // reports the raw and decoded lengths side by side, and `raster::codec_payload`
        // decompresses a synthetic stream it just built. Neither turns the result into bytes.
        for (file, src) in rust_files(&core_src()) {
            if file == "pdfobj.rs" {
                continue; // the owner
            }
            for spelling in ["decompressed_content().unwrap_or_default()", "decompressed_content().unwrap_or_else"] {
                let found = hits(&src, spelling);
                assert!(
                    found.is_empty(),
                    "{file} reads a stream around pdfobj::content_bytes ({spelling}):\n  {}",
                    found.join("\n  ")
                );
            }
        }
    }

    #[test]
    fn every_dos_cap_is_declared_once() {
        // `MAX_FORM_DEPTH` was compared four different ways across four files, and the image
        // ceilings existed as byte-identical pairs. Caps live with the code that enforces
        // them; a second declaration is a second policy nobody chose.
        let owners = ["lib.rs", "raster.rs", "walker.rs"];
        // `text::MAX_XY_CUT_DEPTH` is exempt BY NAME: it bounds whitespace-gutter recursion,
        // not form nesting, and shares the value 40 by coincidence. Aliasing the two would
        // tie a layout heuristic to a DoS cap.
        let exempt = ["MAX_XY_CUT_DEPTH"];
        for (file, src) in rust_files(&core_src()) {
            if owners.contains(&file.as_str()) {
                continue;
            }
            for line in code_only(&src).lines() {
                let Some(at) = line.find("const MAX_") else { continue };
                let name: String = line[at + "const ".len()..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                if exempt.contains(&name.as_str()) {
                    continue;
                }
                assert!(
                    !(name.ends_with("_DIM") || name.ends_with("_PIXELS") || name.ends_with("_DEPTH")),
                    "{file} re-declares the cap {name}; caps live in {owners:?}"
                );
            }
        }
    }

    #[test]
    fn the_binding_mints_exceptions_in_one_place() {
        // Seven sites raised `PyValueError` directly, one of them hand-copying the exact
        // `write failed: {e}` string `Error::Write` owns. An exception minted around `to_py`
        // can never become `EncryptedPdfError` — or any future subclass — because the
        // mapping that picks the class was never consulted.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../distillpdf-python/src/lib.rs");
        let src = std::fs::read_to_string(&path).expect("the binding crate is a sibling of this one");
        let found = hits(&src, "PyValueError::new_err");
        assert_eq!(
            found.len(),
            2,
            "only to_py and page_arg may mint an exception; found {} sites:\n  {}",
            found.len(),
            found.join("\n  ")
        );
        // …and they must be those two, not two others.
        let code = code_only(&src);
        let bodies: Vec<&str> = code.split("\nfn ").collect();
        for want in ["to_py(", "page_arg("] {
            assert!(
                bodies.iter().any(|b| b.starts_with(want) && b.contains("PyValueError::new_err")),
                "{want} no longer holds one of the two sanctioned PyValueError sites"
            );
        }
    }
}
