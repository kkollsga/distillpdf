//! Build a [`DocModel`] from a loaded PDF.
//!
//! **The single-stream principle.** distillPDF's render walk ([`html::render_doc_elements`])
//! materializes one post-transform element IR — reading order, the section tree, tables,
//! figures, footnotes, page chrome, each with its bbox. We PROJECT that IR directly into
//! [`Block`]s here (no HTML round-trip): one block per emitted construct, in reading order, each
//! carrying its physical page, its bbox, and (for the query-lossy parts) the fidelity fields the
//! renderer needs to rebuild the byte-identical IR from blocks alone ([`crate::model::render`]).
//! The section tree comes from the heading levels; section ids are minted to match the
//! renderer's `build_toc` exactly (via the shared [`nav::mint_section_id`]), so model ids == HTML
//! ids == CLI/agent addresses. Figure/table ids adopt their post-`dedup_ids` form — a block
//! addresses the id the final HTML actually uses.
//!
//! **bbox is threaded from the walk** (PDF user space `[x0,y0,x1,y1]`) onto every block the IR
//! gives a position for (100% of content blocks on the born-digital path). The figure raster
//! bytes (sha256 + dims) are still captured under the `assets` profile.

use lopdf::{Document, Object};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

use super::container::AssetBytes;
use super::{
    derive_indexes, Asset, AssetKind, AssetProfile, AssetStorage, Block, BlockKind, DocModel, Link,
    Metadata, NamedDest, OcrDecision, Page, Regen, Section, Source, TocEntry, NATIVE_CONFIDENCE,
    SCHEMA_VERSION,
};
use crate::html::{Bbox, ElKind, PageIR};
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
    let pages: Vec<Page> = page_map
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
            }
        })
        .collect();

    // The single-stream analysis: materialize the post-transform element IR (page mode, images
    // dropped to `<image N>` placeholders) ONCE, then project blocks + sections directly from it
    // — no HTML round-trip, no stored fidelity body. The blocks ARE the render source of truth
    // (`render::render_html` rebuilds the IR from them byte-identically), so the page's content
    // is held once, in `blocks`.
    let (pages_ir, _outline) = html::render_doc_elements(doc, raw, html::Mode::Page, false);
    // The post-dedup id map (`fig-3` → `fig-3-2` when ids collide) and the `sec-…` minting both
    // key off the deduped body's id namespace, exactly as the renderer's `build_toc` does —
    // computed directly from the IR's element fragments in document order (no full-body emit).
    let dedup_map = post_dedup_id_map(&pages_ir);
    let sec_ids = mint_section_ids(&pages_ir, &dedup_map);
    let (mut blocks, sections) = project_blocks(&pages_ir, &dedup_map, &sec_ids);

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

// ---- the element-IR block projection ---------------------------------------

/// Map every id-bearing element (figure / table-caption / dest-anchor) to the id it carries in
/// the FINAL deduped HTML. `dedup_ids` renumbers a colliding `id="fig-3"` to `fig-3-2` in the
/// body; a block/asset must address the id the rendered HTML actually uses (addressability beats
/// purity), so we replay the same counting dedup over the emitted ids IN DOCUMENT ORDER and
/// return, for each occurrence index of a base id, its post-dedup form. Keyed `(base_id ->
/// Vec<deduped_id>)` in occurrence order; the projection consumes them in the same walk order.
fn post_dedup_id_map(pages_ir: &[PageIR]) -> BTreeMap<String, Vec<String>> {
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut bump = |raw: &str, out: &mut BTreeMap<String, Vec<String>>| {
        let n = seen.entry(raw.to_string()).or_insert(0);
        *n += 1;
        let deduped = if *n == 1 { raw.to_string() } else { format!("{raw}-{n}") };
        out.entry(raw.to_string()).or_default().push(deduped);
    };
    for (_pno, els, _uris) in pages_ir {
        for e in els {
            for raw in ids_in_fragment(&e.html()) {
                bump(&raw, &mut out);
            }
        }
    }
    out
}

/// Every `id="…"` literal in an element's emitted HTML fragment, in order — the exact ids
/// `dedup_ids` would see (figure/table ids AND any SVG-internal `id`s the fragment carries).
fn ids_in_fragment(frag: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = frag;
    while let Some(p) = rest.find("id=\"") {
        rest = &rest[p + 4..];
        if let Some(e) = rest.find('"') {
            ids.push(rest[..e].to_string());
            rest = &rest[e + 1..];
        } else {
            break;
        }
    }
    ids
}

/// Mint the `sec-…` id for every heading in the IR, EXACTLY as `nav::build_toc` does on the
/// rendered body: seed the uniqueness set from the deduped body's existing ids (figure/table/
/// dest/SVG ids), skip the front-matter `<header>`'s `<h1>` (it gets no `sec-` id), then mint
/// in document order via the shared [`nav::mint_section_id`]. Returns one `Option<String>` per
/// [`ElKind::Heading`] in walk order (`None` for an empty-label heading, which build_toc skips).
fn mint_section_ids(pages_ir: &[PageIR], dedup_map: &BTreeMap<String, Vec<String>>) -> Vec<Option<String>> {
    // The id namespace already in the deduped body (everything `build_toc` seeds `seen` with).
    let mut seen: HashSet<String> = HashSet::new();
    for deduped in dedup_map.values().flatten() {
        seen.insert(deduped.clone());
    }
    let mut out = Vec::new();
    for (_pno, els, _uris) in pages_ir {
        for e in els {
            // The front-matter <header> carries the title <h1> as opaque HTML, not a `Heading`
            // element — so it never reaches this walk, matching build_toc's header skip.
            if let ElKind::Heading { text, .. } = &e.kind {
                let label = nav::strip_inline(text);
                let label = label.trim();
                out.push((!label.is_empty()).then(|| nav::mint_section_id(label, &mut seen)));
            }
        }
    }
    out
}

/// Project the post-transform element IR into `(blocks, sections)` — the single-stream
/// replacement for parsing the rendered HTML back into blocks. One [`Block`] per emitted
/// construct in reading order (a list emits one `list_item` block per item, a footnote `<aside>`
/// one `footnote` block per note — the query/index granularity the consumers expect), each
/// carrying its bbox (from the walk) and its section (the open-heading stack). Section ids come
/// from `sec_ids` (minted to match `build_toc`); figure/table ids adopt their post-dedup form
/// from `dedup_map`.
fn project_blocks(
    pages_ir: &[PageIR],
    dedup_map: &BTreeMap<String, Vec<String>>,
    sec_ids: &[Option<String>],
) -> (Vec<Block>, Vec<Section>) {
    let mut blocks: Vec<Block> = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut stack: Vec<(String, u8)> = Vec::new();
    let mut ord: usize = 0;
    let mut heading_idx = 0usize; // indexes into sec_ids, in heading walk order
    let mut group: u32 = 0; // per-document element-group ordinal (list / footnote grouping)
    // The running GLOBAL image index across pages — emit_and_merge offsets each page's local
    // `\0idx\0` sentinels by the count of prior pages' images, so a single running counter (in
    // document order) reproduces the global numbering for the stored fidelity fragments.
    let mut global_img: usize = 0;
    // Per-base-id occurrence cursor into dedup_map, advanced as we consume ids in walk order.
    let mut dedup_cursor: BTreeMap<String, usize> = BTreeMap::new();
    let deduped = |raw: &str, cursor: &mut BTreeMap<String, usize>| -> String {
        let k = cursor.entry(raw.to_string()).or_insert(0);
        let v = dedup_map.get(raw).and_then(|v| v.get(*k)).cloned().unwrap_or_else(|| raw.to_string());
        *k += 1;
        v
    };
    let next_id = |ord: &mut usize| {
        *ord += 1;
        format!("b{:04}", *ord)
    };

    for (pno, els, _uris) in pages_ir {
        let page = *pno;
        for e in els {
            let bbox = e.bbox;
            match &e.kind {
                ElKind::DestAnchors(s) => {
                    // Page-head named-destination anchors: a fidelity-only chrome carrier. Store
                    // its FINAL (deduped, image-substituted) fragment so render reproduces it.
                    let frag = finalize_fragment(s, &mut dedup_cursor, &mut global_img, &deduped);
                    let mut b = text_block(next_id(&mut ord), BlockKind::DestAnchors, String::new(), page, bbox, &stack);
                    b.el_html = Some(frag);
                    blocks.push(b);
                }
                ElKind::Header(_) => {
                    // The first-page front-matter `<header>`: a fidelity carrier (rebuilding it
                    // from `metadata` is lossy — sup author markers, the affiliation `<ol>`), so
                    // store its FINAL fragment verbatim.
                    let frag = finalize_fragment(&e.html(), &mut dedup_cursor, &mut global_img, &deduped);
                    let mut b = text_block(next_id(&mut ord), BlockKind::Header, String::new(), page, bbox, &stack);
                    b.el_html = Some(frag);
                    blocks.push(b);
                }
                ElKind::Heading { level, text, .. } => {
                    let level = *level;
                    let label = nav::strip_inline(text).trim().to_string();
                    let sid = sec_ids.get(heading_idx).cloned().flatten();
                    heading_idx += 1;
                    while stack.last().is_some_and(|(_, l)| *l >= level) {
                        stack.pop();
                    }
                    let parent = stack.last().map(|(pid, _)| pid.clone());
                    if let Some(id) = &sid {
                        sections.push(Section {
                            id: id.clone(),
                            level,
                            title: label.clone(),
                            parent,
                            page_start: page,
                            page_end: page,
                        });
                        stack.push((id.clone(), level));
                    }
                    let section = stack.last().map(|(sid, _)| sid.clone());
                    blocks.push(Block {
                        section,
                        heading_level: Some(level),
                        // The heading's INNER HTML carries the minimal inline markup the emit
                        // uses; the block text is that fragment (consumers strip it for display).
                        ..mk_block(next_id(&mut ord), BlockKind::Heading, text.clone(), page, bbox)
                    });
                }
                ElKind::Para { text } => {
                    if !nav::strip_inline(text).trim().is_empty() {
                        // A paragraph whose visible text opens a list marker is a list item.
                        let kind = if html::list_kind(&nav::strip_inline(text)).is_some() {
                            BlockKind::ListItem
                        } else {
                            BlockKind::Para
                        };
                        blocks.push(text_block(next_id(&mut ord), kind, text.clone(), page, bbox, &stack));
                    }
                }
                ElKind::List { ordered, items } => {
                    // One `list_item` block per item (the query granularity), all sharing one
                    // `el_group` + `list_ordered` so render regroups them into the single
                    // `<ul>/<ol>` they were projected from. EVERY item is kept — including an
                    // empty `<li>` (some sources emit a bullet list of empty items) — so the
                    // reconstruction is byte-faithful; the query views simply skip empty text.
                    group += 1;
                    for it in items {
                        let mut b = text_block(next_id(&mut ord), BlockKind::ListItem, it.clone(), page, bbox, &stack);
                        b.list_ordered = Some(*ordered);
                        b.el_group = Some(group);
                        blocks.push(b);
                    }
                }
                ElKind::Code { text } => {
                    // The monospace block's inner is `<pre><code>…</code></pre>`; keep that exact
                    // fragment as the block text so the IR is reproducible from the block.
                    blocks.push(text_block(next_id(&mut ord), BlockKind::Code, text.clone(), page, bbox, &stack));
                }
                ElKind::Footnotes { notes } => {
                    group += 1;
                    for n in notes {
                        if !nav::strip_inline(n).trim().is_empty() {
                            let mut b = text_block(next_id(&mut ord), BlockKind::Footnote, n.clone(), page, bbox, &stack);
                            b.el_group = Some(group);
                            blocks.push(b);
                        }
                    }
                }
                ElKind::Table { header, grid, caption } => {
                    // Consume the table's `tab-N` id (post-dedup) in walk order, carrying the
                    // deduped caption number so re-emit lands the same `<table id>`.
                    let mut deduped_tab: Option<String> = None;
                    for raw in ids_in_fragment(&e.html()) {
                        let d = deduped(&raw, &mut dedup_cursor);
                        if raw.starts_with("tab-") {
                            deduped_tab = Some(d);
                        }
                    }
                    let mut b = text_block(next_id(&mut ord), BlockKind::Table, String::new(), page, bbox, &stack);
                    b.cells = Some(table_cells(header, grid));
                    b.caption = caption.as_ref().map(|(_, c, _)| nav::strip_inline(c).trim().to_string());
                    b.label = b.caption.as_deref().and_then(caption_label);
                    // Fidelity parts for byte-exact re-emit: the detached header rows, the data
                    // grid, and the caption `(number, html, below)` with the number re-keyed to
                    // its post-dedup `tab-N` form.
                    b.table_header = Some(header.clone());
                    b.table_grid = Some(grid.clone());
                    b.table_caption = caption.as_ref().map(|(num, c, below)| {
                        let n = deduped_tab.as_deref().map(strip_tab_prefix).unwrap_or_else(|| num.clone());
                        (n, c.clone(), *below)
                    });
                    blocks.push(b);
                }
                ElKind::Figure { id, caption, .. } => {
                    // Store the figure's FINAL (deduped, image-substituted) fragment as the
                    // fidelity surface, and re-key the image asset id to the figure's post-dedup
                    // `fig-N`. `finalize_fragment` applies the SAME dedup + image substitution the
                    // body does, so the stored fragment is byte-identical to its place in the body.
                    let raw_frag = e.html();
                    let frag = finalize_fragment(&raw_frag, &mut dedup_cursor, &mut global_img, &deduped);
                    let deduped_fig = ids_in_fragment(&frag).into_iter().find(|d| d.starts_with("fig-"));
                    let mut b = text_block(next_id(&mut ord), BlockKind::Figure, String::new(), page, bbox, &stack);
                    b.caption = caption.as_ref().map(|c| nav::strip_inline(c).trim().to_string());
                    b.label = b.caption.as_deref().and_then(caption_label);
                    let has_graphic = raw_frag.contains("<image ") || raw_frag.contains("<img ") || raw_frag.contains("<svg");
                    b.image = (!id.is_empty() && has_graphic).then(|| {
                        let fid = deduped_fig.as_deref().map(strip_fig_prefix).unwrap_or_else(|| num_fig_id(id));
                        format!("img/fig_{fid}.png")
                    });
                    b.el_html = Some(frag);
                    blocks.push(b);
                }
                ElKind::Caption { text, .. } => {
                    let frag = finalize_fragment(&e.html(), &mut dedup_cursor, &mut global_img, &deduped);
                    let cap_text = nav::strip_inline(text).trim().to_string();
                    let mut b = text_block(next_id(&mut ord), BlockKind::Caption, text.clone(), page, bbox, &stack);
                    b.label = caption_label(&cap_text);
                    b.el_html = Some(frag);
                    blocks.push(b);
                }
            }
        }
    }

    finalize_section_ranges(&mut sections, &blocks);
    (blocks, sections)
}

/// The numeric part of a figure id field (`Figure.id` is the bare "N"; the emitted id is
/// `fig-{num_id(N)}`). Mirrors `html::num_id`'s slugging.
fn num_fig_id(id: &str) -> String {
    id.chars().map(|c| if c == '.' { '-' } else { c.to_ascii_lowercase() }).collect()
}

/// Strip the `fig-` prefix off a deduped figure id (`fig-3-2` → `3-2`).
fn strip_fig_prefix(id: &str) -> String {
    id.strip_prefix("fig-").unwrap_or(id).to_string()
}

/// Strip the `tab-` prefix off a deduped table id (`tab-3-2` → `3-2`).
fn strip_tab_prefix(id: &str) -> String {
    id.strip_prefix("tab-").unwrap_or(id).to_string()
}

/// Finalize an element's emitted HTML fragment into the FORM IT TAKES IN THE FINAL BODY, so the
/// model can store it as a byte-exact fidelity surface (and a model-only re-render reproduces it
/// even though `emit_and_merge`/`assemble` are idempotent over already-finalized fragments):
/// (1) every `id="…"` is renamed to its post-dedup form (via the running `dedup_cursor` over the
/// document-order `dedup_map`), exactly as `dedup_ids` does; (2) every page-local `\0idx\0` image
/// sentinel becomes its FINAL `<image N>` placeholder (drop mode), with `N` the 1-based GLOBAL
/// image index — `global_img` is the running count of images seen in document order, which
/// reproduces `emit_and_merge`'s per-page offset accumulation.
fn finalize_fragment(
    frag: &str,
    dedup_cursor: &mut BTreeMap<String, usize>,
    global_img: &mut usize,
    deduped: &impl Fn(&str, &mut BTreeMap<String, usize>) -> String,
) -> String {
    let mut out = String::with_capacity(frag.len());
    let bytes = frag.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // An `id="…"` literal → emit the deduped id.
        if frag[i..].starts_with("id=\"") {
            out.push_str("id=\"");
            i += 4;
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let raw = &frag[start..i];
            out.push_str(&deduped(raw, dedup_cursor));
            // keep the closing quote
            if i < bytes.len() {
                out.push('"');
                i += 1;
            }
            continue;
        }
        // A `\0idx\0` image sentinel → its final global `<image N>` number (drop mode).
        if bytes[i] == 0 {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != 0 {
                j += 1;
            }
            // (idx is page-local; the number we emit is the running global index + 1.)
            out.push_str(&(*global_img + 1).to_string());
            *global_img += 1;
            i = j + 1; // skip past the closing NUL
            continue;
        }
        let c = frag[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A table's row-major cell grid for the block projection: detached header rows (text only,
/// colspans expanded to one cell per spanned column) followed by the data grid — the same cell
/// sequence the rendered `<table>` shows, so `find`/markdown over `cells` matches the HTML.
fn table_cells(header: &[Vec<(String, usize)>], grid: &[Vec<String>]) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for hrow in header {
        let mut row = Vec::new();
        for (text, span) in hrow {
            for _ in 0..(*span).max(1) {
                row.push(text.trim().to_string());
            }
        }
        rows.push(row);
    }
    for r in grid {
        rows.push(r.iter().map(|c| c.trim().to_string()).collect());
    }
    rows
}

/// Make a block carrying the common fields (the kind-specific fields are filled by the caller),
/// with `text` taken VERBATIM (it is the element's inner HTML — the query consumers strip
/// markup; the renderer reconstructs from it).
fn mk_block(id: String, kind: BlockKind, text: String, page: u32, bbox: Option<Bbox>) -> Block {
    Block {
        id,
        kind,
        text,
        page,
        section: None,
        bbox,
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

/// Construct a text-bearing block attributing it to the current open section, carrying its bbox.
fn text_block(id: String, kind: BlockKind, text: String, page: u32, bbox: Option<Bbox>, stack: &[(String, u8)]) -> Block {
    Block {
        section: stack.last().map(|(sid, _)| sid.clone()),
        ..mk_block(id, kind, text, page, bbox)
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

// ---- small helpers (scoped to distillPDF's known output) -------------------

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
    use crate::html::PageElement;

    /// Project a hand-built per-page IR into blocks + sections (minting + dedup like build_model).
    fn project(pages_ir: &[PageIR]) -> (Vec<Block>, Vec<Section>) {
        let dedup = post_dedup_id_map(pages_ir);
        let sec_ids = mint_section_ids(pages_ir, &dedup);
        project_blocks(pages_ir, &dedup, &sec_ids)
    }

    fn heading(level: u8, text: &str) -> PageElement {
        PageElement::at(ElKind::Heading { level, id: String::new(), text: text.into() }, None)
    }
    fn para(text: &str) -> PageElement {
        PageElement::at(ElKind::Para { text: text.into() }, None)
    }

    #[test]
    fn projects_sections_blocks_and_pages() {
        let ir: Vec<PageIR> = vec![
            (1, vec![heading(1, "Title A"), para("Intro para."), heading(2, "Sub B"), para("Body of B.")], vec![]),
            (2, vec![para("More B on page 2.")], vec![]),
        ];
        let (blocks, sections) = project(&ir);
        // 5 blocks: h1, p, h2, p, p.
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0].kind, BlockKind::Heading);
        assert_eq!(blocks[0].text, "Title A");
        assert_eq!(blocks[0].page, 1);
        assert_eq!(blocks[0].section.as_deref(), Some("sec-title-a"));
        assert_eq!(blocks[1].kind, BlockKind::Para);
        assert_eq!(blocks[1].section.as_deref(), Some("sec-title-a"));
        // the sub-heading nests; its body attributes to it.
        assert_eq!(blocks[3].section.as_deref(), Some("sec-sub-b"));
        // the page-2 block stays in sec-sub-b (no new heading), page tracked.
        assert_eq!(blocks[4].page, 2);
        assert_eq!(blocks[4].section.as_deref(), Some("sec-sub-b"));
        // sections: sec-title-a (parent None), sec-sub-b (parent sec-title-a, spans pages 1..2).
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].id, "sec-sub-b");
        assert_eq!(sections[1].parent.as_deref(), Some("sec-title-a"));
        assert_eq!(sections[1].page_start, 1);
        assert_eq!(sections[1].page_end, 2);
        assert_eq!(sections[0].page_end, 2);
    }

    #[test]
    fn projects_table_and_figure() {
        let table = PageElement::at(
            ElKind::Table {
                header: vec![vec![("A".into(), 1), ("B".into(), 1)]],
                grid: vec![vec!["1".into(), "2".into()]],
                caption: None,
            },
            None,
        );
        let fig = PageElement::at(
            ElKind::Figure {
                html: "<figure id=\"fig-3\"><image 1><figcaption>Figure 3: A chart.</figcaption></figure>".into(),
                id: "3".into(),
                caption: Some("Figure 3: A chart.".into()),
                image: Some("img/fig_3.png".into()),
                svg: None,
            },
            None,
        );
        let (blocks, _) = project(&[(1, vec![table, fig], vec![])]);
        let table = blocks.iter().find(|b| b.kind == BlockKind::Table).unwrap();
        assert_eq!(table.cells.as_ref().unwrap(), &vec![vec!["A".to_string(), "B".into()], vec!["1".into(), "2".into()]]);
        let fig = blocks.iter().find(|b| b.kind == BlockKind::Figure).unwrap();
        assert_eq!(fig.caption.as_deref(), Some("Figure 3: A chart."));
        assert_eq!(fig.label.as_deref(), Some("Figure 3"));
        assert_eq!(fig.image.as_deref(), Some("img/fig_3.png"));
    }

    #[test]
    fn projects_footnote_aside() {
        let notes = PageElement::at(
            ElKind::Footnotes { notes: vec!["1. First note.".into(), "2. Second note.".into()] },
            None,
        );
        let (blocks, _) = project(&[(1, vec![notes], vec![])]);
        let notes: Vec<_> = blocks.iter().filter(|b| b.kind == BlockKind::Footnote).collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].text, "1. First note.");
    }

    #[test]
    fn duplicate_figure_id_adopts_post_dedup_form() {
        // Two figures both minted fig-3 — dedup renames the second to fig-3-2 in the HTML, and
        // the block/asset must address that post-dedup id.
        let mk = || PageElement::at(
            ElKind::Figure {
                html: "<figure id=\"fig-3\"><image 1></figure>".into(),
                id: "3".into(),
                caption: None,
                image: Some("img/fig_3.png".into()),
                svg: None,
            },
            None,
        );
        let (blocks, _) = project(&[(1, vec![mk(), mk()], vec![])]);
        let figs: Vec<_> = blocks.iter().filter(|b| b.kind == BlockKind::Figure).collect();
        assert_eq!(figs[0].image.as_deref(), Some("img/fig_3.png"));
        assert_eq!(figs[1].image.as_deref(), Some("img/fig_3-2.png"));
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