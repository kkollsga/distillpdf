//! Build a [`DocModel`] from a loaded PDF.
//!
//! **The boundary principle (Wave 1):** we do NOT re-implement the analysis. distillPDF's
//! `to_html` already produces the typed element tree — reading order, the section tree
//! (heading ids), tables, figures, footnotes — serialized as a small, regular HTML
//! vocabulary. We render in PAGE mode (so every block carries its physical page from the
//! `<section data-page="N">` wrapper) and parse that element stream back into blocks. The
//! section tree is reconstructed from heading levels; section ids are exactly the renderer's
//! `sec-…` slugs, so model ids == HTML ids == CLI/agent addresses.
//!
//! Why parse the HTML rather than refactor `to_html` to emit a typed tree directly? For Wave 1
//! the HTML *is* the serialized element tree, and parsing our OWN deterministic output is far
//! less risky than surgery on the 800-line parallel render.
//!
//! **Wave 2** keeps this parse path for the block decomposition but adds the render-fidelity
//! capture that makes "renderers are pure functions of the model" hold: each page's verbatim
//! PRE-id body (`Page.body_html`, via the shared `html::render_doc` head), captured here, lets
//! a model-only re-render run the identical `html::assemble` tail. It also captures figure
//! raster bytes (sha256 + dims) under the `assets` profile.
//!
//! **Per-block bbox stays `None` — a DELIBERATE, documented Wave-2 decision, not an oversight.**
//! Blocks are decomposed from the FLATTENED page HTML, which carries no positions; threading
//! boxes would require a second, structured render alongside the fidelity render — exactly the
//! deep refactor the spec says NOT to do for bbox (it would risk destabilising the fidelity
//! pipeline for marginal gain). Positions remain available per-page via `_dbg_spans_xy`; a
//! future wave that needs pinpoint citations can thread spans through a structured render.

use lopdf::{Document, Object};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use super::container::AssetBytes;
use super::{
    derive_indexes, Asset, AssetKind, AssetProfile, AssetStorage, Block, BlockKind, DocModel, Link,
    Metadata, NamedDest, OcrDecision, Page, Regen, Section, Source, TocEntry, NATIVE_CONFIDENCE,
    SCHEMA_VERSION,
};
use crate::{frontmatter, html, links, nav, ocr};

/// Build the document model from a parsed PDF plus its raw bytes (the raw bytes back the
/// source hash and lenient stream recovery). `file` is the display name recorded in
/// `source.file` (the source PDF's basename, typically). `generated_at` is the ONE timestamp
/// in the file — taken once by the caller so the model is otherwise fully deterministic.
/// `profile` chooses which asset bytes are captured/embedded (see [`AssetProfile`]).
///
/// Returns the model plus the embedded-asset bytes map (keyed by asset id) the container
/// writer needs; it is empty under `assets="none"` or when no figure raster was recoverable.
pub(crate) fn build_model(doc: &Document, raw: &[u8], file: &str, generated_at: String, profile: AssetProfile) -> (DocModel, AssetBytes) {
    let page_map = doc.get_pages(); // BTreeMap<u32, ObjectId> — 1-indexed, sorted
    let page_count = page_map.len() as u32;

    // Per-page geometry + OCR decision + PDF page labels. Built first so blocks can be
    // attributed and the page index is complete even for pages with no extracted blocks.
    let labels = page_labels(doc, page_count);
    let mut pages: Vec<Page> = page_map
        .iter()
        .map(|(&n, &pid)| {
            let (w, h) = ocr::page_size_pts(doc, pid);
            let decision: OcrDecision = ocr::detect::decide(doc, pid, raw).into();
            let mut lmap: BTreeMap<String, String> = BTreeMap::new();
            if let Some(lbl) = labels.get(&n) {
                lmap.insert("pdf".to_string(), lbl.clone());
            }
            Page {
                n,
                width_pts: w,
                height_pts: h,
                labels: lmap,
                // Only record a decision when it's actionable (a born-digital page's
                // `NotNeeded` is the default and would just be noise on every page).
                ocr_decision: (decision != OcrDecision::NotNeeded).then_some(decision),
                active_ocr_pass: None, // Wave 2 sets this when an OCR pass feeds the page
                body_html: None,       // filled from the parsed page HTML below
            }
        })
        .collect();

    // Render the element tree to PAGE-mode HTML via the SHARED pipeline head (`render_doc`),
    // stopping BEFORE the `assemble` tail (id-minting + nav). Images are dropped to `<image N>`
    // placeholders — we only need to KNOW a figure exists, not carry megabytes of base64
    // through the parser. The resulting body is the *pre-id* page-mode body: it is what the
    // model stores per page (`Page.body_html`), so a model-only re-render runs the identical
    // `assemble` (minting the SAME `sec-…` ids and the SAME nav) — Wave 2's round-trip contract.
    let (pre_body, img_uris, _outline) = html::render_doc(doc, raw, html::Mode::Page, false);
    // Resolve the `\0idx\0` image sentinels to `<image N>` so the stored body is clean JSON
    // (no NUL bytes); this is independent of id-minting, so doing it before `build_toc` yields
    // a body identical to `to_html`'s except that headings are still bare.
    let pre_body = crate::postprocess::substitute_images(pre_body, &img_uris, false);
    // The per-page PRE-id bodies the model stores (what re-render reassembles + `assemble`s).
    let (_, _, pre_bodies) = parse_page_html(&pre_body);
    // Mint heading ids exactly as `to_html(Mode::Page, …)` does, then parse THAT for blocks +
    // sections (their `sec-…` ids are the authoritative id space) — `build_toc` is independent
    // of image substitution, so this equals the Wave-1 `to_html(Page,false,false)` body.
    let page_html = nav::build_toc(pre_body, false);
    let (mut blocks, sections, _) = parse_page_html(&page_html);
    // Fold each page's verbatim PRE-id body HTML (the `<section>` inner content) onto its
    // geometry page, so a model-only re-render reproduces the page-mode body byte-for-byte.
    for p in pages.iter_mut() {
        p.body_html = pre_bodies.get(&p.n).cloned();
    }

    // Front-matter / metadata: reuse the dedicated extractor (the same one the public
    // `metadata()` method exposes) rather than re-deriving from the HTML <header>.
    let fm = frontmatter::extract_front_matter(doc, raw);
    let metadata = Metadata {
        title: (!fm.title.trim().is_empty()).then(|| fm.title.clone()),
        authors: fm.authors.iter().map(|a| a.name.clone()).collect(),
        language: None, // not yet detected on the born-digital path; Wave 2+
        abstract_text: fm.abstract_text.clone(),
        keywords: fm.keywords.clone(),
    };

    // Links, named destinations, TOC — straight from the existing extractors.
    let links: Vec<Link> = links::extract_links(doc)
        .into_iter()
        .map(|l| Link { page: l.page, uri: l.uri, dest_page: l.dest_page, dest_name: l.dest_name })
        .collect();
    let named_dests: Vec<NamedDest> = links::named_destinations(doc)
        .into_iter()
        .map(|d| NamedDest { name: d.name, page: d.page })
        .collect();
    // Prefer the PDF's own outline; fall back to the section tree (same precedence as the
    // rendered <nav>). The fallback is derived from `sections` — the heading tree we already
    // reconstructed — so anchors are exactly the section ids.
    let toc = build_toc(doc, &sections);

    // Assets: one per figure block that carried an image. Under `assets="figures"`/`"full"`
    // we capture the figure's actual bytes (re-rendering inline once), fill sha256 + width +
    // height, and embed them; under `"none"` (or when a figure's graphic is vector-only / not
    // recoverable as a raster) the bytes are dropped and only the regen STUB remains — a named,
    // reversible hole. The asset table is always complete (every figure image id has an entry).
    let (assets, asset_bytes) = build_assets(doc, raw, &mut blocks, profile);

    let source = Source {
        file: file.to_string(),
        sha256: sha256_hex(raw),
        pages: page_count,
        distillpdf: env!("CARGO_PKG_VERSION").to_string(),
        generated_at,
    };

    let indexes = derive_indexes(&blocks);

    let model = DocModel {
        schema_version: SCHEMA_VERSION,
        source,
        metadata,
        pages,
        ocr_passes: Vec::new(), // born-digital: no OCR; Wave 2 populates
        sections,
        blocks,
        indexes,
        assets,
        links,
        named_dests,
        toc,
    };
    (model, asset_bytes)
}

/// Read the PDF `/PageLabels` number tree into a `{1-based page: label}` map (e.g. roman
/// front-matter `i, ii, …` then arabic `1, 2, …`). Returns an empty map when the PDF carries
/// no `/PageLabels` — the common case for born-digital documents. A minimal implementation:
/// it handles the standard label dictionary (`/S` style `D`/`r`/`R`/`a`/`A`, `/P` prefix,
/// `/St` start) over the `/Nums` array; ranges it can't interpret are simply left unlabeled
/// (a missing label is an honest absence, not a wrong one).
fn page_labels(doc: &Document, page_count: u32) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    let Ok(catalog) = doc.catalog() else { return out };
    let Ok(pl) = catalog.get(b"PageLabels") else { return out };
    let pl = match resolve(doc, pl) {
        Some(Object::Dictionary(d)) => d,
        _ => return out,
    };
    let nums = match pl.get(b"Nums").ok().and_then(|o| resolve(doc, o)) {
        Some(Object::Array(a)) => a,
        _ => return out,
    };
    // /Nums is a flat array of alternating [start_index, label_dict, start_index, …].
    // Collect (start_page0, dict) pairs, sorted by start.
    let mut ranges: Vec<(i64, lopdf::Dictionary)> = Vec::new();
    let mut i = 0;
    while i + 1 < nums.len() {
        let start = match &nums[i] {
            Object::Integer(n) => *n,
            _ => {
                i += 2;
                continue;
            }
        };
        if let Some(Object::Dictionary(d)) = resolve(doc, &nums[i + 1]) {
            ranges.push((start, d.clone()));
        }
        i += 2;
    }
    ranges.sort_by_key(|(s, _)| *s);
    for (ri, (start0, d)) in ranges.iter().enumerate() {
        let end0 = ranges.get(ri + 1).map(|(s, _)| *s).unwrap_or(page_count as i64);
        let style = match d.get(b"S").ok().and_then(|o| resolve(doc, o)) {
            Some(Object::Name(n)) => String::from_utf8_lossy(&n).to_string(),
            _ => String::new(),
        };
        let prefix = match d.get(b"P").ok().and_then(|o| resolve(doc, o)) {
            Some(Object::String(s, _)) => String::from_utf8_lossy(&s).to_string(),
            _ => String::new(),
        };
        let st = match d.get(b"St").ok().and_then(|o| resolve(doc, o)) {
            Some(Object::Integer(n)) => n,
            _ => 1,
        };
        for p0 in *start0..end0 {
            if p0 < 0 {
                continue;
            }
            let value = st + (p0 - start0); // the running counter within this range
            let body = match style.as_str() {
                "D" => value.to_string(),
                "r" => roman(value as u32, false),
                "R" => roman(value as u32, true),
                "a" => alpha(value as u32, false),
                "A" => alpha(value as u32, true),
                _ => String::new(), // style-less range → prefix only (or nothing)
            };
            let label = format!("{prefix}{body}");
            if !label.is_empty() {
                out.insert((p0 + 1) as u32, label); // /Nums is 0-based, pages are 1-based
            }
        }
    }
    out
}

/// Resolve an object through a single indirection (the depth `/PageLabels` needs).
fn resolve<'a>(doc: &'a Document, o: &'a Object) -> Option<Object> {
    match o {
        Object::Reference(id) => doc.get_object(*id).ok().cloned(),
        other => Some(other.clone()),
    }
}

/// Lowercase/uppercase roman numeral for a PDF page label (1 → i/I, 4 → iv/IV, …).
fn roman(mut n: u32, upper: bool) -> String {
    if n == 0 {
        return String::new();
    }
    const VALS: &[(u32, &str)] = &[
        (1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"),
        (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i"),
    ];
    let mut s = String::new();
    for &(v, sym) in VALS {
        while n >= v {
            s.push_str(sym);
            n -= v;
        }
    }
    if upper {
        s.to_uppercase()
    } else {
        s
    }
}

/// Alphabetic page label (1 → a, 26 → z, 27 → aa, …), lower/upper.
fn alpha(n: u32, upper: bool) -> String {
    if n == 0 {
        return String::new();
    }
    // PDF spec: 27 → "aa" (the letter repeats, it's not base-26 positional).
    let idx = ((n - 1) % 26) as u8;
    let reps = ((n - 1) / 26 + 1) as usize;
    let c = (if upper { b'A' } else { b'a' } + idx) as char;
    (0..reps).map(|_| c).collect()
}

/// SHA-256 of the raw PDF bytes, lowercase hex — the content-address that binds the model to
/// its source artifact.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Build the model TOC: the PDF's own outline when present (the author's clean TOC, with
/// `page-N` anchors), else the heading tree derived from `sections` (with `sec-…` anchors
/// that resolve into both the HTML and the model's section ids).
fn build_toc(doc: &Document, sections: &[Section]) -> Vec<TocEntry> {
    let outline = links::outline(doc);
    if !outline.is_empty() {
        return outline
            .into_iter()
            .map(|e| TocEntry {
                level: e.level + 1, // OutlineEntry.level is 0-based; the model/HTML use 1-based
                title: e.title,
                page: e.page,
                anchor: if e.page > 0 { format!("page-{}", e.page) } else { String::new() },
            })
            .collect();
    }
    sections
        .iter()
        .map(|s| TocEntry {
            level: s.level,
            title: s.title.clone(),
            page: s.page_start,
            anchor: s.id.clone(),
        })
        .collect()
}

/// Build the asset table + the embedded-bytes map for the figure blocks.
///
/// One asset per figure block that carried an image (id `img/fig_{N}.{ext}`). Under a profile
/// that keeps figures, we re-render the pages with images INLINE once and pull each figure's
/// actual raster bytes out of its `<figure id="fig-N">` data URI, then fill the verifying hash
/// and pixel dimensions and embed the bytes. A figure with no recoverable raster (a pure
/// vector/SVG figure, or one whose graphic the inline render didn't materialise) keeps a
/// DROPPED stub with a `regen` recipe — a named, reversible hole, never silent. Under
/// `assets="none"` every figure is a dropped stub.
fn build_assets(doc: &Document, raw: &[u8], blocks: &mut [Block], profile: AssetProfile) -> (Vec<Asset>, AssetBytes) {
    let mut assets = Vec::new();
    let mut bytes_map = AssetBytes::new();
    // The figure-id → raster bytes map, built only when the profile keeps figures (re-rendering
    // inline is the cost we pay exactly once, and only when bytes are wanted).
    let rasters = if profile.keeps_figures() { figure_rasters(doc, raw) } else { BTreeMap::new() };

    for b in blocks.iter_mut() {
        let Some(id) = b.image.clone() else { continue };
        // The figure number is the `N` in `img/fig_{N}.png` (== the HTML `fig-N`).
        let fig_n = id.strip_prefix("img/fig_").and_then(|s| s.split('.').next()).unwrap_or("").to_string();
        match rasters.get(&fig_n) {
            Some((data, ext, w, h)) => {
                // Re-key the asset id to the real extension (a JPEG figure stays `.jpg`) and
                // re-point the block at it so `block.image` always names a real asset entry.
                let aid = format!("img/fig_{fig_n}.{ext}");
                b.image = Some(aid.clone());
                bytes_map.insert(aid.clone(), data.clone());
                assets.push(Asset {
                    id: aid,
                    kind: AssetKind::Figure,
                    storage: AssetStorage::Embedded,
                    sha256: Some(sha256_hex(data)),
                    bytes: Some(data.len() as u64),
                    width: *w,
                    height: *h,
                    regen: Some(Regen { page: b.page, dpi: None }),
                });
            }
            None => assets.push(Asset {
                id: id.clone(),
                kind: AssetKind::Figure,
                storage: AssetStorage::Dropped,
                sha256: None,
                bytes: None,
                width: None,
                height: None,
                regen: Some(Regen { page: b.page, dpi: None }),
            }),
        }
    }
    (assets, bytes_map)
}

/// A captured figure raster: its bytes, file extension, and decoded pixel dimensions.
type FigureRaster = (Vec<u8>, String, Option<u32>, Option<u32>);

/// Re-render the document with images INLINE (once) and decode each figure's raster into
/// `figure_number → `[`FigureRaster`]. Vector-only figures yield no entry (their graphic is
/// `<svg>`, not a raster). Width/height come from decoding the image header.
fn figure_rasters(doc: &Document, raw: &[u8]) -> BTreeMap<String, FigureRaster> {
    let mut out = BTreeMap::new();
    let html = html::to_html(doc, raw, html::Mode::Page, true, false);
    // Walk `<figure id="fig-N"> … <img src="data:…"> … </figure>` occurrences. We only need the
    // FIRST raster `<img>` inside each figure (a composite figure's base raster).
    let mut rest = html.as_str();
    while let Some(fpos) = rest.find("<figure id=\"fig-") {
        let after = &rest[fpos + "<figure id=\"fig-".len()..];
        let Some(qpos) = after.find('"') else { break };
        let fig_n = after[..qpos].to_string();
        // Bound the search to this figure's element.
        let fig_end = after.find("</figure>").map(|e| e + "</figure>".len()).unwrap_or(after.len());
        let fig_html = &after[..fig_end];
        if let Some((data, ext)) = first_img_data_uri(fig_html) {
            let (w, h) = image_dims(&data);
            out.entry(fig_n).or_insert((data, ext, w, h));
        }
        rest = &after[fig_end..];
    }
    out
}

/// Decode the first `<img src="data:image/…;base64,…">` inside a fragment into `(bytes, ext)`.
fn first_img_data_uri(html: &str) -> Option<(Vec<u8>, String)> {
    let at = html.find("src=\"data:")?;
    let start = at + "src=\"".len();
    let end = html[start..].find('"')? + start;
    decode_data_uri(&html[start..end])
}

/// Decode a `data:image/<fmt>;base64,…` URI into raw bytes + a file extension. (A small,
/// dependency-light mirror of `markdown::decode_data_uri`, scoped to the figure-capture path.)
fn decode_data_uri(uri: &str) -> Option<(Vec<u8>, String)> {
    use base64::Engine;
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.contains("base64") {
        return None;
    }
    let ext = match meta.split(';').next().unwrap_or("") {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    };
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    Some((bytes, ext.to_string()))
}

/// Pixel dimensions of an encoded image, via the `image` crate's cheap header probe. `None` if
/// the format can't be sniffed (the asset still embeds with a hash; dims are an honest absence).
fn image_dims(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    match image::load_from_memory(bytes) {
        Ok(img) => {
            use image::GenericImageView;
            let (w, h) = img.dimensions();
            (Some(w), Some(h))
        }
        Err(_) => (None, None),
    }
}

// ---- the page-mode HTML element-stream parser ------------------------------

/// Parse page-mode HTML into `(blocks, sections, page_bodies)`.
///
/// The page-mode body is a flat sequence of `<section data-page="N" id="page-N">…</section>`
/// wrappers, each containing the page's blocks: `<h1..6 id="sec-…">`, `<p>`, `<table>`,
/// `<figure>`, `<aside>` (footnotes), `<div id="tab-…">` (standalone table captions). We walk
/// the byte stream tag-by-tag (NOT a general HTML parser — this is OUR known, regular output)
/// and emit one [`Block`] per element in document/reading order, attributing each to its
/// current page and the most recent open section at-or-above its level.
///
/// `page_bodies` maps each page number to the VERBATIM inner HTML of its `<section>` wrapper —
/// the render-fidelity capture that lets a model-only re-render reproduce this exact page-mode
/// body (it carries inline markup, list grouping, table structure, headers, dest anchors, and
/// SVG, none of which survive block decomposition). Block decomposition and the body capture
/// are two views of the same single parse pass.
fn parse_page_html(html: &str) -> (Vec<Block>, Vec<Section>, BTreeMap<u32, String>) {
    let body = html.split_once("<body>").map(|x| x.1).unwrap_or(html);
    let body = body.split_once("</body>").map(|x| x.0).unwrap_or(body);

    let mut blocks: Vec<Block> = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut page_bodies: BTreeMap<u32, String> = BTreeMap::new();
    // The open-section stack: (id, level). A heading at level L pops everything >= L, then
    // pushes itself, so each block's section is the stack top.
    let mut stack: Vec<(String, u8)> = Vec::new();
    let mut cur_page: u32 = 0;
    // Byte offset (into `body`) where the current page's `<section>` inner content begins
    // (just past the open tag's `>`); `None` between pages.
    let mut sec_inner_start: Option<usize> = None;
    let mut ord: usize = 0;
    let next_id = |ord: &mut usize| {
        *ord += 1;
        format!("b{:04}", *ord)
    };

    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Close the current page's body capture at its `</section>` before any other dispatch
        // (the close tag is not a block-opening element).
        if body[i..].starts_with("</section>") {
            if let (Some(start), true) = (sec_inner_start, cur_page > 0) {
                page_bodies.insert(cur_page, body[start..i].to_string());
            }
            sec_inner_start = None;
            i += "</section>".len();
            continue;
        }
        // A `<section data-page="N">` wrapper sets the current page; an inner element starts
        // a block. We dispatch on the tag name.
        let Some(close_rel) = body[i..].find('>') else { break };
        let tag = &body[i + 1..i + close_rel]; // contents between < and >
        let name = tag_name(tag);

        match name {
            "section" => {
                if let Some(p) = attr(tag, "data-page") {
                    cur_page = p.parse().unwrap_or(cur_page);
                }
                i += close_rel + 1;
                sec_inner_start = Some(i); // inner content begins just past the open tag
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name.as_bytes()[1] - b'0';
                let id = attr(tag, "id").unwrap_or_default();
                let (inner, end) = element_inner(body, i, name);
                let title = nav::strip_inline(inner).trim().to_string();
                // Maintain the section stack.
                while stack.last().is_some_and(|(_, l)| *l >= level) {
                    stack.pop();
                }
                let parent = stack.last().map(|(pid, _)| pid.clone());
                if !id.is_empty() {
                    let sec_page = cur_page;
                    // Provisional page_end = page_start; widened as later blocks land.
                    sections.push(Section {
                        id: id.clone(),
                        level,
                        title: title.clone(),
                        parent,
                        page_start: sec_page,
                        page_end: sec_page,
                    });
                    stack.push((id.clone(), level));
                }
                let section = stack.last().map(|(sid, _)| sid.clone());
                blocks.push(Block {
                    id: next_id(&mut ord),
                    kind: BlockKind::Heading,
                    text: title,
                    page: cur_page,
                    section,
                    bbox: None,
                    confidence: NATIVE_CONFIDENCE,
                    ocr_pass: None,
                    heading_level: Some(level),
                    cells: None,
                    image: None,
                    label: None,
                    caption: None,
                });
                i = end;
            }
            "p" => {
                let (inner, end) = element_inner(body, i, "p");
                let text = nav::strip_inline(inner).trim().to_string();
                if !text.is_empty() {
                    // A `<p>` whose text begins a list marker is a list item; otherwise prose.
                    let kind = if html::list_kind(&text).is_some() {
                        BlockKind::ListItem
                    } else {
                        BlockKind::Para
                    };
                    blocks.push(text_block(next_id(&mut ord), kind, text, cur_page, &stack));
                }
                i = end;
            }
            "li" => {
                let (inner, end) = element_inner(body, i, "li");
                let text = nav::strip_inline(inner).trim().to_string();
                if !text.is_empty() {
                    blocks.push(text_block(next_id(&mut ord), BlockKind::ListItem, text, cur_page, &stack));
                }
                i = end;
            }
            "aside" => {
                // Footnote block: an <aside> wrapping one <p> per footnote. Emit each as a
                // footnote block (preserving the per-note granularity the renderer gives).
                let (inner, end) = element_inner(body, i, "aside");
                for note in split_top_level(inner, "p") {
                    let text = nav::strip_inline(note).trim().to_string();
                    if !text.is_empty() {
                        blocks.push(text_block(next_id(&mut ord), BlockKind::Footnote, text, cur_page, &stack));
                    }
                }
                i = end;
            }
            "table" => {
                let (inner, end) = element_inner(body, i, "table");
                let (cells, caption) = parse_table(inner);
                let mut b = text_block(next_id(&mut ord), BlockKind::Table, String::new(), cur_page, &stack);
                b.cells = Some(cells);
                b.caption = caption;
                b.label = b.caption.as_deref().and_then(caption_label);
                blocks.push(b);
                i = end;
            }
            "figure" => {
                let (inner, end) = element_inner(body, i, "figure");
                let fig_id = attr(tag, "id").and_then(|s| s.strip_prefix("fig-").map(String::from));
                let caption = element_text(inner, "figcaption");
                // The renderer drops images to `<image N>` (placeholder mode); a figure that
                // had a raster carries that marker. Mint a stable asset id keyed on the
                // figure id so the asset table and the block agree.
                let has_image = inner.contains("<image ") || inner.contains("<img ") || inner.contains("<svg");
                let image = (has_image)
                    .then(|| fig_id.as_ref().map(|n| format!("img/fig_{n}.png")))
                    .flatten();
                let mut b = text_block(next_id(&mut ord), BlockKind::Figure, String::new(), cur_page, &stack);
                b.caption = caption.clone();
                b.label = caption.as_deref().and_then(caption_label);
                b.image = image;
                blocks.push(b);
                i = end;
            }
            "div" => {
                // A standalone table/figure caption `<div id="tab-…">…</div>` (no detected
                // table nearby). Emit as a caption block so its label/anchor survive.
                if attr(tag, "id").is_some_and(|s| s.starts_with("tab-") || s.starts_with("fig-")) {
                    let (inner, end) = element_inner(body, i, "div");
                    let text = nav::strip_inline(inner).trim().to_string();
                    let mut b = text_block(next_id(&mut ord), BlockKind::Caption, text.clone(), cur_page, &stack);
                    b.label = caption_label(&text);
                    blocks.push(b);
                    i = end;
                } else {
                    i += close_rel + 1;
                }
            }
            _ => {
                // Any other tag (header, nav, ul/ol wrappers, inline) — skip the open tag and
                // let the walk descend into its children (lists, headers) so their <p>/<li>
                // are still captured.
                i += close_rel + 1;
            }
        }
    }

    // A final unclosed page (no trailing `</section>` — shouldn't happen for our output, but
    // capture defensively so the body is never silently lost).
    if let (Some(start), true) = (sec_inner_start, cur_page > 0) {
        page_bodies.entry(cur_page).or_insert_with(|| body[start..].to_string());
    }

    // Widen each section's page_end to the max page of any block attributed to it (or any of
    // its descendants — a parent spans all its children's pages).
    finalize_section_ranges(&mut sections, &blocks);
    (blocks, sections, page_bodies)
}

/// Construct a text-bearing block attributing it to the current open section.
fn text_block(id: String, kind: BlockKind, text: String, page: u32, stack: &[(String, u8)]) -> Block {
    Block {
        id,
        kind,
        text,
        page,
        section: stack.last().map(|(sid, _)| sid.clone()),
        bbox: None,
        confidence: NATIVE_CONFIDENCE,
        ocr_pass: None,
        heading_level: None,
        cells: None,
        image: None,
        label: None,
        caption: None,
    }
}

/// Widen section page ranges to cover every attributed block, then propagate child ranges up
/// to parents (a section spans all its subsections).
fn finalize_section_ranges(sections: &mut [Section], blocks: &[Block]) {
    use std::collections::HashMap;
    let mut span: HashMap<String, (u32, u32)> = HashMap::new();
    for b in blocks {
        if let Some(sid) = &b.section {
            let e = span.entry(sid.clone()).or_insert((b.page, b.page));
            e.0 = e.0.min(b.page);
            e.1 = e.1.max(b.page);
        }
    }
    // Index sections by id for parent propagation.
    let parents: Vec<(String, Option<String>)> = sections.iter().map(|s| (s.id.clone(), s.parent.clone())).collect();
    // Bubble each section's own span up the parent chain.
    for (id, parent) in &parents {
        if let Some(&(lo, hi)) = span.get(id) {
            let mut cur = Some(parent.clone());
            // include self
            let e = span.entry(id.clone()).or_insert((lo, hi));
            e.0 = e.0.min(lo);
            e.1 = e.1.max(hi);
            while let Some(Some(pid)) = cur {
                let e = span.entry(pid.clone()).or_insert((lo, hi));
                e.0 = e.0.min(lo);
                e.1 = e.1.max(hi);
                cur = Some(parents.iter().find(|(i, _)| *i == pid).and_then(|(_, p)| p.clone()));
            }
        }
    }
    for s in sections.iter_mut() {
        if let Some(&(lo, hi)) = span.get(&s.id) {
            s.page_start = if s.page_start == 0 { lo } else { s.page_start.min(lo) };
            s.page_end = s.page_end.max(hi);
        }
    }
}

// ---- small HTML helpers (scoped to distillPDF's known output) --------------

/// The tag name from a tag's inner text (between `<` and `>`), lowercased view of the leading
/// name token. e.g. `section data-page="1"` → `section`, `/p` → `/p`.
fn tag_name(tag: &str) -> &str {
    let t = tag.trim_start();
    let end = t.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(t.len());
    // A self-closing or close tag begins with '/'; keep it for callers that check, but the
    // dispatcher only matches open names, so a leading '/' simply won't match.
    &t[..end]
}

/// The value of `key="…"` in a tag's inner text, if present.
fn attr(tag: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let s = tag.find(&pat)? + pat.len();
    let e = tag[s..].find('"')?;
    Some(tag[s..s + e].to_string())
}

/// Given the byte offset of an element's open tag `<name …>` in `html`, return
/// `(inner_html, byte_offset_just_past_the_matching_close)`. Handles nesting of the SAME tag
/// name (e.g. nested `<section>`s never occur in page-mode body, but `<div>`/`<p>` safety).
fn element_inner<'a>(html: &'a str, open: usize, name: &str) -> (&'a str, usize) {
    // Scan over BYTES (tag delimiters and the ASCII tag names are single-byte, so byte
    // matching is correct), advancing one byte at a time. We only ever slice `html` at the
    // byte offsets where a `<…>` boundary starts — all char boundaries — so multibyte body
    // text (em dashes, accents) never trips a char-boundary panic.
    let bytes = html.as_bytes();
    // Move past the open tag.
    let after_open = match html[open..].find('>') {
        Some(r) => open + r + 1,
        None => return ("", html.len()),
    };
    let open_pat = format!("<{name}").into_bytes();
    let close_pat = format!("</{name}>").into_bytes();
    let mut depth = 1i32;
    let mut i = after_open;
    while i < bytes.len() {
        if bytes[i..].starts_with(&close_pat) {
            depth -= 1;
            if depth == 0 {
                return (&html[after_open..i], i + close_pat.len());
            }
            i += close_pat.len();
        } else if bytes[i..].starts_with(&open_pat) {
            // Only count as nesting if the next byte ends the tag name (`<p>` vs `<pre>`).
            let nb = bytes.get(i + open_pat.len());
            if matches!(nb, Some(b) if *b == b'>' || *b == b' ' || *b == b'/') {
                depth += 1;
                i += open_pat.len();
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    (&html[after_open..], html.len())
}

/// Inner text of the first `<tag>…</tag>` inside `html`, stripped of inline markup. Used for
/// `<figcaption>` extraction. Returns `None` when the tag is absent.
fn element_text(html: &str, tag: &str) -> Option<String> {
    let open = html.find(&format!("<{tag}"))?;
    let (inner, _) = element_inner(html, open, tag);
    let t = nav::strip_inline(inner).trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Split `html` into the inner texts of each TOP-LEVEL `<tag>…</tag>` (non-nested). Used to
/// pull each `<p>` out of a footnote `<aside>`.
fn split_top_level<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let open_pat = format!("<{tag}");
    let mut i = 0;
    // `str::find` returns a char-boundary offset, and `open_pat`/the next byte are ASCII, so
    // the indices used to slice here are always valid boundaries.
    while let Some(rel) = html[i..].find(&open_pat) {
        let open = i + rel;
        // Ensure it's the tag, not a prefix (`<p` vs `<pre`).
        let nb = html.as_bytes().get(open + open_pat.len());
        if !matches!(nb, Some(b) if *b == b'>' || *b == b' ' || *b == b'/') {
            i = open + open_pat.len();
            continue;
        }
        let (inner, end) = element_inner(html, open, tag);
        out.push(inner);
        i = end;
    }
    out
}

/// Parse a `<table>`'s inner HTML into a row-major cell grid plus an optional caption
/// (`<caption>`). Each `<tr>` is a row of `<th>`/`<td>` cell texts.
fn parse_table(inner: &str) -> (Vec<Vec<String>>, Option<String>) {
    let caption = element_text(inner, "caption");
    let mut rows = Vec::new();
    for tr in split_top_level(inner, "tr") {
        let mut row = Vec::new();
        // Cells are <th> or <td>, never nested — a simple byte scan suffices (byte-indexed so
        // multibyte cell text doesn't trip a char-boundary slice).
        let mut j = 0;
        let tb = tr.as_bytes();
        while j < tb.len() {
            let is_th = tb[j..].starts_with(b"<th");
            let is_td = tb[j..].starts_with(b"<td");
            if is_th || is_td {
                let cell_tag = if is_th { "th" } else { "td" };
                let (cinner, end) = element_inner(tr, j, cell_tag);
                row.push(nav::strip_inline(cinner).trim().to_string());
                j = end;
            } else {
                j += 1;
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    (rows, caption)
}

/// Parse a leading element label ("Table 3", "Figure 1") from a caption string, if present.
/// Mirrors the renderer's caption convention ("Table N: …" / "Figure N. …").
fn caption_label(caption: &str) -> Option<String> {
    let t = caption.trim();
    let lower = t.to_lowercase();
    for kw in ["table", "figure", "fig.", "fig"] {
        if lower.starts_with(kw) {
            // Take "<Kw> <number>" up to the first separator (':', '.', whitespace run).
            let rest = t[kw.len()..].trim_start();
            let num: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '.').collect();
            let num = num.trim_end_matches('.');
            if !num.is_empty() {
                // Normalize the keyword's display casing to the source's leading word.
                let kw_disp = &t[..kw.len()];
                return Some(format!("{kw_disp} {num}"));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_blocks_and_pages() {
        let html = "<!doctype html><html><body>\n\
            <section data-page=\"1\" id=\"page-1\">\n\
            <h1 id=\"sec-a\">Title A</h1><p>Intro para.</p>\
            <h2 id=\"sec-b\">Sub B</h2><p>Body of B.</p></section>\n\
            <section data-page=\"2\" id=\"page-2\">\n\
            <p>More B on page 2.</p></section>\n\
            </body></html>";
        let (blocks, sections, _) = parse_page_html(html);
        // 5 blocks: h1, p, h2, p, p.
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0].kind, BlockKind::Heading);
        assert_eq!(blocks[0].text, "Title A");
        assert_eq!(blocks[0].page, 1);
        assert_eq!(blocks[1].kind, BlockKind::Para);
        assert_eq!(blocks[1].section.as_deref(), Some("sec-a"));
        // sec-b is nested under sec-a; its blocks attribute to sec-b.
        assert_eq!(blocks[3].section.as_deref(), Some("sec-b"));
        // the page-2 block stays in sec-b (no new heading), page tracked.
        assert_eq!(blocks[4].page, 2);
        assert_eq!(blocks[4].section.as_deref(), Some("sec-b"));
        // sections: sec-a (parent None), sec-b (parent sec-a, spans pages 1..2).
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].id, "sec-b");
        assert_eq!(sections[1].parent.as_deref(), Some("sec-a"));
        assert_eq!(sections[1].page_start, 1);
        assert_eq!(sections[1].page_end, 2);
        // sec-a spans both its own + child pages.
        assert_eq!(sections[0].page_end, 2);
    }

    #[test]
    fn parses_table_and_figure() {
        let html = "<body><section data-page=\"1\" id=\"page-1\">\
            <table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>\
            <figure id=\"fig-3\"><image 1><figcaption>Figure 3: A chart.</figcaption></figure>\
            </section></body>";
        let (blocks, _, _) = parse_page_html(html);
        let table = blocks.iter().find(|b| b.kind == BlockKind::Table).unwrap();
        assert_eq!(table.cells.as_ref().unwrap(), &vec![vec!["A".to_string(), "B".into()], vec!["1".into(), "2".into()]]);
        let fig = blocks.iter().find(|b| b.kind == BlockKind::Figure).unwrap();
        assert_eq!(fig.caption.as_deref(), Some("Figure 3: A chart."));
        assert_eq!(fig.label.as_deref(), Some("Figure 3"));
        assert_eq!(fig.image.as_deref(), Some("img/fig_3.png"));
    }

    #[test]
    fn parses_footnote_aside() {
        let html = "<body><section data-page=\"1\" id=\"page-1\">\
            <aside><p>1. First note.</p><p>2. Second note.</p></aside></section></body>";
        let (blocks, _, _) = parse_page_html(html);
        let notes: Vec<_> = blocks.iter().filter(|b| b.kind == BlockKind::Footnote).collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].text, "1. First note.");
    }

    #[test]
    fn caption_label_parsing() {
        assert_eq!(caption_label("Table 3: Results"), Some("Table 3".to_string()));
        assert_eq!(caption_label("Figure 1. A plot"), Some("Figure 1".to_string()));
        assert_eq!(caption_label("Figure 5.6: deep"), Some("Figure 5.6".to_string()));
        assert_eq!(caption_label("just prose"), None);
    }

    #[test]
    fn roman_and_alpha_labels() {
        assert_eq!(roman(1, false), "i");
        assert_eq!(roman(4, true), "IV");
        assert_eq!(roman(12, false), "xii");
        assert_eq!(alpha(1, false), "a");
        assert_eq!(alpha(27, false), "aa");
        assert_eq!(alpha(2, true), "B");
    }
}