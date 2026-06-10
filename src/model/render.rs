//! Render a loaded [`DocModel`] back to HTML / Markdown / plain text — the proof that
//! "renderers are pure functions of the model" (see docs/datamodel-design.md).
//!
//! ## The design (single-stream, Stage C)
//!
//! We do NOT fork the renderer. The page renderer ([`crate::html`]) is split into a HEAD
//! ([`crate::html::render_doc_elements`]: positional analysis → the per-page element IR + the
//! cross-page transforms), a MIDDLE ([`crate::html::emit_and_merge`]: IR → the PRE-id merged
//! body + `dedup_ids`), and a TAIL ([`crate::html::assemble`]: id-minting + `<nav>` + image
//! substitution). The MODEL is the IR's durable form: [`crate::model::build`] PROJECTS the
//! post-transform element IR into [`crate::model::Block`]s. This module does the inverse — it
//! REBUILDS the page-element IR from those blocks ([`rebuild_ir`]) and runs the SAME middle +
//! tail the parse path runs. So a model-only re-render is the identical emit/merge/assemble
//! code, only the IR's source differs (blocks, not a fresh parse) — equivalence holds by
//! construction, byte-for-byte across page AND section mode (verified across the corpus).
//! Markdown is the HTML→Markdown transform over that HTML; `extract_text` is the visible text
//! of each rebuilt page body — all sourced from the file, with no source PDF present.
//!
//! ## What the blocks carry for this (and why)
//!
//! The blocks are BOTH the queryable structure / index source of truth AND the render source of
//! truth — there is no separate stored body. The query-lossy parts are carried as dedicated
//! fidelity fields on the block (see [`crate::model::Block`]): figures/captions and the
//! page-chrome `header`/`dest_anchors` carriers keep their exact emitted `el_html` fragment;
//! tables keep their header/grid/caption parts; consecutive `list_item`/`footnote` blocks carry
//! an `el_group` so they regroup into the single `<ul>/<ol>` / `<aside>` they came from; and
//! `block.text` is the element's minimal inline HTML. One structure, two faces.

use crate::html::{self, Bbox, ElKind, Mode, PageElement, PageIR};
use crate::links::OutlineEntry;
use crate::markdown::{self, ImgMode};

use super::{Block, BlockKind, DocModel, TocEntry};

/// Rebuild the post-transform page-element IR ([`PageIR`] list) from the model's BLOCKS — the
/// single-stream "renderers are pure functions of the model" property, sourced from blocks (no
/// stored `body_html`). Each block reconstructs the [`crate::html::PageElement`] it was projected
/// from in [`crate::model::build`]; consecutive `list_item` / `footnote` blocks sharing an
/// `el_group` regroup into the single `<ul>/<ol>` / `<aside>` element they came from; figures,
/// captions, and the page-chrome carriers re-emit their stored `el_html` fragment verbatim
/// (already in its FINAL deduped + image-substituted form, so [`crate::html::emit_and_merge`]'s
/// `dedup_ids` is idempotent over it). Pages are emitted in `n` order (the model stores them
/// sorted); a page with no blocks renders as an empty section, matching an empty source page.
fn rebuild_ir(model: &DocModel) -> Vec<PageIR> {
    // Group blocks by their page, preserving reading order.
    let mut by_page: std::collections::BTreeMap<u32, Vec<&Block>> = std::collections::BTreeMap::new();
    for b in &model.blocks {
        by_page.entry(b.page).or_default().push(b);
    }
    let mut pages: Vec<PageIR> = Vec::with_capacity(model.pages.len());
    for p in &model.pages {
        let blocks = by_page.remove(&p.n).unwrap_or_default();
        pages.push((p.n, elements_from_blocks(&blocks), Vec::new()));
    }
    pages
}

/// Reconstruct one page's ordered [`PageElement`] list from its blocks. The `el_group` ordinal
/// groups the consecutive `list_item` / `footnote` blocks a single list / aside was decomposed
/// into; every other block maps one-to-one to its element. Bbox is threaded back (it does not
/// affect emission but keeps the rebuilt IR a faithful twin of the distill-time one).
fn elements_from_blocks(blocks: &[&Block]) -> Vec<PageElement> {
    let mut out: Vec<PageElement> = Vec::with_capacity(blocks.len());
    let mut i = 0;
    while i < blocks.len() {
        let b = blocks[i];
        match b.kind {
            BlockKind::Heading => {
                out.push(PageElement::at(
                    ElKind::Heading { level: b.heading_level.unwrap_or(1), id: String::new(), text: b.text.clone() },
                    b.bbox,
                ));
                i += 1;
            }
            BlockKind::Para => {
                out.push(PageElement::at(ElKind::Para { text: b.text.clone() }, b.bbox));
                i += 1;
            }
            BlockKind::Code => {
                out.push(PageElement::at(ElKind::Code { text: b.text.clone() }, b.bbox));
                i += 1;
            }
            BlockKind::ListItem => {
                // A `list_item` block with NO `el_group` is a PARAGRAPH whose visible text merely
                // opens a list marker ("1. …") — the renderer emitted it as `<p>`, and the
                // `list_item` kind is only a query-view label; reconstruct it as a `Para`. Only an
                // item that came from a real `<ul>/<ol>` List element carries `el_group`.
                if b.el_group.is_none() {
                    out.push(PageElement::at(ElKind::Para { text: b.text.clone() }, b.bbox));
                    i += 1;
                    continue;
                }
                // Absorb the maximal run of `list_item` blocks sharing this one's `el_group`
                // into one `<ul>/<ol>` element (the projection split it into per-item blocks).
                let group = b.el_group;
                let ordered = b.list_ordered.unwrap_or(false);
                let mut items = Vec::new();
                let mut bbox = b.bbox;
                while i < blocks.len()
                    && blocks[i].kind == BlockKind::ListItem
                    && blocks[i].el_group == group
                {
                    items.push(blocks[i].text.clone());
                    bbox = bbox_union(bbox, blocks[i].bbox);
                    i += 1;
                }
                out.push(PageElement::at(ElKind::List { ordered, items }, bbox));
            }
            BlockKind::Footnote => {
                let group = b.el_group;
                let mut notes = Vec::new();
                let mut bbox = b.bbox;
                while i < blocks.len()
                    && blocks[i].kind == BlockKind::Footnote
                    && blocks[i].el_group == group
                {
                    notes.push(blocks[i].text.clone());
                    bbox = bbox_union(bbox, blocks[i].bbox);
                    i += 1;
                }
                out.push(PageElement::at(ElKind::Footnotes { notes }, bbox));
            }
            BlockKind::Table => {
                out.push(PageElement::at(
                    ElKind::Table {
                        header: b.table_header.clone().unwrap_or_default(),
                        grid: b.table_grid.clone().unwrap_or_default(),
                        caption: b.table_caption.clone(),
                    },
                    b.bbox,
                ));
                i += 1;
            }
            BlockKind::Figure => {
                out.push(PageElement::at(
                    ElKind::Figure {
                        html: b.el_html.clone().unwrap_or_default(),
                        id: String::new(),
                        caption: None,
                        image: None,
                        svg: None,
                    },
                    b.bbox,
                ));
                i += 1;
            }
            BlockKind::Caption => {
                out.push(PageElement::at(
                    ElKind::Caption { html: b.el_html.clone().unwrap_or_default(), id: String::new(), text: String::new(), is_figure: false },
                    b.bbox,
                ));
                i += 1;
            }
            BlockKind::Header => {
                out.push(PageElement::at(ElKind::Header(b.el_html.clone().unwrap_or_default()), b.bbox));
                i += 1;
            }
            BlockKind::DestAnchors => {
                out.push(PageElement::at(ElKind::DestAnchors(b.el_html.clone().unwrap_or_default()), b.bbox));
                i += 1;
            }
        }
    }
    out
}

/// Union two bboxes (re-exported shape of `html::bbox_union`, used by the list/footnote regroup).
fn bbox_union(a: Option<Bbox>, b: Option<Bbox>) -> Option<Bbox> {
    html::bbox_union(a, b)
}

/// Reconstruct the PDF's own outline (`/Outlines`) from the model's `toc`, for the `assemble`
/// tail's `nav_from_outline` step. The model's `toc` was built from the same outline at distill
/// time (page-anchored `page-N` entries), with `level` 1-based; `OutlineEntry.level` is 0-based.
/// When the model's `toc` came from the heading tree instead (no PDF outline), its anchors are
/// `sec-…`, never `page-…` — we treat ONLY the page-anchored form as an outline so the tail's
/// "prefer the author's TOC" branch fires exactly when the parse path's did.
fn outline_from_model(toc: &[TocEntry]) -> Vec<OutlineEntry> {
    let is_pdf_outline = toc.iter().any(|e| e.anchor.starts_with("page-"));
    if !is_pdf_outline {
        return Vec::new();
    }
    toc.iter()
        .map(|e| OutlineEntry {
            level: e.level.saturating_sub(1), // model/HTML 1-based → OutlineEntry 0-based
            title: e.title.clone(),
            page: e.page,
        })
        .collect()
}

/// Parse the `image_mode` string the same way the Python boundary does, for a STRING result
/// (no folder to externalize into): `embed` keeps bytes inline, `external` falls back to the
/// given string-fallback, `drop` → placeholders.
fn parse_image_mode(image_mode: &str, string_fallback: ImgMode) -> Result<ImgMode, String> {
    match image_mode {
        "embed" => Ok(ImgMode::Embed),
        "drop" => Ok(ImgMode::Placeholder),
        "external" => Ok(string_fallback),
        other => Err(format!("invalid image_mode {other:?}: expected \"embed\", \"external\", or \"drop\"")),
    }
}

/// Render the model to HTML — the model-only analogue of [`crate::html::to_html`]'s
/// string-returning path.
///
/// `mode` is `"section"`/`"page"`; `include_toc` keeps/drops the `<nav>`. Images: the Wave-1/2
/// born-digital model drops figure bytes (a regenerable stub), so the body carries `<image N>`
/// placeholders and the HTML is the `image_mode="drop"` shape regardless — embed/external
/// re-render awaits the asset-bytes capture (Wave 3/4). This still produces the EXACT HTML the
/// PDF path produces for `image_mode="drop"`, which is the round-trip contract.
pub(crate) fn render_html(model: &DocModel, mode: Mode, include_toc: bool) -> String {
    // Rebuild the post-transform element IR from the blocks, then run the SAME emit + merge +
    // assemble path the PDF parse path runs — so a model-only re-render is the identical code,
    // only the IR source differs (blocks, not a fresh parse). The rebuilt figure/chrome fragments
    // are already in their final deduped + image-substituted form, so `emit_and_merge`'s
    // `dedup_ids` is idempotent over them and the merge carries no `\0idx\0` sentinels (the URI
    // list comes back empty → `assemble`'s `substitute_images` is a no-op).
    let mut pages_ir = rebuild_ir(model);
    // The blocks are projected from the PAGE-mode IR, so they carry the page-LOCAL transforms but
    // NOT the SECTION-mode cross-page merges (a list / display equation straddling a page break is
    // folded into one element only when the per-page bodies are concatenated bare — see
    // `elem_passes`). Re-run the cross-page passes on the rebuilt IR for the requested mode: in
    // page mode they are page-local + idempotent (a no-op over the already-transformed IR); in
    // section mode they additionally apply the straddling-list / straddling-math merges page mode
    // omits, so the section-mode re-render matches `to_html(section)` byte-for-byte.
    crate::elem_passes::run_cross_page_passes(&mut pages_ir, mode);
    let (body, _img_uris) = html::emit_and_merge(&pages_ir, mode);
    let outline = outline_from_model(&model.toc);
    html::assemble(body, mode, include_toc, &outline, &[], false)
}

/// Render the model to Markdown — the existing HTML→Markdown transform over [`render_html`],
/// so every Markdown improvement flows in for free (the same property the PDF path has).
/// Returns the Markdown plus any figure files (empty here — the Wave-1/2 model has no bytes).
pub(crate) fn render_markdown(model: &DocModel, mode: Mode, include_toc: bool, image_mode: &str) -> Result<(String, Vec<markdown::ImageFile>), String> {
    let im = parse_image_mode(image_mode, ImgMode::Placeholder)?;
    let html = render_html(model, mode, include_toc);
    Ok(markdown::html_to_markdown(&html, include_toc, im))
}

/// Plain text of the model — the model-only analogue of `Pdf.extract_text`: the visible text
/// of each page's body in reading order, one page per line (matching `extract_text`'s per-page
/// `\n` joining). SVG subtrees are dropped (their `<text>` labels are figure-internal, not page
/// prose) and the remaining inline tags stripped, so this surfaces ALL visible page text —
/// table cells and figure captions included, exactly like `extract_text` — not just block
/// `text` (tables/figures carry their text in `cells`/`caption`, which a block-text-only join
/// would miss).
///
/// RESIDUE (documented, not papered over): this is TOKEN-equivalent to `Pdf.extract_text()`,
/// not byte-equal. `extract_text` is a separate POSITIONAL extractor (raw text spans, its own
/// word-break and blank-line heuristics) — a different code path from rendering, with no HTML
/// in between — so the model (which carries the *rendered* structure, not the raw span stream)
/// cannot reproduce its exact intra-page whitespace. The two agree on the WORD sequence, which
/// is the substantive content; the round-trip regression asserts that token equality (and pins
/// HTML/Markdown byte-for-byte, since those ARE pure HTML transforms).
pub(crate) fn extract_text(model: &DocModel) -> String {
    // Each page's body is the emit of its rebuilt element IR (the page-mode INNER, the same body
    // the fidelity render carries) — SVG subtrees dropped, inline tags → token boundaries.
    let pages_ir = rebuild_ir(model);
    let mut out = String::new();
    for (_pno, els, _uris) in &pages_ir {
        let body = html::emit_page_elements(els);
        let no_svg = strip_svg(&body);
        out.push_str(visible_text(&no_svg).trim());
        out.push('\n');
    }
    out
}

/// Visible text of an HTML fragment with EVERY tag replaced by a space (then whitespace
/// collapsed). Distinct from `nav::strip_inline`, which drops tags with no separator — here a
/// tag boundary MUST become a token boundary so adjacent table cells / blocks
/// (`<td>A</td><td>B</td>`) read as separate words `A B`, matching `extract_text`'s tokens.
fn visible_text(html: &str) -> String {
    let mut s = String::with_capacity(html.len());
    let mut intag = false;
    for c in html.chars() {
        match c {
            '<' => {
                intag = true;
                s.push(' ');
            }
            '>' => intag = false,
            _ if !intag => s.push(c),
            _ => {}
        }
    }
    let s = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    // collapse runs of whitespace to single spaces.
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// Remove `<svg>…</svg>` subtrees (verbatim, balanced) from a body fragment.
fn strip_svg(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find("<svg") {
        out.push_str(&rest[..open]);
        match rest[open..].find("</svg>") {
            Some(rel) => rest = &rest[open + rel + "</svg>".len()..],
            None => {
                rest = ""; // unterminated — drop the tail
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, BlockKind, Coverage, Indexes, Metadata, Page, Source, NATIVE_CONFIDENCE, SCHEMA_VERSION};
    use std::collections::BTreeMap;

    /// A model built from BLOCKS (the render source of truth, post-`body_html`). Pages carry
    /// geometry only; render rebuilds the page-element IR from the per-page blocks.
    fn model_with_blocks(npages: u32, blocks: Vec<Block>) -> DocModel {
        let pages = (1..=npages)
            .map(|n| Page {
                n,
                width_pts: 612.0,
                height_pts: 792.0,
                labels: BTreeMap::new(),
                ocr_decision: None,
                active_ocr_pass: None,
            })
            .collect();
        DocModel {
            schema_version: SCHEMA_VERSION,
            source: Source { file: "x.pdf".into(), sha256: "ab".into(), pages: npages, distillpdf: "0".into(), generated_at: "t".into() },
            metadata: Metadata::default(),
            pages,
            ocr_passes: Vec::new(),
            sections: Vec::new(),
            blocks,
            indexes: Indexes { coverage: Coverage::default(), ..Default::default() },
            assets: Vec::new(),
            chunks: None,
            embedding_spaces: Vec::new(),
            links: Vec::new(),
            named_dests: Vec::new(),
            toc: Vec::new(),
        }
    }

    fn blk(id: &str, kind: BlockKind, page: u32, text: &str) -> Block {
        Block {
            id: id.into(),
            kind,
            text: text.into(),
            page,
            section: None,
            bbox: None,
            confidence: NATIVE_CONFIDENCE,
            ocr_pass: None,
            heading_level: None,
            cells: None,
            image: None,
            label: None,
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
    fn page_mode_mints_ids_and_nav() {
        let h = Block { heading_level: Some(1), ..blk("b0001", BlockKind::Heading, 1, "Intro") };
        let p = blk("b0002", BlockKind::Para, 1, "body");
        let m = model_with_blocks(1, vec![h, p]);
        let html = render_html(&m, Mode::Page, true);
        // build_toc minted the heading id and built the nav (page-mode keeps <section data-page>).
        assert!(html.contains("<section data-page=\"1\" id=\"page-1\">"));
        assert!(html.contains("<h1 id=\"sec-intro\">Intro</h1>"));
        assert!(html.contains("<nav>") && html.contains("href=\"#sec-intro\""));
    }

    #[test]
    fn section_mode_wraps_sections() {
        let h = Block { heading_level: Some(1), ..blk("b0001", BlockKind::Heading, 1, "Intro") };
        let p = blk("b0002", BlockKind::Para, 1, "body");
        let m = model_with_blocks(1, vec![h, p]);
        let html = render_html(&m, Mode::Section, false);
        // section mode regroups into a <section id="sec-…"> wrapper (no page wrappers).
        assert!(html.contains("<section id=\"sec-intro\">"));
        assert!(!html.contains("data-page"));
    }

    #[test]
    fn extract_text_surfaces_all_visible_text_and_drops_svg() {
        // The visible text of the rebuilt page body — table cells + figure captions included,
        // markup stripped, SVG label text dropped — one page per line.
        let h = Block { heading_level: Some(1), ..blk("b0001", BlockKind::Heading, 1, "Title") };
        let table = Block {
            table_header: Some(vec![vec![("Cell".into(), 1)]]),
            table_grid: Some(vec![]),
            ..blk("b0002", BlockKind::Table, 1, "")
        };
        let fig = Block {
            el_html: Some("<figure><svg><text>axis</text></svg><figcaption>Cap</figcaption></figure>".into()),
            ..blk("b0003", BlockKind::Figure, 1, "")
        };
        let para = blk("b0004", BlockKind::Para, 1, "Body <b>bold</b>");
        let m = model_with_blocks(1, vec![h, table, fig, para]);
        let txt = extract_text(&m);
        assert!(txt.contains("Title") && txt.contains("Cell") && txt.contains("Cap") && txt.contains("Body bold"));
        assert!(!txt.contains("axis"), "SVG label text is figure-internal, not page prose");
        assert!(!txt.contains('<'), "all markup stripped");
        assert!(txt.ends_with('\n'), "one page per line");
    }

    #[test]
    fn markdown_transforms_the_rendered_html() {
        let h = Block { heading_level: Some(1), ..blk("b0001", BlockKind::Heading, 1, "Intro") };
        let p = blk("b0002", BlockKind::Para, 1, "hello");
        let m = model_with_blocks(1, vec![h, p]);
        let (md, files) = render_markdown(&m, Mode::Section, false, "drop").unwrap();
        assert!(md.contains("# Intro") && md.contains("hello"));
        assert!(files.is_empty());
    }
}
