//! Render a loaded [`DocModel`] back to HTML / Markdown / plain text — the proof that
//! "renderers are pure functions of the model" (see docs/datamodel-design.md).
//!
//! ## The design decision (Wave 2)
//!
//! We do NOT fork the renderer. The page renderer ([`crate::html`]) is split into a HEAD
//! (`render_doc`: all the positional analysis → the PRE-id page-mode body) and a TAIL
//! (`assemble`: id-minting + `<nav>` + image substitution). The model captures each page's
//! PRE-id body verbatim at distill time (`Page.body_html`); this module reassembles those
//! page bodies into the exact same merged `body` the parse path produces, then runs the
//! IDENTICAL [`crate::html::assemble`] tail. So a model-only re-render is the same code path
//! as a fresh parse — equivalence holds by construction, not by a parallel implementation
//! that could drift. Markdown is then the existing HTML→Markdown transform over that HTML,
//! and `extract_text` concatenates the page bodies' visible text — the same shapes the PDF
//! paths produce, but sourced from the file, with no source PDF present.
//!
//! ## What this needs from the model (and why)
//!
//! The block decomposition (the queryable structure + the index source of truth) is LOSSY for
//! rendering — it drops inline emphasis, list `<ul>/<ol>` grouping, `<th>/<td>` table
//! structure, footnote `<aside>`s, front-matter `<header>`s, and SVG. So the render-fidelity
//! data lives in `Page.body_html` (the verbatim page body), not reconstructed from blocks.
//! Re-rendering from blocks alone is intentionally NOT attempted: it would be a second,
//! drifting renderer — exactly what the split avoids.

use crate::html::{self, Mode, DOC_SHELL_HEAD};
use crate::links::OutlineEntry;
use crate::markdown::{self, ImgMode};

use super::{DocModel, TocEntry};

/// Reassemble the PRE-id, PRE-nav `body` (full `<!doctype…></html>` document) from the model's
/// per-page bodies — byte-identical to what [`crate::html::render_doc`] produced at distill
/// time for the requested `mode`. Pages are emitted in `n` order (the model stores them
/// sorted).
///
/// The two modes differ ONLY in framing, exactly as the per-page render does:
/// - **Page** wraps each page in `<section data-page="N" id="page-N">…</section>\n`. The stored
///   `body_html` is that section's verbatim INNER (it already carries the `\n` right after the
///   open tag and the `\n` before the close), so wrapping it reproduces the page-mode body.
/// - **Section** emits the page CONTENT bare (no page wrappers), and the per-page render adds
///   no framing newlines — so we strip the single leading/trailing `\n` `body_html` carries for
///   the page wrapper and concatenate the contents directly. `build_sections` (in `assemble`)
///   then regroups them by heading.
fn reassemble_body(model: &DocModel, mode: Mode) -> String {
    let mut out = String::with_capacity(DOC_SHELL_HEAD.len() + 4096);
    out.push_str(DOC_SHELL_HEAD);
    for p in &model.pages {
        // A page the renderer produced no body for (should not happen — every page renders a
        // section) degrades to an empty section, matching an empty source page.
        let inner = p.body_html.as_deref().unwrap_or("\n\n");
        match mode {
            Mode::Page => {
                out.push_str(&format!("<section data-page=\"{n}\" id=\"page-{n}\">", n = p.n));
                out.push_str(inner);
                out.push_str("</section>\n");
            }
            Mode::Section => {
                // Drop the page-wrapper framing newlines (one leading + one trailing) the
                // page-mode INNER carries; section mode strings the bare contents together.
                let content = inner.strip_prefix('\n').unwrap_or(inner);
                let content = content.strip_suffix('\n').unwrap_or(content);
                out.push_str(content);
            }
        }
    }
    out.push_str("</body>\n</html>\n");
    out
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
    let body = reassemble_body(model, mode);
    let outline = outline_from_model(&model.toc);
    // No deferred image URIs: the stored body already has resolved `<image N>` placeholders, so
    // `substitute_images` inside `assemble` is a no-op (no `\0idx\0` sentinels remain).
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
    let mut out = String::new();
    for p in &model.pages {
        if let Some(body) = &p.body_html {
            let no_svg = strip_svg(body);
            out.push_str(visible_text(&no_svg).trim());
        }
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

    fn model_with_pages(bodies: Vec<(u32, &str)>) -> DocModel {
        let pages = bodies
            .iter()
            .map(|(n, body)| Page {
                n: *n,
                width_pts: 612.0,
                height_pts: 792.0,
                labels: BTreeMap::new(),
                ocr_decision: None,
                active_ocr_pass: None,
                body_html: Some(body.to_string()),
            })
            .collect();
        DocModel {
            schema_version: SCHEMA_VERSION,
            source: Source { file: "x.pdf".into(), sha256: "ab".into(), pages: bodies.len() as u32, distillpdf: "0".into(), generated_at: "t".into() },
            metadata: Metadata::default(),
            pages,
            ocr_passes: Vec::new(),
            sections: Vec::new(),
            blocks: vec![Block {
                id: "b0001".into(),
                kind: BlockKind::Para,
                text: "x".into(),
                page: 1,
                section: None,
                bbox: None,
                confidence: NATIVE_CONFIDENCE,
                ocr_pass: None,
                heading_level: None,
                cells: None,
                image: None,
                label: None,
                caption: None,
            }],
            indexes: Indexes { coverage: Coverage::default(), ..Default::default() },
            assets: Vec::new(),
            links: Vec::new(),
            named_dests: Vec::new(),
            toc: Vec::new(),
        }
    }

    #[test]
    fn reassembled_body_frames_pages_exactly() {
        let m = model_with_pages(vec![(1, "\n<h1>A</h1><p>one</p>\n"), (2, "\n<p>two</p>\n")]);
        let body = reassemble_body(&m, Mode::Page);
        assert!(body.starts_with(DOC_SHELL_HEAD));
        // page wrappers + the inter-page newline framing.
        assert!(body.contains("<section data-page=\"1\" id=\"page-1\">\n<h1>A</h1><p>one</p>\n</section>\n"));
        assert!(body.contains("<section data-page=\"2\" id=\"page-2\">\n<p>two</p>\n</section>\n"));
        assert!(body.ends_with("</body>\n</html>\n"));
    }

    #[test]
    fn page_mode_mints_ids_and_nav() {
        let m = model_with_pages(vec![(1, "\n<h1>Intro</h1><p>body</p>\n")]);
        let html = render_html(&m, Mode::Page, true);
        // build_toc minted the heading id and built the nav (page-mode keeps <section data-page>).
        assert!(html.contains("<h1 id=\"sec-intro\">Intro</h1>"));
        assert!(html.contains("<nav>") && html.contains("href=\"#sec-intro\""));
    }

    #[test]
    fn section_mode_wraps_sections() {
        let m = model_with_pages(vec![(1, "\n<h1>Intro</h1><p>body</p>\n")]);
        let html = render_html(&m, Mode::Section, false);
        // section mode regroups into a <section id="sec-…"> wrapper (no page wrappers).
        assert!(html.contains("<section id=\"sec-intro\">"));
        assert!(!html.contains("data-page"));
    }

    #[test]
    fn extract_text_surfaces_all_visible_text_and_drops_svg() {
        // The visible text of the body — table cells + figure captions included (they live in
        // the body HTML, not block.text), markup stripped, SVG label text dropped — one page
        // per line.
        let m = model_with_pages(vec![(
            1,
            "\n<h1>Title</h1><table><tr><th>Cell</th></tr></table>\
             <figure><svg><text>axis</text></svg><figcaption>Cap</figcaption></figure><p>Body <b>bold</b></p>\n",
        )]);
        let txt = extract_text(&m);
        assert!(txt.contains("Title") && txt.contains("Cell") && txt.contains("Cap") && txt.contains("Body bold"));
        assert!(!txt.contains("axis"), "SVG label text is figure-internal, not page prose");
        assert!(!txt.contains('<'), "all markup stripped");
        assert!(txt.ends_with('\n'), "one page per line");
    }

    #[test]
    fn markdown_transforms_the_rendered_html() {
        let m = model_with_pages(vec![(1, "\n<h1>Intro</h1><p>hello</p>\n")]);
        let (md, files) = render_markdown(&m, Mode::Section, false, "drop").unwrap();
        assert!(md.contains("# Intro") && md.contains("hello"));
        assert!(files.is_empty());
    }
}
