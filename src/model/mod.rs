//! The `.dpdf` document model — distillPDF's durable, re-renderable analysis snapshot.
//!
//! distillPDF builds a typed element tree per document (reading order, headings, the section
//! tree, tables, figures, OCR provenance) and today renders it to HTML and throws it away.
//! This module persists that analysis instead, so HTML / Markdown / text can be re-derived
//! from the file, with different options, forever — and so a single document is queryable on
//! its own and is the ingestion contract for downstream corpus systems (kglite-docs).
//!
//! **Status: experimental (`schema_version = 0`).** The shape is NOT yet a stability
//! commitment — it goes to `1` only after the first downstream cutover survives it
//! (see docs/datamodel-design.md). Treat every field as provisional.
//!
//! ## Layering (Wave 1)
//!
//! - this file: the serde structs, the legibility metric, and `derive_indexes` (the
//!   deterministic blocks → indexes pass — indexes are DERIVED, never hand-maintained, so
//!   drift is impossible by construction).
//! - `build.rs`: constructs a [`DocModel`] from a loaded PDF by parsing the element stream
//!   that `to_html` already produces (page mode). This REUSES the existing analysis rather
//!   than forking it; the renderer-from-model refactor is Wave 2.
//! - `container.rs`: the `.dpdf` zip (`model.json` + `img/` assets) save/load, with the three
//!   asset storage modes (embedded / external / dropped-with-stub).

// The OCR-provenance surface (legibility metric, pass builder, OcrPass/OcrResult fields) is
// DEFINED in Wave 1 and WIRED in Wave 2 (the agent/native OCR backends write passes into the
// model). The born-digital distill path doesn't call it yet, so parts read as `dead_code` in
// a non-test build — allowed deliberately, the same way `ocr/mod.rs` allows its
// incrementally-wired surface. The spec explicitly asks for these exposed on the structs.
#![allow(dead_code)]

pub(crate) mod build;
pub(crate) mod container;
pub(crate) mod render;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The current model schema version. `0` = experimental until the first downstream cutover
/// survives the shape; see the module docs.
pub(crate) const SCHEMA_VERSION: u32 = 0;

/// Which binary assets a `distill`/save keeps. Size is a CHOICE, never a surprise (the asset
/// policy in docs/datamodel-design.md). Every profile keeps the asset STUBS (hash/dims/regen)
/// — only the bytes differ — so a dropped asset is always a named, reversible hole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetProfile {
    /// `assets="none"` — text + structure only; ALL asset bytes dropped (stubs remain). A few
    /// MB even for a 1,500-page scan; emailable.
    None,
    /// `assets="figures"` (default) — embed figure image bytes; page rasters (OCR inputs) stay
    /// dropped-with-stub (they're regenerable).
    Figures,
    /// `assets="full"` — figures AND page rasters (the audit archive: "this is the image the
    /// model read"). Wave 2 has no page-raster capture yet, so this currently equals `Figures`
    /// for the born-digital path; the variant exists so the surface is stable.
    Full,
}

impl AssetProfile {
    /// Parse the `assets=` string the Python `distill` accepts.
    pub(crate) fn parse(s: &str) -> Result<AssetProfile, String> {
        match s {
            "none" => Ok(AssetProfile::None),
            "figures" => Ok(AssetProfile::Figures),
            "full" => Ok(AssetProfile::Full),
            other => Err(format!("invalid assets {other:?}: expected \"figures\", \"full\", or \"none\"")),
        }
    }

    /// Whether figure image bytes are kept (embedded) under this profile.
    fn keeps_figures(self) -> bool {
        matches!(self, AssetProfile::Figures | AssetProfile::Full)
    }
}

/// Confidence of a block whose text comes from the PDF's NATIVE text layer (not OCR).
/// `1.0` is the design's sentinel for "born-digital, no OCR uncertainty".
pub(crate) const NATIVE_CONFIDENCE: f32 = 1.0;

/// A bounding box in PDF user space `[x0, y0, x1, y1]` (origin bottom-left, points), threaded
/// from the render walk's positioned items onto each block. `Option` on a block — `None` for a
/// construct the walk produced from no positioned line (rare); 100% of content blocks carry one
/// on the born-digital path.
pub(crate) type Bbox = [f32; 4];

/// The whole document model — the root of `model.json`.
///
/// Field order here is also the JSON key order under serde's struct serialization, but the
/// container writer canonicalizes to SORTED keys regardless (see `container::to_canonical_json`)
/// so save→load→save is byte-identical. Nothing here carries a timestamp except
/// [`Source::generated_at`], taken once at distill time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct DocModel {
    pub schema_version: u32,
    pub source: Source,
    pub metadata: Metadata,
    pub pages: Vec<Page>,
    /// Append-only OCR history; empty for a born-digital distill. Wave 2 populates this from
    /// the agent/native OCR backends.
    #[serde(default)]
    pub ocr_passes: Vec<OcrPass>,
    pub sections: Vec<Section>,
    pub blocks: Vec<Block>,
    pub indexes: Indexes,
    #[serde(default)]
    pub assets: Vec<Asset>,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub named_dests: Vec<NamedDest>,
    #[serde(default)]
    pub toc: Vec<TocEntry>,
}

/// Binds the model to its source PDF (by hash) and records the extractor version + the one
/// timestamp in the file. A model is a snapshot of extractor quality at `generated_at`;
/// `distillpdf` + `schema_version` make that explicit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Source {
    pub file: String,
    pub sha256: String,
    pub pages: u32,
    pub distillpdf: String,
    pub generated_at: String,
}

/// Document front-matter / catalog metadata. Sparse by design — only what the extractor
/// understood. `total = false` semantics via `#[serde(default)]` on each optional field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Metadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abstract_text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

/// One physical page. `labels` is an EXTENSIBLE map (a `BTreeMap` for deterministic key
/// order): the core fills `"pdf"` from `/PageLabels` when present; downstream verticals may
/// write others (e-filing stamps, etc.) — addresses are separated from labels so citations
/// resolve both ways.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Page {
    pub n: u32,
    pub width_pts: f32,
    pub height_pts: f32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// detect.rs's OCR decision for the page (`NotNeeded` / `NeedsOcr` / `DropAndOcr`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_decision: Option<OcrDecision>,
    /// Which OCR pass feeds this page's blocks/renders (`None` = native text layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_ocr_pass: Option<String>,
    // (No `body_html`: the per-page render-fidelity body USED to be stored verbatim here, but the
    // BLOCKS are now the single render source of truth — `render::render_html` rebuilds the
    // page-element IR from them byte-identically (Stage C of the single-stream refactor). The
    // page's content lives entirely in `DocModel.blocks`, indexed by page; nothing is duplicated.)
}

/// Mirror of `ocr::detect::OcrDecision` as a serde enum (kept here so the model module owns
/// its wire shape and doesn't leak the internal type's representation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum OcrDecision {
    NotNeeded,
    NeedsOcr,
    DropAndOcr,
}

impl From<crate::ocr::detect::OcrDecision> for OcrDecision {
    fn from(d: crate::ocr::detect::OcrDecision) -> Self {
        match d {
            crate::ocr::detect::OcrDecision::NotNeeded => OcrDecision::NotNeeded,
            crate::ocr::detect::OcrDecision::NeedsOcr => OcrDecision::NeedsOcr,
            crate::ocr::detect::OcrDecision::DropAndOcr => OcrDecision::DropAndOcr,
        }
    }
}

/// One OCR pass over (some of) the pages: an append-only, comparable history entry. Text is
/// cheap (KB/page), so the model keeps EVERY pass — the triage Tesseract pass, the VLM
/// re-OCR, the strongest-agent escalation — and `active_ocr_pass` picks which feeds blocks.
/// Wave 2 writes these; Wave 1 only defines the shape + the legibility metric below.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct OcrPass {
    pub id: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    pub results: Vec<OcrResult>,
}

/// One page's outcome within an [`OcrPass`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct OcrResult {
    pub page: u32,
    pub outcome: OcrOutcome,
    /// Legible alphanumeric character count (see [`legible_chars`]) — the size of the
    /// trustworthy text recovered, distinct from the raw string length.
    pub legible_chars: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctags: Option<String>,
}

/// Per-page OCR legibility band. The honest-coverage signal: an illegible page is a NAMED
/// hole a consumer can see, not a silent empty.
// The `Ocr` prefix is intentional: the snake_case wire names are exactly `ocr_ok` /
// `ocr_partial` / `ocr_illegible` (the design's per-page legibility band), so the variant
// names match the serialized values one-to-one.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OcrOutcome {
    OcrOk,
    OcrPartial,
    OcrIllegible,
}

/// A section in the document's heading tree (flat list; `parent` links rebuild the tree).
/// `id` is the `sec-…` slug the renderer already mints, so model ids == HTML ids == the
/// CLI/agent addresses — one id space across every face of the document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Section {
    pub id: String,
    pub level: u8,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub page_start: u32,
    pub page_end: u32,
}

/// One content block in reading order — the heart of the model. `id` is `b0001`-style, an
/// ordinal in reading order, scoped to the file (cross-file stability is the re-distill
/// snapshot question, already accepted in the design).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Block {
    pub id: String,
    pub kind: BlockKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    pub page: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Bbox>,
    /// `1.0` = native text layer; lower means OCR-derived (see [`NATIVE_CONFIDENCE`]).
    pub confidence: f32,
    /// Block-level OCR provenance: which pass produced this text (`None` = native).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_pass: Option<String>,
    // ---- kind-specific fields (only the ones that apply are serialized) ----
    /// Heading depth (1..6) for `kind = heading`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_level: Option<u8>,
    /// Row-major cell grid for `kind = table`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cells: Option<Vec<Vec<String>>>,
    /// Asset id (`img/…`) of a `kind = figure`'s image, when one was extracted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Parsed element label ("Table 3", "Figure 1") — separated from the address so
    /// label ↔ block-id resolves both ways (robust citations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Figure/table caption text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,

    // ---- render-reconstruction fields (Stage C; the model is the render source of truth) ----
    // These let `render::render_html` rebuild the byte-identical page-element IR from blocks
    // alone — the single-stream "renderers are pure functions of the model" property, now WITHOUT
    // a stored `body_html`. They are FIDELITY data, distinct from the query fields above.
    /// For `kind = list_item`: the `<ul>`/`<ol>` tag (`true` = ordered) the item belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_ordered: Option<bool>,
    /// Groups consecutive `list_item` (→ one `<ul>/<ol>`) or `footnote` (→ one `<aside>`) blocks
    /// back into the single element they were projected from, so two adjacent distinct lists /
    /// asides don't merge on reconstruction. A per-document ordinal; same group ⇒ same element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub el_group: Option<u32>,
    /// A `kind = table`'s detached header rows (`(cell text, colspan)`), distinct from the data
    /// `cells` grid — the exact parts `table_html_from_parts` re-emits. (`cells` carries the
    /// query view: header rows expanded + the grid.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_header: Option<Vec<Vec<(String, usize)>>>,
    /// A `kind = table`'s data grid (without the detached header rows), for faithful re-emit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_grid: Option<Vec<Vec<String>>>,
    /// A `kind = table`'s caption as `(number, inner-html, below)` — the renderer's
    /// `<caption>` parts (separate from the plain-text `caption` query field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_caption: Option<(String, String, bool)>,
    /// The EXACT emitted HTML fragment for constructs not faithfully reconstructible from the
    /// structured fields alone — `figure`/`caption` (SVG, overlays, composite, captions) and the
    /// page-chrome `header`/`dest_anchors` carriers. SVG subtrees are pulled out to `\0svg:ID\0`
    /// sentinels (the bytes live in a `kind = svg` asset); render splices them back. `None` for
    /// the structured kinds (heading/para/list_item/code/table/footnote).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub el_html: Option<String>,
}

/// The block kinds the extractor distinguishes. Lowercase wire names match the design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BlockKind {
    Heading,
    Para,
    ListItem,
    Table,
    Figure,
    Caption,
    Footnote,
    /// A `<pre><code>…</code></pre>` monospace/code block.
    Code,
    /// The first-page semantic `<header>` front-matter carrier (fidelity-only; reconstructed
    /// from `el_html`, since rebuilding it from `metadata` is lossy — sup author markers, the
    /// affiliation `<ol>`). Not a queryable content block.
    Header,
    /// The page-head named-destination `<a id>` anchors carrier (fidelity-only). Not content.
    DestAnchors,
}

impl BlockKind {
    /// The wire name (matches the serde rename) — used by `derive_indexes` for the `kinds`
    /// index keys.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BlockKind::Heading => "heading",
            BlockKind::Para => "para",
            BlockKind::ListItem => "list_item",
            BlockKind::Table => "table",
            BlockKind::Figure => "figure",
            BlockKind::Caption => "caption",
            BlockKind::Footnote => "footnote",
            BlockKind::Code => "code",
            BlockKind::Header => "header",
            BlockKind::DestAnchors => "dest_anchors",
        }
    }
}

/// DERIVED views over `blocks` — stored in the file (tiny, and they make the raw JSON
/// self-describing for Tier-1 agents) but formally regenerable via [`derive_indexes`]. Any
/// mutation that touches blocks must re-derive, so drift is impossible by construction.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Indexes {
    /// page number (as a string key, for JSON-object stability) → block ids on that page.
    pub pages: BTreeMap<String, Vec<String>>,
    /// section id → block ids belonging to it.
    pub sections: BTreeMap<String, Vec<String>>,
    /// kind name → labelled entries (label/page/block-id) for tables/figures/footnotes/etc.
    pub kinds: BTreeMap<String, Vec<KindEntry>>,
    pub coverage: Coverage,
}

/// A `kinds`-index entry: the address (block id, page) plus the human/parsed label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct KindEntry {
    pub id: String,
    pub page: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Validated index coverage — surfaced so a consumer sees "97% sectioned, 3% unsectioned
/// (page-reachable)" rather than trusting a green light over invisible content.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Coverage {
    /// Fraction of blocks that belong to a section (front-matter before the first heading is
    /// legitimately unsectioned, not an error).
    pub sectioned: f32,
    /// Block ids with no section (the explicit unsectioned bucket).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsectioned_blocks: Vec<String>,
}

/// A binary asset (figure image, page raster, vector→SVG). Every asset carries a `storage`
/// mode so size is a CHOICE, never a surprise: `embedded` keeps the bytes in the container,
/// `external` references a sibling path, `dropped` keeps only a stub (hash + dims + a
/// `regen` recipe) — a named, reversible hole.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Asset {
    pub id: String,
    pub kind: AssetKind,
    pub storage: AssetStorage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// How to rebuild the asset from the source PDF (page + dpi for a page raster, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regen: Option<Regen>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssetKind {
    Figure,
    PageRaster,
    Svg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssetStorage {
    Embedded,
    External,
    Dropped,
}

/// A recipe to regenerate a dropped/external asset from the hash-verified source PDF.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Regen {
    pub page: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi: Option<u32>,
}

/// A hyperlink (external URI or internal destination).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Link {
    pub page: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_name: Option<String>,
}

/// A PDF named destination (a label that resolves to a page/position).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct NamedDest {
    pub name: String,
    pub page: u32,
}

/// A table-of-contents entry (from the PDF's own `/Outlines` when present, else detected
/// headings).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct TocEntry {
    pub level: u8,
    pub title: String,
    pub page: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub anchor: String,
}

// ---- legibility metric -----------------------------------------------------

/// Count the LEGIBLE alphanumeric characters in an OCR result, after stripping the
/// DocTags/structure markers (anything inside `[...]` or `<...>`).
///
/// Rationale: a raw character count over-credits an illegible page — a VLM emitting
/// `<loc_…>` tags, `[unclear]` placeholders, and punctuation soup reports a large string
/// length while carrying almost no readable content. Counting only alphanumerics OUTSIDE the
/// markers gives an honest size of the trustworthy text, which then drives the
/// `ocr_ok | ocr_partial | ocr_illegible` band (a coverage signal, per the north star).
///
/// Markers stripped: `[...]` (square-bracket annotations like `[unclear]`/`[FORMULA]`) and
/// `<...>` (DocTags element/location tags). Bracket nesting is handled by a depth counter so
/// a stray unmatched bracket doesn't swallow the rest of the page.
pub(crate) fn legible_chars(text: &str) -> u32 {
    let mut count: u32 = 0;
    let mut angle_depth: u32 = 0; // inside <...>
    let mut square_depth: u32 = 0; // inside [...]
    for c in text.chars() {
        match c {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '[' => square_depth += 1,
            ']' => square_depth = square_depth.saturating_sub(1),
            _ if angle_depth == 0 && square_depth == 0 && c.is_alphanumeric() => count += 1,
            _ => {}
        }
    }
    count
}

/// Bands above ~10% of a "full" page of legible text are `ok`, a sliver is `partial`, and
/// near-nothing is `illegible`. `page_chars_full` is the rough legible-char count a fully
/// legible page of this size would carry (caller-supplied — Wave 2 estimates it from page
/// area / font size; Wave 1 callers may pass a fixed reference). Kept as a pure function so
/// the threshold is testable and tunable in one place.
pub(crate) fn classify_outcome(legible: u32, page_chars_full: u32) -> OcrOutcome {
    let full = page_chars_full.max(1);
    let frac = legible as f32 / full as f32;
    if frac >= 0.5 {
        OcrOutcome::OcrOk
    } else if frac >= 0.05 {
        OcrOutcome::OcrPartial
    } else {
        OcrOutcome::OcrIllegible
    }
}

/// Build an append-only [`OcrPass`] from a `{1-based page: text/doctags}` map — the same
/// shape the `set_ocr` cache uses. This is the wiring point for the legibility metric: each
/// page's result carries its [`legible_chars`] and the resulting [`OcrOutcome`] band, so OCR
/// provenance + the honest-coverage signal land in the model the moment text is stored.
///
/// `page_chars_full` is the reference "fully-legible page" char count used to band the
/// outcome (see [`classify_outcome`]). `is_doctags` records the text under `doctags` vs
/// `text` (the model keeps the raw model output either way). `params`/`generated_at` are
/// carried verbatim for audit/comparison across passes.
pub(crate) fn ocr_pass_from_pages(
    id: &str,
    engine: &str,
    params: BTreeMap<String, String>,
    generated_at: Option<String>,
    pages: &BTreeMap<u32, String>,
    page_chars_full: u32,
    is_doctags: bool,
) -> OcrPass {
    let results = pages
        .iter()
        .map(|(&page, content)| {
            let legible = legible_chars(content);
            OcrResult {
                page,
                outcome: classify_outcome(legible, page_chars_full),
                legible_chars: legible,
                confidence: None,
                text: (!is_doctags).then(|| content.clone()),
                doctags: is_doctags.then(|| content.clone()),
            }
        })
        .collect();
    OcrPass { id: id.to_string(), engine: engine.to_string(), params, generated_at, results }
}

// ---- index derivation ------------------------------------------------------

/// Rebuild ALL indexes from `blocks` + `sections` by a deterministic pass. This is the only
/// writer of [`Indexes`]; the model never hand-maintains them. Determinism: block order is
/// the authoritative reading order, and every map is a `BTreeMap` (sorted keys) so the JSON
/// is byte-stable across runs.
///
/// - `pages`: page number → block ids in reading order.
/// - `sections`: section id → block ids in reading order.
/// - `kinds`: kind name → labelled entries for tables/figures/footnotes (the "navigable"
///   kinds — headings/paras/list-items are addressable via `pages`/`sections` and would only
///   bloat this).
/// - `coverage`: the sectioned fraction + the explicit unsectioned bucket.
pub(crate) fn derive_indexes(blocks: &[Block]) -> Indexes {
    let mut pages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut kinds: BTreeMap<String, Vec<KindEntry>> = BTreeMap::new();
    let mut unsectioned: Vec<String> = Vec::new();

    // The kinds worth a top-level index: the labelled, cross-referenced elements. Headings,
    // paras and list-items are already reachable via pages/sections.
    let indexed_kind = |k: BlockKind| matches!(k, BlockKind::Table | BlockKind::Figure | BlockKind::Footnote);

    for b in blocks {
        pages.entry(b.page.to_string()).or_default().push(b.id.clone());
        match &b.section {
            Some(s) => sections.entry(s.clone()).or_default().push(b.id.clone()),
            None => unsectioned.push(b.id.clone()),
        }
        if indexed_kind(b.kind) {
            kinds.entry(b.kind.as_str().to_string()).or_default().push(KindEntry {
                id: b.id.clone(),
                page: b.page,
                label: b.label.clone(),
            });
        }
    }

    let total = blocks.len();
    let sectioned_count = total - unsectioned.len();
    let sectioned = if total == 0 { 1.0 } else { sectioned_count as f32 / total as f32 };

    Indexes {
        pages,
        sections,
        kinds,
        coverage: Coverage { sectioned, unsectioned_blocks: unsectioned },
    }
}

/// Rebuild ALL of `model.indexes` from `model.blocks` in place — the single re-derive entry
/// point. Any mutation that touches blocks (Wave 2+: switching `active_ocr_pass` and
/// re-deriving the page's blocks) must call this so the stored indexes can never drift from
/// content. `container::save` calls it as a cheap guard and then asserts the stored indexes
/// already equalled the derived ones (a build that forgot to re-derive is a loud save error,
/// not silent drift).
pub(crate) fn reindex(model: &mut DocModel) {
    model.indexes = derive_indexes(&model.blocks);
}

/// Validate that the model's indexes are CONSISTENT with its blocks before persisting:
/// (1) the stored indexes equal a fresh derive (no drift), and (2) every block is reachable
/// from the page index AND from either a section or the explicit unsectioned bucket (no
/// orphan / silently-unreachable content). Returns `Err(reason)` on any violation — the
/// honest-coverage north star: a coverage hole is a typed error at save, never a silent one.
pub(crate) fn validate_indexes(model: &DocModel) -> Result<(), String> {
    let derived = derive_indexes(&model.blocks);
    if derived != model.indexes {
        return Err("indexes drifted from blocks (call reindex before save)".to_string());
    }
    let all: std::collections::BTreeSet<&str> = model.blocks.iter().map(|b| b.id.as_str()).collect();
    let paged: std::collections::BTreeSet<&str> =
        model.indexes.pages.values().flatten().map(String::as_str).collect();
    if paged != all {
        return Err(format!(
            "page index does not reach every block ({} blocks, {} page-indexed)",
            all.len(),
            paged.len()
        ));
    }
    let sectioned: std::collections::BTreeSet<&str> =
        model.indexes.sections.values().flatten().map(String::as_str).collect();
    let unsectioned: std::collections::BTreeSet<&str> =
        model.indexes.coverage.unsectioned_blocks.iter().map(String::as_str).collect();
    for id in &all {
        if !sectioned.contains(id) && !unsectioned.contains(id) {
            return Err(format!("block {id} is in no section and not in the unsectioned bucket"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(id: &str, kind: BlockKind, page: u32, section: Option<&str>, label: Option<&str>) -> Block {
        Block {
            id: id.into(),
            kind,
            text: String::new(),
            page,
            section: section.map(String::from),
            bbox: None,
            confidence: NATIVE_CONFIDENCE,
            ocr_pass: None,
            heading_level: None,
            cells: None,
            image: None,
            label: label.map(String::from),
            caption: None,
            list_ordered: None,
            el_group: None,
            table_header: None,
            table_grid: None,
            table_caption: None,
            el_html: None,
        }
    }

    #[test]
    fn legibility_strips_markers() {
        // DocTags location tags and [unclear] annotations don't count toward legible text.
        assert_eq!(legible_chars("<loc_10><loc_20>hello<otsl>"), 5); // only "hello"
        assert_eq!(legible_chars("[unclear][FORMULA]"), 0);
        assert_eq!(legible_chars("abc [noise xyz] def"), 6); // "abc" + "def"
        assert_eq!(legible_chars("plain text 123"), 12); // alnum only (5+4+3), spaces excluded
        // an unmatched bracket must not swallow the rest of the page
        assert_eq!(legible_chars("ok ] more"), 6); // "ok" + "more"
    }

    #[test]
    fn outcome_bands() {
        assert_eq!(classify_outcome(600, 1000), OcrOutcome::OcrOk);
        assert_eq!(classify_outcome(200, 1000), OcrOutcome::OcrPartial);
        assert_eq!(classify_outcome(10, 1000), OcrOutcome::OcrIllegible);
        // degenerate full-count never divides by zero
        assert_eq!(classify_outcome(0, 0), OcrOutcome::OcrIllegible);
    }

    #[test]
    fn derive_indexes_is_deterministic() {
        let blocks = vec![
            blk("b0001", BlockKind::Heading, 1, Some("sec-intro"), None),
            blk("b0002", BlockKind::Para, 1, Some("sec-intro"), None),
            blk("b0003", BlockKind::Table, 2, Some("sec-results"), Some("Table 1")),
            blk("b0004", BlockKind::Figure, 2, Some("sec-results"), Some("Figure 1")),
        ];
        let a = derive_indexes(&blocks);
        let b = derive_indexes(&blocks);
        // Same input → identical (incl. JSON key order, since BTreeMap).
        assert_eq!(a, b);
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
        // pages index threads block ids per page in reading order.
        assert_eq!(a.pages.get("1").unwrap(), &["b0001", "b0002"]);
        assert_eq!(a.pages.get("2").unwrap(), &["b0003", "b0004"]);
        // sections index groups by section.
        assert_eq!(a.sections.get("sec-results").unwrap(), &["b0003", "b0004"]);
        // kinds index carries label + address for tables/figures; not headings/paras.
        assert_eq!(a.kinds.get("table").unwrap()[0].label.as_deref(), Some("Table 1"));
        assert!(a.kinds.get("heading").is_none());
        assert!(a.kinds.get("para").is_none());
        // full coverage when every block is sectioned.
        assert_eq!(a.coverage.sectioned, 1.0);
        assert!(a.coverage.unsectioned_blocks.is_empty());
    }

    #[test]
    fn ocr_pass_wires_legibility() {
        let mut pages = BTreeMap::new();
        pages.insert(1u32, "<loc_0>good clean text here that is plenty<otsl>".to_string());
        pages.insert(2u32, "[unclear]<loc_5>".to_string()); // illegible
        let pass = ocr_pass_from_pages(
            "p1",
            "tesseract",
            BTreeMap::new(),
            None,
            &pages,
            50, // reference full-page legible chars
            true,
        );
        assert_eq!(pass.id, "p1");
        assert_eq!(pass.results.len(), 2);
        // page 1: many legible chars (text minus the <loc>/<otsl> markers).
        assert!(pass.results[0].legible_chars > 20);
        assert_eq!(pass.results[0].outcome, OcrOutcome::OcrOk);
        assert_eq!(pass.results[0].doctags.is_some(), true);
        // page 2: nothing legible → illegible band (a named hole).
        assert_eq!(pass.results[1].legible_chars, 0);
        assert_eq!(pass.results[1].outcome, OcrOutcome::OcrIllegible);
    }

    #[test]
    fn coverage_surfaces_unsectioned_front_matter() {
        // A front-matter block before the first heading is legitimately unsectioned —
        // it must show up in the bucket, not be a silent hole.
        let blocks = vec![
            blk("b0001", BlockKind::Para, 1, None, None), // front matter
            blk("b0002", BlockKind::Heading, 1, Some("sec-a"), None),
            blk("b0003", BlockKind::Para, 1, Some("sec-a"), None),
        ];
        let idx = derive_indexes(&blocks);
        assert_eq!(idx.coverage.unsectioned_blocks, vec!["b0001"]);
        assert!((idx.coverage.sectioned - 2.0 / 3.0).abs() < 1e-6);
    }
}
