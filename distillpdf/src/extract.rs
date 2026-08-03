//! Image, font and table extraction pillars, built on lopdf's object model.
//!
//! Pure Rust: these return plain owned structs. The PyO3 layer (`src/lib.rs`) assembles the
//! Python dicts/lists from them — no pyo3 types appear in this module.

use crate::pdfobj::{deref, filters_of, sub_dict};
use crate::raster::{assemble_png, codec_payload, filter_to_format, image_bpc, image_color_space, normalized_jpeg_png};
use crate::text::{self, Span};
use lopdf::{Dictionary, Document, ObjectId};
use std::collections::{BTreeMap, HashSet, VecDeque};

/// One extracted raster image. Mirrors the dict `Pdf.extract_images` returns:
/// `{page, index, width, height, color_space, bits_per_component, format, data}`.
pub struct ImageInfo {
    pub page: u32,
    pub index: usize,
    pub width: i64,
    pub height: i64,
    /// The image's colour space **family**, resolved through indirect references and
    /// through a name defined in the resource dictionary's `/ColorSpace` sub-dictionary:
    /// `DeviceRGB`, `DeviceGray`, `DeviceCMYK`, `ICCBased`, `Indexed`, `Separation`, …
    /// `None` only when the image genuinely declares no colour space (a `/ImageMask`
    /// stencil, or a JPXDecode stream that carries it inside the codestream).
    pub color_space: Option<String>,
    /// `/BitsPerComponent` — 1, 2, 4, 8 or 16. `None` when the image declares none and it
    /// cannot be inferred (JPXDecode, where the codestream carries it).
    pub bits_per_component: Option<i64>,
    pub format: &'static str,
    pub data: Vec<u8>,
}

/// One page/font row. Mirrors the dict `Pdf.extract_fonts` returns:
/// `{page, name, subtype, base_font, encoding, embedded, has_tounicode}`.
pub struct FontInfo {
    pub page: u32,
    pub name: String,
    pub subtype: String,
    pub base_font: String,
    pub encoding: String,
    pub embedded: bool,
    pub has_tounicode: bool,
}

/// One detected table. `cells` is the row-major grid; the binding derives `n_rows`/`n_cols`.
pub struct TableInfo {
    pub page: u32,
    pub cells: Vec<Vec<String>>,
}

/// Every resource dictionary a page can reach: its own `/Resources` (plus whatever it
/// inherits from the page tree) and then, transitively, the `/Resources` of every
/// `/Subtype /Form` XObject it references. Annotation appearance streams are **not**
/// reached from here — they hang off `/Annots`, and [`appearance_resource_dicts`] is the
/// walk that finds them.
///
/// This is deliberately *not* what `img.rs` / `text.rs` do. Those are content-stream
/// interpreters: they decode operators because they need placement (the CTM, the form's
/// `/Matrix`, graphics state), and they disagree on resource-inheritance semantics for
/// good reasons of their own. The extract API needs neither placement nor content
/// decoding — only "what does this page's resource tree reach", which is the same
/// resource-walk semantics pymupdf reports images and fonts under. So no operator is
/// parsed here and no stream is decompressed.
///
/// Ordering is stable and deterministic: the page's own dictionaries come first (so
/// directly-referenced resources keep the index they had before recursion existed) and
/// nested forms are appended breadth-first in `/XObject` dictionary order. A visited-
/// `ObjectId` set cuts reference cycles; `crate::MAX_FORM_DEPTH` caps nesting.
fn page_resource_dicts(doc: &Document, page_id: ObjectId) -> Vec<&Dictionary> {
    let mut queue: VecDeque<(&Dictionary, u32)> = VecDeque::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();

    if let Ok((own, inherited)) = doc.get_page_resources(page_id) {
        if let Some(d) = own {
            queue.push_back((d, 0));
        }
        // `inherited` is ordered page → parent → …, so the page's own resources (when
        // written as a reference rather than inline) still lead.
        for id in inherited {
            if !seen.insert(id) {
                continue;
            }
            if let Ok(d) = doc.get_dictionary(id) {
                queue.push_back((d, 0));
            }
        }
    }
    resource_bfs(doc, queue, &mut seen)
}

/// Every resource dictionary reachable from an annotation's **appearance stream**
/// (`/Annots` → `/AP` → `/N`), transitively through the Form XObjects it references.
///
/// An appearance stream hangs off the annotation, not off the page's `/Resources` — so
/// nothing [`page_resource_dicts`] reaches can ever name an image that lives only in a
/// stamp's or a widget's appearance. `extract_images` appends these dictionaries **after**
/// the page's own, which is why every `(page, index)` a page already reported is unchanged
/// and appearance rows land at the end.
///
/// `extract_fonts` deliberately does **not** consume this — see its doc comment.
fn appearance_resource_dicts(doc: &Document, page_id: ObjectId) -> Vec<&Dictionary> {
    let mut queue: VecDeque<(&Dictionary, u32)> = VecDeque::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    for (id, stream) in crate::walker::appearance_streams(doc, page_id) {
        if !seen.insert(id) {
            continue; // one appearance stream shared by several annotations
        }
        // §12.5.5: an appearance stream's resources are its OWN — it inherits nothing from
        // the page, the same rule the nested-form step below applies.
        if let Some(fr) = sub_dict(doc, &stream.dict, b"Resources") {
            queue.push_back((fr, 0));
        }
    }
    resource_bfs(doc, queue, &mut seen)
}

/// The shared body of both resource walks: drain `queue` breadth-first, appending each
/// dictionary and then queueing the own-`/Resources` of every `/Subtype /Form` XObject it
/// names. `seen` cuts reference cycles (and is pre-seeded by the caller with whatever it
/// has already visited); [`crate::MAX_FORM_DEPTH`] caps nesting.
fn resource_bfs<'a>(
    doc: &'a Document,
    mut queue: VecDeque<(&'a Dictionary, u32)>,
    seen: &mut HashSet<ObjectId>,
) -> Vec<&'a Dictionary> {
    let mut out: Vec<&Dictionary> = Vec::new();
    while let Some((res, depth)) = queue.pop_front() {
        out.push(res);
        if depth >= crate::MAX_FORM_DEPTH {
            continue; // nesting cap (a self-referential form is already cut by `seen`)
        }
        let Some(xobjects) = sub_dict(doc, res, b"XObject") else {
            continue;
        };
        for (_, v) in xobjects.iter() {
            let Ok(id) = v.as_reference() else { continue };
            if !seen.insert(id) {
                continue; // already walked: a shared or self-referential form
            }
            let Ok(stream) = doc.get_object(id).and_then(|o| o.as_stream()) else {
                continue;
            };
            if stream.dict.get(b"Subtype").and_then(|o| o.as_name()).unwrap_or(b"") != b"Form" {
                continue;
            }
            // A form's resources live in its OWN /Resources (PDF 32000-1 §8.10.2); a form
            // without one contributes nothing we could resolve.
            if let Some(fr) = sub_dict(doc, &stream.dict, b"Resources") {
                queue.push_back((fr, depth + 1));
            }
        }
    }
    out
}

/// Collect the image XObjects invoked by `Do` in this operator list, descending into the
/// `/Subtype /Form` XObjects the stream actually invokes (a form's content can `Do`
/// further XObjects). `xmap` is the resource scope in force: a form starts from its
/// parent's map with its own `/Resources` overlaid
/// ([`crate::walker::ScopePolicy::OverlayParent`]), so an unqualified `/Im0` resolves to
/// *that* form's `/Im0` and not a sibling's.
///
/// This is the one walk that is a **collector**, not a renderer, and the difference is
/// load-bearing. It answers "which images exist on this page", so a form already walked
/// contributes nothing new and is skipped by `seen` — which also bounds the recursion, and
/// is why this walk takes no [`crate::WalkBudget`]: with a visited set the work is bounded
/// by the number of distinct forms, so the budget the three renderers need (they must
/// repaint a repeated form and therefore cannot dedupe) would only risk truncating a
/// legitimately huge document. It composes `walker`'s descent pieces rather than calling
/// `walker::descend_form`, which is the budgeted composition.
fn walk_drawn(
    doc: &Document,
    ops: &[lopdf::content::Operation],
    xmap: &crate::walker::XMap,
    depth: u32,
    seen: &mut HashSet<ObjectId>,
    out: &mut HashSet<ObjectId>,
) {
    for op in ops {
        if op.operator != "Do" {
            continue;
        }
        let Some((id, stream)) = crate::walker::xobject_at(doc, xmap, &op.operands) else {
            continue; // not a name, a dangling name, or not a stream: nothing to draw
        };
        match crate::walker::subtype_of(stream) {
            b"Image" => {
                out.insert(id);
            }
            b"Form" => {
                if crate::walker::too_deep(depth) {
                    continue; // the one nesting cap
                }
                if !seen.insert(id) {
                    continue; // a form already walked on this page: cycle / repeat guard
                }
                let Some(scope) = crate::walker::form_scope(doc, stream, xmap, crate::walker::ScopePolicy::OverlayParent)
                else {
                    continue;
                };
                if let Some(ops) = crate::walker::form_ops(stream) {
                    walk_drawn(doc, &ops, &scope.xobjects, depth + 1, seen, out);
                }
            }
            _ => {}
        }
    }
}

/// The image XObjects this page's content stream actually DRAWS.
///
/// Reachability through the resource tree is not the same question. A producer may share
/// ONE `/Resources` dictionary across every page (iText does; one 166-page corpus document
/// lists all 166 of its page-body forms in a single shared dict), which makes a pure
/// resource walk report every image in the document on every page — 56,108 rows and 2.4 GB
/// of pixels for 338 distinct images. What a page *contains* is what it paints, so the
/// `Do` operands decide membership and [`page_resource_dicts`] is left to do what it is
/// good at: resolving a name to an object and fixing the report order.
///
/// A page's **annotation appearances** are walked too, each as a form in its own right:
/// `/Annots → /AP /N` is a content stream a viewer paints onto the page, and it is not
/// reachable from the page's content stream or its `/Resources`. Its names resolve in its
/// own `/Resources` alone ([`crate::walker::ScopePolicy::OwnOnly`], §12.5.5).
///
/// `None` when the page's content stream can't be read or parsed at all — the caller then
/// falls back to plain reachability rather than silently reporting an image-less page.
fn drawn_images(doc: &Document, page_id: ObjectId) -> Option<HashSet<ObjectId>> {
    let mut xmap = crate::walker::XMap::new();
    if let Ok((own, inherited)) = doc.get_page_resources(page_id) {
        // `inherited` runs page → parent → …; apply it outermost-first so the nearest
        // scope wins, then the page's own inline dictionary last of all.
        for id in inherited.iter().rev() {
            if let Ok(d) = doc.get_dictionary(*id) {
                crate::walker::overlay_xobjects(doc, d, &mut xmap);
            }
        }
        if let Some(d) = own {
            crate::walker::overlay_xobjects(doc, d, &mut xmap);
        }
    }
    let content = doc.get_page_content(page_id).ok()?;
    let ops = lopdf::content::Content::decode(&content).ok()?;
    let mut out = HashSet::new();
    let mut seen = HashSet::new();
    walk_drawn(doc, &ops.operations, &xmap, 0, &mut seen, &mut out);
    for (id, ap) in crate::walker::appearance_streams(doc, page_id) {
        if !seen.insert(id) {
            continue; // shared between annotations, or already reached from the content
        }
        let Some(scope) = crate::walker::form_scope(doc, ap, &crate::walker::XMap::new(), crate::walker::ScopePolicy::OwnOnly)
        else {
            continue; // no /Resources: nothing its names could resolve against
        };
        if let Some(ops) = crate::walker::form_ops(ap) {
            // The appearance stream is itself one form level below the page's content.
            walk_drawn(doc, &ops, &scope.xobjects, 1, &mut seen, &mut out);
        }
    }
    Some(out)
}

/// Does any of these resource dictionaries name an **image** XObject?
///
/// Dict-only, exactly like the walk that produced `dicts`: it resolves references and reads
/// `/Subtype`, and decompresses nothing. The predicate is deliberately the *same* one
/// [`extract_images`]'s enumeration loop applies to decide whether a `/XObject` entry can
/// become a row — a named reference whose object is a stream with `/Subtype /Image` — so
/// "false here" means "that loop would push nothing for this page, whatever the drawn set
/// says". That is what makes the short-circuit semantics-preserving rather than a heuristic.
fn reaches_image_xobject(doc: &Document, dicts: &[&Dictionary]) -> bool {
    dicts.iter().any(|res| {
        sub_dict(doc, res, b"XObject").is_some_and(|xobjects| {
            xobjects.iter().any(|(_, v)| {
                v.as_reference()
                    .ok()
                    .and_then(|id| doc.get_object(id).ok())
                    .and_then(|o| o.as_stream().ok())
                    .is_some_and(|s| crate::walker::subtype_of(s) == b"Image")
            })
        })
    })
}

/// The `/Filter` chain as display names, in application order — the `String` view of
/// [`pdfobj::filters_of`] that [`ImageInfo::filters`] reports.
fn image_filters(dict: &Dictionary) -> Vec<String> {
    filters_of(dict).iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect()
}

/// Extract images from all pages as owned [`ImageInfo`] rows.
///
/// A page's images are the ones its content stream actually **draws** ([`drawn_images`]),
/// including those drawn from inside a Form XObject — lopdf's `get_page_images()` reads
/// the page's own `/Resources` and stops there, which left 13 of 54 corpus documents
/// (every LaTeX/e-filing producer that wraps its page body in a form) reporting no images
/// at all. An image merely *reachable* through the resource tree but never painted is not
/// this page's image; reporting those made a shared-`/Resources` producer return every
/// image in the document on every page.
///
/// [`page_resource_dicts`] still drives enumeration, so the reported `(page, index)`
/// ordering is unchanged wherever the drawn set equals the reachable set; the annotation
/// appearances ([`appearance_resource_dicts`]) are enumerated after it, so an image that
/// exists only inside a stamp's or a widget's `/AP /N` stream is reported at the end.
/// An appearance image that the page's resource tree *also* reaches (a producer sharing one
/// `/Resources` across the file) is reported from there instead, at its ordinary place in
/// resource-dictionary order — an annotation appearance widens the **drawn set**, and a
/// newly-drawn image takes the index its enumeration position gives it, exactly like any
/// other.
///
/// `data` is a blob the caller can open, not the verbatim stream: a coded image is peeled
/// back to its codec payload (so a `[/FlateDecode /DCTDecode]` stream is a JPEG file, not
/// a Flate blob) and a plain sample block is assembled into a PNG via [`assemble_png`]
/// with `format` reported as `"png"`. Only samples we cannot faithfully reduce keep
/// `format:"raw"` — and those now carry `color_space` and `bits_per_component`, which is
/// what a caller needs to reassemble them by hand.
pub fn extract_images(doc: &Document) -> Vec<ImageInfo> {
    extract_images_inner(doc, true)
}

/// The body of [`extract_images`], with the resource-tree short-circuit switchable so a test
/// can assert the two paths agree ([`tests::the_short_circuit_reports_exactly_what_the_full_walk_reports`]).
/// Production always passes `true`; `false` is the full-walk oracle.
fn extract_images_inner(doc: &Document, short_circuit: bool) -> Vec<ImageInfo> {
    let mut out = Vec::new();
    for (&pno, &page_id) in &doc.get_pages() {
        // The page's own resource tree first, so every `(page, index)` a page already
        // reported keeps it; the annotation appearances are appended after.
        let mut dicts = page_resource_dicts(doc, page_id);
        dicts.extend(appearance_resource_dicts(doc, page_id));
        // A page whose resource tree reaches no image XObject cannot report one, because
        // enumeration below runs over exactly these dictionaries and `drawn` can only
        // *remove* candidates from it — so the content walk is pure cost. On a 102-page
        // regulation with zero images that walk was lexing 2.4 MB of content streams (77% of
        // the operation, in lopdf's lexer) to conclude nothing. Both dict walks above parse
        // no operator and decompress no stream, and the scan is the enumeration loop's own
        // predicate, so skipping is by construction unobservable — not an approximation.
        if short_circuit && !reaches_image_xobject(doc, &dicts) {
            continue;
        }
        let mut index = 0usize;
        let drawn = drawn_images(doc, page_id);
        // Dedup is across resource dictionaries only: an image the page's own /XObject
        // already listed is not re-reported when a nested form points at it too. Repeats
        // *within* one dictionary are kept, so the `index` a directly-referenced image had
        // before this walk existed is unchanged.
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut from_this_dict: Vec<ObjectId> = Vec::new();
        for res in dicts {
            seen.extend(from_this_dict.drain(..));
            let Some(xobjects) = sub_dict(doc, res, b"XObject") else {
                continue;
            };
            for (_, v) in xobjects.iter() {
                let Ok(id) = v.as_reference() else { continue };
                if drawn.as_ref().is_some_and(|d| !d.contains(&id)) {
                    continue; // reachable from the resource tree, but this page never paints it
                }
                if seen.contains(&id) {
                    continue; // already reported from an outer resource dictionary
                }
                let Ok(stream) = doc.get_object(id).and_then(|o| o.as_stream()) else {
                    continue;
                };
                let dict = &stream.dict;
                if dict.get(b"Subtype").and_then(|o| o.as_name()).unwrap_or(b"") != b"Image" {
                    continue;
                }
                let (Ok(width), Ok(height)) = (
                    dict.get(b"Width").and_then(|o| o.as_i64()),
                    dict.get(b"Height").and_then(|o| o.as_i64()),
                ) else {
                    continue; // not a usable image row without dimensions
                };
                let filters = image_filters(dict);
                let mut format = filter_to_format(&Some(filters.clone()));
                // Hand back something a caller can actually open. A coded image gives up
                // its codec payload (a Flate-wrapped JPEG becomes a JPEG file); a `raw`
                // sample block is assembled into a PNG, and stays `raw` — with the
                // metadata to reassemble it by hand — only when it cannot be.
                let mut data = if format == "raw" {
                    match assemble_png(doc, res, stream) {
                        Some(png) => {
                            format = "png";
                            png
                        }
                        None => stream.content.clone(),
                    }
                } else {
                    codec_payload(stream).into_owned()
                };
                // A CMYK JPEG is decoded to the wrong colours by every consumer that reads
                // it as a standalone file, so it is normalized rather than passed through.
                if format == "jpeg" {
                    if let Some(png) = normalized_jpeg_png(doc, dict, &data) {
                        format = "png";
                        data = png;
                    }
                }
                out.push(ImageInfo {
                    page: pno,
                    index,
                    width,
                    height,
                    color_space: image_color_space(doc, res, dict),
                    bits_per_component: image_bpc(doc, dict),
                    format,
                    data,
                });
                index += 1;
                from_this_dict.push(id);
            }
        }
    }
    out
}

/// Group spans into visual rows (top-to-bottom), cells left-to-right.
fn rows_of(mut spans: Vec<Span>) -> Vec<Vec<Span>> {
    spans.retain(|s| !s.text.trim().is_empty() && s.angle.abs() < 0.01); // rotated text isn't tabular
    if spans.is_empty() {
        return Vec::new();
    }
    let band = (spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32 * 0.6).max(2.0);
    // Cluster by actual y-proximity, not rounded bands: a span joins the current
    // row if within `band` (≈half a line) of the row's reference y. This merges
    // small sub/superscripts (e.g. the "BASE"/"LARGE" in BERT_BASE/BERT_LARGE,
    // ~1pt off the baseline) into their row instead of letting a rounding boundary
    // split them into a 1-cell row that would flush a table run mid-table.
    spans.sort_by(|p, q| q.y.partial_cmp(&p.y).unwrap_or(std::cmp::Ordering::Equal));
    let mut rows: Vec<Vec<Span>> = Vec::new();
    let mut ref_y: Option<f32> = None;
    for s in spans {
        if ref_y.is_none_or(|ry| (ry - s.y).abs() > band) {
            rows.push(Vec::new());
            ref_y = Some(s.y);
        }
        rows.last_mut().unwrap().push(s);
    }
    for r in &mut rows {
        r.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
    }
    rows
}

/// A merged cell: text, its left x-edge, and current right edge.
struct Cell {
    x: f32,
    end: f32,
    text: String,
}

/// Merge a row's word-spans into cells: small inter-word gaps (prose) collapse;
/// wide gutters (table columns) stay separate.
/// Whether a space belongs between two glyph-runs joined into one cell — mirrors
/// the HTML typographic binding so "33"+"."+"20" becomes "33.20" (a single value),
/// not "33 . 20" (which then reads as two columns).
fn join_space(prev: &str, next: &str) -> bool {
    let (p, n) = match (prev.chars().last(), next.chars().next()) {
        (Some(p), Some(n)) => (p, n),
        _ => return false,
    };
    if ")]},.;:!?%".contains(n) {
        return false; // no space before closing/trailing punctuation
    }
    if "([{".contains(p) {
        return false; // no space after an opening bracket
    }
    if matches!(p, '.' | ':' | '/' | '-' | ',' | '\u{2212}') && n.is_ascii_digit() {
        return false; // numeric separator (decimal/ratio/range): 33.20, 1:3, 27-31
    }
    true
}

fn is_num_token(t: &str) -> bool {
    let t = t.trim();
    !t.is_empty() && t.chars().any(|c| c.is_ascii_digit()) && t.chars().all(|c| c.is_ascii_digit() || ".,%+-±()*".contains(c) || c == '\u{2212}')
}

fn row_cells(row: &[Span]) -> Vec<Cell> {
    let mut cells: Vec<Cell> = Vec::new();
    for s in row {
        let txt = s.text.trim();
        if txt.is_empty() {
            continue;
        }
        let w = if s.width > 0.1 { s.width } else { txt.chars().count() as f32 * s.size * 0.5 };
        let gap = cells.last().map_or(f32::INFINITY, |p| s.x - p.end);
        // Two NUMERIC tokens separated by any real gap are adjacent data columns,
        // not one cell ("33.20 0.963" → two cells); they merge only on a hair-thin
        // gap (same number split mid-glyph). Text ("BERT BASE") still merges up to
        // the normal column gutter.
        let numeric_split = is_num_token(txt)
            && cells.last().is_some_and(|p| is_num_token(p.text.rsplit(' ').next().unwrap_or("")))
            && gap > s.size * 0.45;
        match cells.last_mut() {
            Some(prev) if gap < s.size * 1.3 && !numeric_split => {
                // A space belongs where a READER sees one, and nowhere else. This branch
                // used to space every merged pair unconditionally — correct for the
                // word-level spans it was written against, and shredding for a generator
                // that emits one `Tj` per glyph: `Texas` came back as `T e x a s`, and with
                // it 266 of the corpus's 632 tables (every SEC filing header). The body
                // path and the figure-label path already draw this line at
                // [`crate::textutil::SPACE_GAP`]; this is the copy that was missing.
                if !crate::textutil::glyph_adjacent(gap, s.size) && join_space(&prev.text, txt) {
                    prev.text.push(' ');
                }
                prev.text.push_str(txt);
                prev.end = s.x + w;
            }
            _ => cells.push(Cell { x: s.x, end: s.x + w, text: txt.to_string() }),
        }
    }
    cells
}

/// Cluster cell LEFT edges into column anchors (gap-based, tolerance `tol`). This is the
/// pre-band-model detector, kept as a FALLBACK: the whitespace-lane `column_bands` is the
/// primary, but on a wide-first-column table (e.g. the Transformer "Layer Type | …" Table 1)
/// a long row label bridges the lane and merges columns, so the band model degenerates to
/// <2 columns and the table is lost. Left-x clustering recovers those — it anchors on where
/// each column STARTS, which a wide neighbour doesn't disturb.
fn columns(rows: &[Vec<Cell>], tol: f32) -> Vec<f32> {
    let mut xs: Vec<f32> = rows.iter().flat_map(|r| r.iter().map(|c| c.x)).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut cols: Vec<f32> = Vec::new();
    for x in xs {
        if cols.last().is_none_or(|&c| x - c > tol) {
            cols.push(x);
        }
    }
    cols
}

/// Index of the column anchor nearest to `x` (left-x fallback occupancy counting).
fn nearest_col(cols: &[f32], x: f32) -> Option<usize> {
    cols.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (x - **a).abs().partial_cmp(&(x - **b).abs()).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
}

/// Column bands via vertical whitespace lanes (PASS 1 of table parsing).
///
/// Project every cell's x-interval from the data rows AND any header rows, then read
/// off the maximal x-ranges that some row covers; the clear gaps between them are the
/// column separators. This keys on WHERE TEXT SITS, not where it starts, so a
/// right-aligned numeric column (whose left edges scatter row to row) stays a single
/// band, and because the header rows are projected too, a SPARSE column the body
/// rarely fills is still a band (the header spans it). `bridge` is how many outlier
/// rows may span a lane before it stops being a separator (0 = a lane must be fully
/// clear). Cells within a row are disjoint, so interval coverage == row coverage.
/// Returns each column band as (lo, hi), left→right; deterministic (event sweep).
fn column_bands(rows: &[&[Cell]], bridge: usize) -> Vec<(f32, f32)> {
    let mut ev: Vec<(f32, i32)> = Vec::new();
    for r in rows {
        for c in *r {
            if c.end > c.x {
                ev.push((c.x, 1));
                ev.push((c.end, -1));
            }
        }
    }
    if ev.is_empty() {
        return Vec::new();
    }
    ev.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.cmp(&a.1)));
    let mut bands: Vec<(f32, f32)> = Vec::new();
    let mut cov = 0i32;
    let mut prev_x = ev[0].0;
    let mut in_band = false;
    let (mut lo, mut hi) = (0.0f32, 0.0f32);
    for (x, d) in ev {
        if x > prev_x {
            // segment [prev_x, x) carried coverage `cov`
            if cov as usize > bridge {
                if !in_band {
                    in_band = true;
                    lo = prev_x;
                }
                hi = x;
            } else if in_band {
                bands.push((lo, hi));
                in_band = false;
            }
        }
        cov += d;
        prev_x = x;
    }
    if in_band {
        bands.push((lo, hi));
    }
    bands
}

/// Index of the band whose interval contains `x`, else the nearest band by distance
/// to its interval. Used to assign a span to a column in PASS 2.
fn band_of(bands: &[(f32, f32)], x: f32) -> Option<usize> {
    if bands.is_empty() {
        return None;
    }
    for (i, &(lo, hi)) in bands.iter().enumerate() {
        if x >= lo && x <= hi {
            return Some(i);
        }
    }
    bands
        .iter()
        .enumerate()
        .min_by(|(_, &(lo, hi)), (_, &(lo2, hi2))| {
            let d = |l: f32, h: f32| if x < l { l - x } else { x - h };
            d(lo, hi).partial_cmp(&d(lo2, hi2)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
}

/// Structural ADMISSION test: is this region a genuine data table, or prose / an
/// equation / a symbolic matrix that merely happens to have aligned tokens?
///
/// This is the single backstop that keeps false positives out. It is deliberately
/// kept SEPARATE from column-keeping (how many columns survive) so that recovering a
/// sparse column can never silently re-admit a prose/equation block: admission reads
/// the region's content, column-keeping reads its geometry, and the two no longer
/// interfere. Returns true to accept the region as a table.
fn is_coherent_grid(grid: &[Vec<String>]) -> bool {
    // Prose guard: real tabular cells are terse. A 2-column block averaging >4
    // words/cell is running prose (wrapped body lines), not a table.
    let (mut wc, mut nz, mut prose) = (0usize, 0usize, 0usize);
    for row in grid {
        for c in row {
            let w = c.split_whitespace().count();
            if w > 0 {
                wc += w;
                nz += 1;
                if w > 8 {
                    prose += 1;
                }
            }
        }
    }
    let mean_words = if nz > 0 { wc as f32 / nz as f32 } else { 0.0 };
    let ncols = grid.first().map(|r| r.len()).unwrap_or(0);
    if ncols <= 2 && mean_words > 4.0 {
        return false;
    }
    // 2-col body gridded into 3 cols (gutter-crossing title): tell is a phantom
    // anchor column empty in nearly every row plus long cells. Real 3-col tables
    // are populated, so they pass (e.g. the W-9 field tables).
    let has_empty_col = ncols > 0
        && !grid.is_empty()
        && (0..ncols).any(|c| {
            let empty = grid.iter().filter(|r| r.get(c).is_none_or(|s| s.trim().is_empty())).count();
            empty * 5 >= grid.len() * 4
        });
    if ncols == 3 && mean_words > 4.5 && has_empty_col {
        return false;
    }
    // Wider mis-grids: reject only when nearly every cell is a full sentence.
    if nz >= 6 && prose * 3 >= nz * 2 && mean_words > 6.0 {
        return false;
    }
    // Display EQUATION mis-detected as a table: cells are dominated by math
    // operators / Greek (not numeric data) and the region carries an '=' or an
    // equation-number "(N)". Reject so the equation stays in the text flow
    // (where it is reassembled as one block) instead of a spurious <table>. A
    // numeric data table has no operators and no '=', so it is unaffected.
    let opcell = |t: &str| t.chars().any(|c| "=+−–×÷·≤≥≠≈∝∫∑∏√∈∉∂∇→←↔⇒⇐↦∼≜≡∥⟨⟩".contains(c) || "αβγδεζηθικλμνξπρςστυϕφχψωΓΔΘΛΞΠΣΦΨΩ".contains(c));
    let op = grid.iter().flatten().filter(|c| opcell(c)).count();
    // An equation is signalled by a RELATION — '=' or an inequality/equivalence
    // (≤ ≥ ≠ ≈ ≜ ≡ ∝). These appear in display math/inequalities but almost never as
    // the content of a data cell (a stats table's "p ≤ 0.05" carries real words too,
    // which the alpha_words gate below preserves).
    let has_rel = grid.iter().flatten().any(|c| c.chars().any(|ch| "=≤≥≠≈≜≡∝".contains(ch)));
    let eqnum = grid.iter().flatten().any(|c| {
        let t = c.trim();
        let inner: String = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or("").to_string();
        !inner.is_empty() && inner.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
    });
    // Real (alphabetic, ≥2-letter) words — an equation has almost none; its
    // "words" are space-separated symbols. A data table has real words.
    let alpha_words = grid.iter().flatten().flat_map(|c| c.split(|ch: char| !ch.is_alphabetic())).filter(|w| w.chars().count() >= 3).count();
    // Reject an equation region: it carries a relation or eq-number, OR it is
    // operator-dense (a relation/arrow chain), and it has almost no real words.
    if nz > 0 && alpha_words <= nz && ((op >= 1 && (has_rel || eqnum)) || op * 2 >= nz) {
        return false;
    }
    // Symbolic MATRIX/array mis-detected as a table (e.g. a block matrix of
    // subscripted variables W₀, D₁Y₁, ∇W₁). Unlike the equation case above it
    // carries no '=' / eq-number and is not operator-dense — its cells are plain
    // variables. Signature: NO data values (a real data table has decimals or
    // multi-digit numbers; a matrix has only single-digit sub/superscripts), NO
    // real words, and a majority of cells are variable-like (start with a letter).
    // A numeric data table fails this (its cells start with digits and it has data
    // values), so it is unaffected.
    let dataval = grid
        .iter()
        .flatten()
        .filter(|c| {
            let b = c.as_bytes();
            (0..b.len()).any(|i| b[i].is_ascii_digit() && i + 2 < b.len() && b[i + 1] == b'.' && b[i + 2].is_ascii_digit())
                || c.chars().filter(|ch| ch.is_ascii_digit()).count() >= 3
        })
        .count();
    let letter_start = grid.iter().flatten().filter(|c| c.trim_start().chars().next().is_some_and(|ch| ch.is_alphabetic())).count();
    if nz >= 4 && dataval == 0 && alpha_words == 0 && letter_start * 2 >= nz {
        return false;
    }
    // Scattered symbolic DIAGRAM mis-detected as a table (e.g. a commutative
    // diagram: nodes X, Y, D, E with arrow labels ⟨(234)⟩ flung across the page).
    // Distinct from the matrix case above, which needs a letter-start majority —
    // a diagram is half bare digits, half symbols, so no axis dominates. Its tells
    // are instead: very LOW fill (nodes float in whitespace, unlike a real table
    // whose occupied columns are densely populated), NO numeric data values, almost
    // no real words, and either arrow/operator glyphs or short variable-like cells.
    // Gated on dataval == 0 so no numeric data table can ever be hit.
    // Commutative DIAGRAM mis-detected as a table (nodes X, Y, D, E with morphism
    // labels ⟨(234)⟩ scattered across the page — or, once the left-x fallback merges
    // them, a degenerate 2-column block). The tell is a category-theory
    // arrow/morphism glyph (→ ↦ ⟨ ⟩ …), which essentially never appears in tabular
    // DATA, in a grid that is NOT word-dominated. A word-heavy table (a state- or
    // reaction-transition table whose cells are real labels — conv → relu → pool)
    // survives via the alpha-word gate, so numeric/label tables are unaffected.
    let diagram_glyph = grid.iter().flatten().any(|c| c.chars().any(|ch| "→←↔⇒⇐↦⟨⟩∘↪↩⟶⟵↠↣".contains(ch)));
    // A real DATA table is full of decimal values (319.61, 0.446); a commutative
    // diagram has none — its numbers are bare node indices. Require decimal-absence
    // so a numeric table that merely uses an arrow in a header (input → output) is
    // never mistaken for a diagram.
    let has_decimal = grid.iter().flatten().any(|c| {
        let b = c.as_bytes();
        (0..b.len()).any(|i| b[i].is_ascii_digit() && i + 2 < b.len() && b[i + 1] == b'.' && b[i + 2].is_ascii_digit())
    });
    if nz >= 6 && diagram_glyph && alpha_words * 3 <= nz && !has_decimal {
        return false;
    }
    true
}

/// Detect tables: runs of >=3 consecutive rows that each have >=2 gutter-separated
/// cells and share >=2 columns occupied in a majority of rows. This rejects
/// word-positioned prose (whose words merge into a single cell).
/// A detected table with its vertical extent (PDF user space, y increases up).
#[derive(Clone)]
pub struct PosTable {
    pub y_top: f32,
    pub y_bottom: f32,
    pub x_left: f32,
    pub x_right: f32,
    pub grid: Vec<Vec<String>>,
    /// Grouped/multi-level HEADER rows mapped onto the data column grid, each cell as
    /// (text, colspan): a header cell spanning several data columns ("Masking Rates"
    /// over MASK/SAME/RND) carries colspan>1; cells over one column carry colspan 1.
    /// Empty when the table has no detached header (the data grid's row 0 is the header).
    pub header: Vec<Vec<(String, usize)>>,
}

fn clone_span(s: &Span) -> Span {
    Span {
        x: s.x,
        y: s.y,
        size: s.size,
        width: s.width,
        text: s.text.clone(),
        bold: s.bold,
        italic: s.italic,
        mono: s.mono,
        angle: s.angle,
        font: s.font,
    }
}

/// The x of the central gutter when the page is a two-column layout, else None.
///
/// A two-column page is split down the middle by a vertical whitespace lane that
/// is empty across (almost) every text row — a handful of full-width lines (a
/// title, a banner) are tolerated. Crucially, when such a clean centre gutter
/// exists there is, by definition, *no* full-width element crossing it, so the
/// caller can treat each side completely independently. A page where a wide
/// element (a spanning figure/table) sits across the centre has no clean gutter,
/// returns None, and is handled whole.
pub(crate) fn central_gutter(spans: &[Span]) -> Option<f32> {
    let rows = rows_of(spans.iter().map(clone_span).collect());
    if rows.len() < 6 {
        return None;
    }
    let span_r = |s: &Span| (s.x, s.x + s.width.max(s.size * 0.3));
    let x0 = spans.iter().map(|s| s.x).fold(f32::INFINITY, f32::min);
    let x1 = spans.iter().map(|s| span_r(s).1).fold(f32::NEG_INFINITY, f32::max);
    let width = x1 - x0;
    if !(width > 1.0) {
        return None;
    }
    // Scan the central band for the x crossed by the fewest rows.
    let (lo, hi) = (x0 + width * 0.30, x0 + width * 0.70);
    let step = (width / 200.0).max(1.0);
    let row_clear = |x: f32| rows.iter().filter(|r| !r.iter().any(|s| { let (a, b) = span_r(s); a <= x && x <= b })).count();
    let mut best = (0usize, lo);
    let mut x = lo;
    while x <= hi {
        let c = row_clear(x);
        if c > best.0 {
            best = (c, x);
        }
        x += step;
    }
    // A two-column LAYOUT has wide wrapping PROSE on both sides: each side's line is
    // a SINGLE wide cell (>=4 words, spanning most of its half-width). A table half
    // has >=2 cells (its own columns), so a table's internal gutter — even one near
    // the centre — is never mistaken for a page split. Require >=3 prose lines/side.
    let g = best.1;
    let prose_lines = |left: bool, min_w: f32| {
        rows.iter()
            .filter(|r| {
                let side: Vec<Span> = r.iter().filter(|s| (s.x + s.width.max(0.0) * 0.5 < g) == left).map(clone_span).collect();
                let cells = row_cells(&side);
                cells.len() == 1
                    && cells[0].text.split_whitespace().count() >= 4
                    && (cells[0].end - cells[0].x) > min_w
            })
            .count()
    };
    if best.0 as f32 >= rows.len() as f32 * 0.88
        && prose_lines(true, (g - x0) * 0.5) >= 3
        && prose_lines(false, (x1 - g) * 0.5) >= 3
    {
        Some(g)
    } else {
        None
    }
}

/// Detect tables. On a two-column page we split down the middle and detect each
/// side independently — a clean centre gutter guarantees nothing spans it, so the
/// two sides are genuinely separate (this is what stops adjacent-column prose from
/// merging into a phantom wide table). Otherwise (single column, or a full-width
/// element across the centre) the whole page is one region.
pub fn detect_tables_pos(spans: &[Span]) -> Vec<PosTable> {
    // Tables are built from upright text only — rotated labels (axis titles etc.) must
    // not perturb gutter detection or column structure (they're figure labels).
    let upright: Vec<Span> = spans.iter().filter(|s| s.angle.abs() < 0.01).map(clone_span).collect();
    let spans = &upright[..];
    match central_gutter(spans) {
        None => detect_tables_region(spans),
        Some(g) => {
            // Split down the gutter and detect each side independently (this is what
            // stops adjacent-column prose from merging into a phantom wide table).
            let side = |left: bool| -> Vec<Span> {
                spans.iter().filter(|s| (s.x + s.width.max(0.0) * 0.5 < g) == left).map(clone_span).collect()
            };
            let lt = detect_tables_region(&side(true));
            let rt = detect_tables_region(&side(false));
            // A full-width table (e.g. BERT's GLUE table) was split into a left half
            // and a right half that occupy the SAME rows. Detect that: a left-side
            // table whose vertical extent overlaps a right-side table is one table cut
            // in two. Re-detect across the FULL width within just that vertical band
            // (prose outside the band can't interfere) to recover the whole table. A
            // single-column table beside prose has no mate (prose isn't a table), so
            // it is kept as-is — no cross-column bleed.
            let overlaps = |a: &PosTable, b: &PosTable| {
                let lo = a.y_bottom.max(b.y_bottom);
                let hi = a.y_top.min(b.y_top);
                let span = (a.y_top - a.y_bottom).min(b.y_top - b.y_bottom).max(1.0);
                (hi - lo) >= span * 0.5
            };
            let mut out: Vec<PosTable> = Vec::new();
            let mut used_r = vec![false; rt.len()];
            for l in &lt {
                match rt.iter().enumerate().find(|(j, r)| !used_r[*j] && overlaps(l, r)) {
                    Some((j, r)) => {
                        used_r[j] = true;
                        let (yb, yt) = (l.y_bottom.min(r.y_bottom), l.y_top.max(r.y_top));
                        let pad = 2.0;
                        let band: Vec<Span> =
                            spans.iter().filter(|s| s.y >= yb - pad && s.y <= yt + pad).map(clone_span).collect();
                        let merged = detect_tables_region(&band);
                        if merged.is_empty() {
                            out.push(l.clone());
                            out.push(r.clone());
                        } else {
                            out.extend(merged);
                        }
                    }
                    None => out.push(l.clone()),
                }
            }
            for (j, r) in rt.into_iter().enumerate() {
                if !used_r[j] {
                    out.push(r);
                }
            }
            out
        }
    }
}

/// Detect tables within a single region (one text column, or the whole page):
/// runs of >=3 consecutive multi-cell rows sharing >=2 aligned columns (occupied
/// in a majority of rows). Rejects word-positioned prose (words merge to a cell).
fn detect_tables_region(spans: &[Span]) -> Vec<PosTable> {
    let avg_size = if spans.is_empty() {
        10.0
    } else {
        spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32
    };
    let tol = (avg_size * 1.5).max(6.0);
    let rows = rows_of(spans.iter().map(clone_span).collect());
    let mut celled: Vec<(f32, Vec<Cell>, Vec<Span>)> = rows
        .iter()
        .map(|r| (r.first().map(|s| s.y).unwrap_or(0.0), row_cells(r), r.iter().map(clone_span).collect()))
        .collect();

    // Coalesce wrapped multi-line cells. A borderless table with a long column (e.g.
    // a "Description" that wraps) emits its overflow lines as rows holding only that
    // one interior cell. Those 1-cell rows would otherwise break the multi-cell row
    // run and the table would be missed (and its bare ruling leak out as a figure).
    // Fold each such overflow line into the nearest multi-cell row whose columns
    // include it, so the wrapped cell stays a single cell and the run is contiguous.
    {
        let anchors: Vec<usize> = (0..celled.len()).filter(|&i| celled[i].1.len() >= 2).collect();
        if anchors.len() >= 2 {
            // Left edge of the table body: a genuine row label starts here; an overflow
            // line of a wrapped *interior* cell does not, which is how we tell them apart.
            let region_min_x = anchors
                .iter()
                .map(|&i| celled[i].1.iter().map(|c| c.x).fold(f32::INFINITY, f32::min))
                .fold(f32::INFINITY, f32::min);
            let mut absorb: Vec<(usize, usize)> = Vec::new(); // (anchor, overflow-row)
            for ti in 0..celled.len() {
                if celled[ti].1.len() != 1 {
                    continue;
                }
                let cx = celled[ti].1[0].x;
                if cx <= region_min_x + tol {
                    continue; // sits at the left edge -> a row label / prose line, not overflow
                }
                let mut best: Option<(usize, f32)> = None;
                for &ai in &anchors {
                    let dy = (celled[ai].0 - celled[ti].0).abs();
                    if dy > avg_size * 1.8 {
                        continue; // not vertically adjacent -> not the same wrapped cell
                    }
                    if !celled[ai].1.iter().any(|c| (c.x - cx).abs() <= tol) {
                        continue; // overflow x must line up with one of the anchor's columns
                    }
                    if best.is_none_or(|(_, bd)| dy < bd) {
                        best = Some((ai, dy));
                    }
                }
                if let Some((ai, _)) = best {
                    absorb.push((ai, ti));
                }
            }
            let mut drop = vec![false; celled.len()];
            for (ai, ti) in absorb {
                let mut moved = std::mem::take(&mut celled[ti].2);
                drop[ti] = true;
                celled[ai].2.append(&mut moved);
            }
            if drop.iter().any(|&d| d) {
                let mut kept: Vec<(f32, Vec<Cell>, Vec<Span>)> = Vec::new();
                for (i, mut row) in celled.into_iter().enumerate() {
                    if drop[i] {
                        continue;
                    }
                    // Reading order within a merged cell is top-to-bottom: sort the row's
                    // spans by descending y (then x) so bin_row accumulates the wrapped
                    // lines in order. The anchor's cell list (row.1) is left untouched —
                    // the overflow lands in an existing column, so the column structure
                    // and x-extent are unchanged.
                    row.2.sort_by(|a, b| {
                        b.y.partial_cmp(&a.y)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
                    });
                    kept.push(row);
                }
                celled = kept;
            }
        }
    }

    let mut tables = Vec::new();

    let flush = |run: &Vec<&(f32, Vec<Cell>, Vec<Span>)>, headers: &[&(f32, Vec<Cell>, Vec<Span>)], tables: &mut Vec<PosTable>| {
        if run.len() < 3 {
            return;
        }
        let owned: Vec<Vec<Cell>> = run
            .iter()
            .map(|(_, c, _)| c.iter().map(|x| Cell { x: x.x, end: x.end, text: x.text.clone() }).collect())
            .collect();
        // Region x-extent of the DATA.
        let (mut x_left, mut x_right) = (f32::INFINITY, f32::NEG_INFINITY);
        for row in &owned {
            for c in row {
                x_left = x_left.min(c.x);
                x_right = x_right.max(c.end);
            }
        }
        // Build a grid from the RAW SPANS for a candidate set of kept columns (expressed as
        // x-bands) by assigning each span to the band containing its CENTRE, then ADMIT it
        // (prose/equation/matrix reject). Returns (grid, kept left-x anchors) iff admitted.
        // `min_fill` is the minimum fraction of non-empty grid cells required to ACCEPT —
        // 0 for the band model (its header-named keep legitimately produces sparse wide
        // tables), but raised for the left-x fallback so a sparse symbol SCATTER (a
        // commutative diagram, a math array) isn't clustered into a spurious table.
        let try_model = |kept: Vec<(f32, f32)>, min_fill: f32| -> Option<(Vec<Vec<String>>, Vec<f32>)> {
            if kept.len() < 2 {
                return None;
            }
            let grid: Vec<Vec<String>> = run
                .iter()
                .map(|(_, _, spans)| {
                    let mut cells = vec![String::new(); kept.len()];
                    // Where each cell's text currently ENDS, so the space decision below can
                    // see the gap. This is the grid a consumer actually reads, and it spaced
                    // every appended span unconditionally: on a generator that emits one `Tj`
                    // per glyph — every SEC filing in the corpus — `Texas` came out `T e x a s`.
                    let mut ends = vec![f32::NEG_INFINITY; kept.len()];
                    for s in spans {
                        let txt = s.text.trim();
                        if txt.is_empty() {
                            continue;
                        }
                        let w = if s.width > 0.1 { s.width } else { txt.chars().count() as f32 * s.size * 0.5 };
                        if let Some(k) = band_of(&kept, s.x + w * 0.5) {
                            if !cells[k].is_empty()
                                && !crate::textutil::glyph_adjacent(s.x - ends[k], s.size)
                                && join_space(&cells[k], txt)
                            {
                                cells[k].push(' ');
                            }
                            cells[k].push_str(txt);
                            ends[k] = s.x + w;
                        }
                    }
                    cells
                })
                .collect();
            if min_fill > 0.0 {
                let total = grid.len() * kept.len();
                let filled = grid.iter().flatten().filter(|c| !c.trim().is_empty()).count();
                if total == 0 || (filled as f32) < min_fill * total as f32 {
                    return None;
                }
            }
            if is_coherent_grid(&grid) {
                Some((grid, kept.iter().map(|b| b.0).collect()))
            } else {
                None
            }
        };
        // PASS 1a (PRIMARY) — whitespace-lane band columns: keys on where text SITS, so
        // right-aligned numerics stay distinct and a header-named sparse column survives.
        let band_kept: Vec<(f32, f32)> = {
            let owned_slices: Vec<&[Cell]> = owned.iter().map(|r| r.as_slice()).collect();
            let bands = column_bands(&owned_slices, 0);
            if bands.len() < 2 {
                Vec::new()
            } else {
                let center = |c: &Cell| (c.x + c.end) * 0.5;
                let mut occ = vec![0usize; bands.len()];
                for row in &owned {
                    let mut hit = vec![false; bands.len()];
                    for c in row {
                        if let Some(bi) = band_of(&bands, center(c)) {
                            hit[bi] = true;
                        }
                    }
                    for (i, &h) in hit.iter().enumerate() {
                        if h {
                            occ[i] += 1;
                        }
                    }
                }
                // A band is NAMED when a header cell (a stranded header row, or the run's own
                // first row) overlaps it by ≥0.35 of its width — header-named bands survive
                // even when the body rarely fills them (wide sparse tables).
                let hdr_src: Vec<&Cell> = headers.iter().flat_map(|hr| hr.1.iter()).chain(owned.first().into_iter().flat_map(|r| r.iter())).collect();
                let body_rows = owned.len();
                (0..bands.len())
                    .filter(|&i| {
                        let (lo, hi) = bands[i];
                        let w = hi - lo;
                        let named = w > 0.0 && hdr_src.iter().any(|c| c.end.min(hi) - c.x.max(lo) > 0.35 * w);
                        occ[i] * 2 >= body_rows || named
                    })
                    .map(|i| bands[i])
                    .collect()
            }
        };
        // PASS 1b (FALLBACK) — left-x clustering, as bands [anchor_k, anchor_{k+1}). Recovers
        // wide-first-column tables the lane model over-merges (Transformer Table 1), where a
        // long row label bridges a lane and collapses the band grid to <2 columns.
        let leftx_kept = || -> Vec<(f32, f32)> {
            let cols = columns(&owned, tol);
            if cols.len() < 2 {
                return Vec::new();
            }
            let mut occ = vec![0usize; cols.len()];
            for row in &owned {
                for c in row {
                    if let Some(ci) = nearest_col(&cols, c.x) {
                        occ[ci] += 1;
                    }
                }
            }
            let keep: Vec<usize> = (0..cols.len()).filter(|&i| occ[i] * 2 >= owned.len()).collect();
            keep.iter()
                .enumerate()
                .map(|(j, &k)| (cols[k], keep.get(j + 1).map(|&nk| cols[nk]).unwrap_or(x_right + tol * 0.5)))
                .collect()
        };
        // Band model first; on its failure (degenerate or rejected) fall back to left-x,
        // which must clear a density bar (≥0.5 filled) so a sparse math scatter the band
        // model correctly rejected isn't resurrected as a spurious table.
        let (grid, kept_x) = match try_model(band_kept, 0.0).or_else(|| try_model(leftx_kept(), 0.5)) {
            Some(gx) => gx,
            None => return,
        };

        // Now that the data table is ACCEPTED (past every prose/equation guard), attach
        // the grouped/multi-level HEADER rows the run-builder skipped — they don't form
        // uniform >=2-cell rows, so they were stranded above the data and leaked into the
        // prose. Map each header cell onto the SINGLE data-column grid: the data columns
        // its x-span covers become one cell with colspan = #covered (a label centred over
        // several columns merges); a cell over one column gets colspan 1; uncovered
        // columns become empty cells. Only rows horizontally overlapping the table count.
        let ncols = kept_x.len();
        let m = tol * 0.5;
        let mut y_top = run.first().map(|(y, _, _)| *y).unwrap_or(0.0);
        let mut header: Vec<Vec<(String, usize)>> = Vec::new();
        for hr in headers.iter() {
            let (hx0, hx1) = hr.1.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), c| (a.min(c.x), b.max(c.end)));
            if hx1 < x_left - m || hx0 > x_right + m {
                continue; // not above this table's columns
            }
            let mut slots: Vec<Option<(String, usize)>> = vec![None; ncols];
            let mut owner: Vec<Option<usize>> = vec![None; ncols]; // column → slot holding its text
            // Each data column k owns the x-region [band_k.lo, band_{k+1}.lo) (last → x_right).
            // A header cell covers column k when its x-span overlaps that region by a
            // MEANINGFUL fraction of the column width — so a group label centred over
            // several columns spans them all, while a label that merely grazes a
            // neighbouring column by a few points (e.g. "MNLI" starting 3pt inside the
            // previous column) is NOT pulled into it.
            let col_hi = |k: usize| if k + 1 < ncols { kept_x[k + 1] } else { x_right + m };
            for c in &hr.1 {
                let txt = c.text.trim();
                if txt.is_empty() {
                    continue;
                }
                let covered: Vec<usize> = (0..ncols)
                    .filter(|&k| {
                        let w = col_hi(k) - kept_x[k];
                        let overlap = c.end.min(col_hi(k)) - c.x.max(kept_x[k]);
                        w > 0.0 && overlap > 0.35 * w
                    })
                    .collect();
                let (a, span) = match (covered.first(), covered.last()) {
                    (Some(&f), Some(&l)) => (f, l - f + 1),
                    _ => {
                        // grazes no column centre — pin to the nearest by left edge
                        let k = (0..ncols)
                            .min_by(|&i, &j| (kept_x[i] - c.x).abs().partial_cmp(&(kept_x[j] - c.x).abs()).unwrap_or(std::cmp::Ordering::Equal))
                            .unwrap_or(0);
                        (k, 1)
                    }
                };
                match owner[a] {
                    Some(o) => {
                        // collision: append to whichever slot actually holds the text
                        if let Some((t, _)) = slots[o].as_mut() {
                            t.push(' ');
                            t.push_str(txt);
                        }
                    }
                    None => {
                        slots[a] = Some((txt.to_string(), span));
                        for k in a..(a + span).min(ncols) {
                            owner[k] = Some(a);
                        }
                    }
                }
            }
            // Emit cells in column order, honouring spans (skip columns a spanned cell ate).
            let mut hrow: Vec<(String, usize)> = Vec::new();
            let mut k = 0;
            while k < ncols {
                match slots[k].take() {
                    Some((t, sp)) => {
                        hrow.push((t, sp));
                        k += sp.max(1);
                    }
                    None => {
                        hrow.push((String::new(), 1));
                        k += 1;
                    }
                }
            }
            header.push(hrow);
            y_top = y_top.max(hr.0);
        }
        tables.push(PosTable {
            y_top,
            y_bottom: run.last().map(|(y, _, _)| *y).unwrap_or(0.0),
            x_left,
            x_right,
            grid,
            header,
        });
    };

    let n = celled.len();
    let mut i = 0;
    while i < n {
        if celled[i].1.len() < 2 {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && celled[i].1.len() >= 2 {
            i += 1;
        }
        // Walk upward over contiguous HEADER-like rows immediately above the data run:
        // tightly spaced, short, and not prose (no long sentence cell). Stops at a gap
        // or a prose line, so it captures stranded grouped-header rows without eating
        // body text above the table.
        let mut h = start;
        while h > 0 {
            let cand = &celled[h - 1];
            let gap = cand.0 - celled[h].0; // y increases up; cand sits above
            let words: usize = cand.1.iter().map(|c| c.text.split_whitespace().count()).sum();
            let prose_cell = cand.1.iter().any(|c| c.text.split_whitespace().count() > 5 && c.text.trim_end().ends_with('.'));
            if gap > 0.0 && gap < avg_size * 2.2 && words <= 8 && !prose_cell {
                h -= 1;
            } else {
                break;
            }
        }
        let headers: Vec<&(f32, Vec<Cell>, Vec<Span>)> = celled[h..start].iter().collect();
        let run_slice: Vec<&(f32, Vec<Cell>, Vec<Span>)> = celled[start..i].iter().collect();
        flush(&run_slice, &headers, &mut tables);
    }
    tables
}

fn detect_tables(spans: Vec<Span>) -> Vec<Vec<Vec<String>>> {
    detect_tables_pos(&spans).into_iter().map(|t| t.grid).collect()
}


/// Extract tables from all pages as owned [`TableInfo`] rows (row-major grids).
pub fn extract_tables(doc: &Document, raw: &[u8]) -> Vec<TableInfo> {
    let mut out = Vec::new();
    for (&pno, &page_id) in &doc.get_pages() {
        let spans = text::extract_spans(doc, page_id, raw);
        for grid in detect_tables(spans) {
            out.push(TableInfo { page: pno, cells: grid });
        }
    }
    out
}

/// Does this font dict (or its descendant) carry an embedded font program?
fn font_embedded(doc: &Document, dict: &Dictionary) -> bool {
    // Type0: descriptor lives on the descendant font.
    let descriptor = dict
        .get(b"FontDescriptor")
        .ok()
        .and_then(|o| deref(doc, o))
        .or_else(|| {
            dict.get(b"DescendantFonts")
                .ok()
                .and_then(|o| deref(doc, o))
                .and_then(|o| o.as_array().ok())
                .and_then(|a| a.first())
                .and_then(|o| deref(doc, o))
                .and_then(|o| o.as_dict().ok())
                .and_then(|dd| dd.get(b"FontDescriptor").ok())
                .and_then(|o| deref(doc, o))
        });
    match descriptor.and_then(|o| o.as_dict().ok()) {
        Some(d) => {
            d.has(b"FontFile") || d.has(b"FontFile2") || d.has(b"FontFile3")
        }
        None => false,
    }
}

/// Extract per-page font info as owned [`FontInfo`] rows: `{page, name, subtype, base_font,
/// encoding, embedded, has_tounicode}`.
///
/// Enumerates `/Font` dictionaries through [`page_resource_dicts`], so a font used only by
/// text inside a Form XObject is reported. lopdf's `get_page_fonts()` reads the page's own
/// (and inherited) `/Resources` and stops there: an astro-ph preprint in the corpus whose
/// page `/Resources` carries an empty `/Font <<>>` and puts all content in `/TPL*` forms
/// returned zero rows for all 166 pages, and ~22 of 54 documents missed fonts partially.
///
/// **Annotation appearance streams are deliberately not enumerated here**, unlike
/// [`extract_images`]. The reference this pillar is measured against does not report them
/// either: `fw9_form.pdf` in the corpus has eight `/Widget` annotations whose appearance
/// streams set `/ZaDb` (ZapfDingbats, the checkbox tick) from their own `/Resources`, and
/// pymupdf's `get_fonts()` returns the same six page fonts we do — not seven. A font a
/// widget uses to draw its own tick is a property of the form field, not of the page's
/// text, and adding it would put this pillar *ahead* of the parity target rather than at
/// it. [`appearance_resource_dicts`] exists and is one call away if that verdict changes.
pub fn extract_fonts(doc: &Document) -> Vec<FontInfo> {
    let mut out = Vec::new();
    for (&pno, &page_id) in &doc.get_pages() {
        // De-duplicated per page by (resource name, font object id): one font shared by
        // several forms is one row, while the same name bound to different objects in the
        // page and in a form is two. `BTreeMap` keeps rows in resource-name order, which
        // is the order the non-recursive accessor produced them in.
        let mut fonts: BTreeMap<(Vec<u8>, Option<ObjectId>), &Dictionary> = BTreeMap::new();
        for res in page_resource_dicts(doc, page_id) {
            // A form's fonts live in its OWN /Resources (PDF 32000-1 §8.10.2) — the same
            // rule text.rs:1213 follows when it decodes a form's content.
            let Some(fdict) = sub_dict(doc, res, b"Font") else {
                continue;
            };
            for (name, v) in fdict.iter() {
                let Some(dict) = deref(doc, v).and_then(|o| o.as_dict().ok()) else {
                    continue;
                };
                fonts.entry((name.clone(), v.as_reference().ok())).or_insert(dict);
            }
        }
        for ((name, _), dict) in fonts {
            let subtype = dict
                .get(b"Subtype")
                .and_then(|o| o.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            let base_font = dict
                .get(b"BaseFont")
                .and_then(|o| o.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            let encoding = dict
                .get(b"Encoding")
                .ok()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_else(|| "custom".to_string());
            out.push(FontInfo {
                page: pno,
                name: String::from_utf8_lossy(&name).into_owned(),
                subtype,
                base_font,
                encoding,
                embedded: font_embedded(doc, dict),
                has_tounicode: dict.has(b"ToUnicode"),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owned form-XObject raster fixture (`tests/gen_fixtures.py::gen_form_image`).
    fn form_image_doc() -> Document {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_image.pdf");
        Document::load(path).expect("form_image.pdf fixture must load")
    }

    #[test]
    fn a_table_drawn_one_glyph_per_tj_reads_as_words_not_spaced_letters() {
        // `tests/gen_fixtures.py::gen_glyph_table`. Both cell builders — `row_cells`, which
        // feeds column detection, and `try_model`, which builds the grid a consumer reads —
        // spaced EVERY appended span, with no gap test at all. That is right for the
        // word-level spans they were written against and shredding for a generator that
        // emits one `Tj` per glyph: `Texas` came out of a `<th>` as `T e x a s`, in 266 of
        // the local corpus's 632 detected tables. Every table-quality signal that reads cell
        // text — word counts, prose detection — was reading that.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/glyph_table.pdf");
        let doc = Document::load(path).expect("glyph_table.pdf fixture must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let page = *doc.get_pages().get(&1).expect("page 1");
        let spans = crate::text::extract_spans(&doc, page, &raw);
        let tables = detect_tables_pos(&spans);
        assert_eq!(tables.len(), 1, "one table, got {}", tables.len());
        let want = [
            ["Region", "Samples", "Depth"],
            ["North", "128", "42.5"],
            ["South", "96", "31.0"],
            ["East ridge", "77", "18.2"],
        ];
        let got: Vec<Vec<String>> = tables[0].grid.iter().map(|r| r.iter().map(|c| c.trim().to_string()).collect()).collect();
        assert_eq!(got.len(), want.len(), "row count: {got:?}");
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g.as_slice(), w.map(String::from).as_slice(), "grid: {got:?}");
        }
    }

    /// A row reduced to everything a caller can observe, so two runs can be compared.
    fn image_rows(rows: &[ImageInfo]) -> Vec<(u32, usize, i64, i64, Option<String>, Option<i64>, &'static str, usize, u64)> {
        rows.iter()
            .map(|i| {
                // A cheap order-sensitive checksum over the payload: comparing the bytes
                // themselves would make the failure message unreadable.
                let sum = i.data.iter().fold(1469598103934665603u64, |h, &b| (h ^ b as u64).wrapping_mul(1099511628211));
                (i.page, i.index, i.width, i.height, i.color_space.clone(), i.bits_per_component, i.format, i.data.len(), sum)
            })
            .collect()
    }

    #[test]
    fn the_short_circuit_reports_exactly_what_the_full_walk_reports() {
        // `extract_images` skips the content walk on a page whose resource tree reaches no
        // image XObject. The claim is that this is unobservable, not merely usually right —
        // so assert it against the full-walk oracle over EVERY committed fixture, which
        // spans the cases that could break it: pages with no images at all, images nested in
        // form XObjects, images reachable but never drawn (`undrawn_image.pdf`), images that
        // exist only in an annotation appearance (`annot_appearance.pdf`), cyclic and
        // repeated forms, and the adversarial form bomb.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf");
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for d in [std::path::PathBuf::from(dir), std::path::Path::new(dir).join("adversarial")] {
            let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(&d)
                .expect("fixture dir readable")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf")))
                .collect();
            found.sort();
            paths.append(&mut found);
        }
        assert!(paths.len() > 40, "expected the full fixture corpus, got {}", paths.len());
        let mut with_images = 0usize;
        for p in &paths {
            let Ok(doc) = Document::load(p) else { continue }; // encrypted / deliberately damaged
            let short = extract_images_inner(&doc, true);
            let full = extract_images_inner(&doc, false);
            assert_eq!(image_rows(&short), image_rows(&full), "short-circuit changed the rows of {}", p.display());
            if !short.is_empty() {
                with_images += 1;
            }
        }
        assert!(with_images >= 10, "the comparison must actually cover image-bearing docs, got {with_images}");
    }

    #[test]
    fn images_nested_in_a_form_xobject_are_found() {
        // The page's own /XObject holds only the form; the image is in the FORM's
        // /Resources. lopdf's get_page_images() stops at the page, so this returned
        // nothing — 13 of 54 corpus documents reported no images at all.
        let doc = form_image_doc();
        let rows = extract_images(&doc);
        let page1: Vec<&ImageInfo> = rows.iter().filter(|i| i.page == 1).collect();
        assert_eq!(page1.len(), 1, "exactly one form-nested image on page 1");
        assert_eq!((page1[0].width, page1[0].height), (240, 160));
        assert_eq!(page1[0].index, 0);
        assert_eq!(page1[0].color_space.as_deref(), Some("DeviceRGB"));
        assert!(!page1[0].data.is_empty());
    }

    #[test]
    fn direct_images_keep_their_index_and_nested_ones_are_appended() {
        // The ordering contract: recursing must not renumber images that were already
        // reported. Page 2 draws one raster directly and a second inside a form.
        let doc = form_image_doc();
        let rows = extract_images(&doc);
        let page2: Vec<&ImageInfo> = rows.iter().filter(|i| i.page == 2).collect();
        assert_eq!(page2.len(), 2);
        assert_eq!((page2[0].index, page2[0].width, page2[0].height), (0, 120, 90), "direct image keeps index 0");
        assert_eq!((page2[1].index, page2[1].width, page2[1].height), (1, 240, 160), "nested image appended");
    }

    /// A hand-built document whose form XObject lists ITSELF in its own `/Resources`
    /// `/XObject` dict — the cycle a naive recursion follows forever.
    fn cyclic_form_doc() -> (Document, ObjectId) {
        use lopdf::{dictionary, Stream};
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 7i64, "Height" => 5i64,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8i64,
            },
            vec![0u8; 35],
        ));
        let form_id = doc.new_object_id();
        let inner_id = doc.new_object_id();
        // form -> inner -> form: a two-step cycle, plus a direct self-reference.
        doc.set_object(
            form_id,
            Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Form", "FormType" => 1i64,
                    "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                    "Resources" => dictionary! {
                        "XObject" => dictionary! {
                            "Fm0" => form_id, "Fm1" => inner_id, "Im0" => img_id,
                        },
                    },
                },
                // Invokes ITSELF and the inner form (which invokes it back) before painting
                // the image: the cycle is in the content stream, not just the resource tree.
                b"q /Fm0 Do /Fm1 Do /Im0 Do Q".to_vec(),
            ),
        );
        doc.set_object(
            inner_id,
            Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Form", "FormType" => 1i64,
                    "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                    "Resources" => dictionary! {
                        "XObject" => dictionary! { "Fm0" => form_id },
                    },
                },
                b"q /Fm0 Do Q".to_vec(),
            ),
        );
        let pages_id = doc.new_object_id();
        let contents_id = doc.add_object(Stream::new(dictionary! {}, b"q /Fm0 Do Q".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Fm0" => form_id },
            },
            "Contents" => contents_id,
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages", "Count" => 1i64, "Kids" => vec![page_id.into()],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        (doc, page_id)
    }

    #[test]
    fn resource_walk_terminates_on_a_self_referential_form() {
        // Without the visited-ObjectId set this recurses until the depth cap (or forever,
        // for a mutual cycle). Each form's /Resources must be visited exactly once.
        let (doc, page_id) = cyclic_form_doc();
        let dicts = page_resource_dicts(&doc, page_id);
        assert_eq!(dicts.len(), 3, "page + the two form resource dicts, each once");

        // …and the image inside the cyclic form is still reported, exactly once.
        let rows = extract_images(&doc);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].page, rows[0].index, rows[0].width, rows[0].height), (1, 0, 7, 5));
    }

    #[test]
    fn images_listed_in_the_resources_but_never_drawn_are_not_reported() {
        // One /Resources dict shared by both pages through the /Pages node (the iText
        // layout a 166-page corpus preprint uses). Reachability alone made every page
        // reach every image in the document — 56,108 rows for 338 distinct images there.
        // A page's images are the ones its content stream paints.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/undrawn_image.pdf");
        let doc = Document::load(path).expect("undrawn_image.pdf fixture must load");
        let rows = extract_images(&doc);
        let dims: Vec<(u32, usize, i64, i64)> = rows.iter().map(|i| (i.page, i.index, i.width, i.height)).collect();
        // page 1 paints /ImDrawn (40x30) only; page 2 paints only the form, whose content
        // paints /ImInForm (42x32). /ImNever (41x31) and /ImFormNever (43x33) are listed
        // in the very same resource dictionaries and must not appear.
        assert_eq!(dims, vec![(1, 0, 40, 30), (2, 0, 42, 32)]);

        // The reachability walk still sees all four — the filter is the `Do` walk, not a
        // narrower resource tree (which extract_fonts shares).
        let page1 = *doc.get_pages().get(&1).expect("page 1");
        let reachable: usize = page_resource_dicts(&doc, page1)
            .iter()
            .filter_map(|r| sub_dict(&doc, r, b"XObject"))
            .map(|x| x.iter().count())
            .sum();
        assert_eq!(reachable, 5, "page + form resources list 3 + 2 XObject entries");
    }

    #[test]
    fn images_that_exist_only_in_an_annotation_appearance_are_reported() {
        // `/Annots -> /AP /N` is a content stream nothing in this crate used to walk, so an
        // image inside a stamp's or a widget's appearance was reported by nobody.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/annot_appearance.pdf");
        let doc = Document::load(path).expect("annot_appearance.pdf fixture must load");
        let dims: Vec<(u32, usize, i64, i64)> =
            extract_images(&doc).iter().map(|i| (i.page, i.index, i.width, i.height)).collect();
        assert_eq!(
            dims,
            vec![(1, 0, 40, 30), (1, 1, 10, 10), (1, 2, 12, 12), (1, 3, 15, 15), (1, 4, 16, 16)],
            "the page's own image keeps index 0; the appearances are appended after it"
        );
        // What must NOT be there, and why each would be a different bug:
        let sizes: Vec<i64> = extract_images(&doc).iter().map(|i| i.width).collect();
        assert!(!sizes.contains(&11), "11x11 sits in the appearance's /Resources but is never drawn");
        assert!(!sizes.contains(&13), "13x13 is the appearance state /AS did NOT select");
        assert!(!sizes.contains(&14), "14x14 belongs to a HIDDEN (/F bit 2) annotation");
    }

    #[test]
    fn appearance_stream_fonts_stay_out_of_the_font_report() {
        // The deliberate asymmetry with `extract_images`, pinned so it cannot drift: a
        // widget's own tick font is a property of the form field, and the parity reference
        // (pymupdf `get_fonts()`) does not report it either. `annot_appearance.pdf` has no
        // appearance fonts, so this checks the mechanism on the resource walk instead.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/annot_appearance.pdf");
        let doc = Document::load(path).expect("annot_appearance.pdf fixture must load");
        let page1 = *doc.get_pages().get(&1).expect("page 1");
        assert_eq!(
            page_resource_dicts(&doc, page1).len(),
            1,
            "extract_fonts sees the page's own /Resources only — no appearance dictionary"
        );
        assert_eq!(
            appearance_resource_dicts(&doc, page1).len(),
            4,
            "stamp + /AS-selected state + both un-selected-/AS states; the hidden annot contributes none"
        );
    }

    /// `tests/gen_fixtures.py::gen_colorspace_images` — Flate rasters in the four colour
    /// spaces whose resolution steps the reporter used to skip.
    fn colorspace_doc() -> Document {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/colorspace_images.pdf");
        Document::load(path).expect("colorspace_images.pdf fixture must load")
    }

    /// Decode an assembled PNG blob back to RGB8 — the caller's side of the contract.
    fn decode_png(data: &[u8]) -> image::RgbImage {
        image::load_from_memory_with_format(data, image::ImageFormat::Png)
            .expect("assembled bytes must be a readable PNG")
            .to_rgb8()
    }

    #[test]
    fn colorspaces_resolve_and_bits_per_component_is_reported() {
        // 971 of 2604 corpus rows reported `color_space: None`: an indirect /ColorSpace,
        // an ICC profile whose /N is the only component count, a palette, or a name that
        // only means something in the resource dictionary's /ColorSpace sub-dictionary.
        let doc = colorspace_doc();
        let rows = extract_images(&doc);
        let seen: Vec<(usize, Option<&str>, Option<i64>, &str)> = rows
            .iter()
            .map(|i| (i.index, i.color_space.as_deref(), i.bits_per_component, i.format))
            .collect();
        assert_eq!(
            seen,
            vec![
                (0, Some("Indexed"), Some(4), "png"),
                (1, Some("ICCBased"), Some(8), "png"), // /ColorSpace written as `9 0 R`
                (2, Some("DeviceCMYK"), Some(8), "png"),
                (3, Some("ICCBased"), Some(8), "png"), // /ColorSpace /CS0, a named resource
            ]
        );
    }

    #[test]
    fn raw_samples_are_assembled_into_a_readable_png() {
        // The bytes used to be the compressed samples with no container — nothing opened
        // them. Each assembled PNG must carry the authored pixels back.
        let doc = colorspace_doc();
        let rows = extract_images(&doc);

        // 4x2 @ 4bpc through a 4-entry palette: row 0 is red/green/blue/white, row 1 the
        // reverse — sub-byte unpacking AND the palette lookup, in one image.
        let ix = decode_png(&rows[0].data);
        assert_eq!(ix.dimensions(), (4, 2));
        assert_eq!(ix.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(ix.get_pixel(3, 0).0, [255, 255, 255]);
        assert_eq!(ix.get_pixel(0, 1).0, [255, 255, 255]);

        // ICCBased /N 3 -> three 8-bit samples per pixel.
        let icc = decode_png(&rows[1].data);
        assert_eq!(icc.dimensions(), (2, 2));
        assert_eq!(icc.get_pixel(0, 0).0, [10, 20, 30]);
        assert_eq!(icc.get_pixel(1, 1).0, [100, 110, 120]);

        // DeviceCMYK: (0,0,0,0) is white and (255,0,0,0) is pure cyan.
        let cmyk = decode_png(&rows[2].data);
        assert_eq!(cmyk.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(cmyk.get_pixel(1, 0).0, [0, 255, 255]);

        // ICCBased /N 1 reached through the named /CS0 resource -> grayscale.
        let gray = decode_png(&rows[3].data);
        assert_eq!(gray.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(gray.get_pixel(1, 0).0, [255, 255, 255]);
    }

    #[test]
    fn unfiltered_samples_assemble_too_and_metadata_is_complete() {
        // The hand-written undrawn_image fixture stores its rasters with NO filter at all
        // — lopdf errors on `decompressed_content()` there, which must not lose the row.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/undrawn_image.pdf");
        let doc = Document::load(path).expect("undrawn_image.pdf fixture must load");
        for r in extract_images(&doc) {
            assert_eq!(r.format, "png", "unfiltered DeviceRGB samples must assemble");
            assert_eq!(r.color_space.as_deref(), Some("DeviceRGB"));
            assert_eq!(r.bits_per_component, Some(8));
            let png = decode_png(&r.data);
            assert_eq!(png.dimensions(), (r.width as u32, r.height as u32));
        }
    }

    #[test]
    fn a_cmyk_jpeg_decodes_to_the_authored_colour_not_its_inverse() {
        // `tests/gen_fixtures.py::gen_cmyk_jpeg` — three flat CMYK bands behind an Adobe
        // APP14 marker and `/Decode [1 0 1 0 1 0 1 0]`, wrapped in ASCII85 by reportlab.
        // Handing the raw DCT stream back made every consumer read the complement: the
        // white band came out black. K is 0 in all three bands, so RGB is just 255 - ink.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/cmyk_jpeg.pdf");
        let doc = Document::load(path).expect("cmyk_jpeg.pdf fixture must load");
        let rows = extract_images(&doc);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].color_space.as_deref(), Some("DeviceCMYK"));
        assert_eq!(rows[0].format, "png", "a CMYK JPEG is normalized, not passed through");
        let img = decode_png(&rows[0].data);
        assert_eq!(img.dimensions(), (96, 48));
        for (x, want) in [(15u32, [255u8, 255, 255]), (47, [0, 255, 255]), (79, [255, 75, 255])] {
            let got = img.get_pixel(x, 24).0;
            let d = (0..3).map(|i| got[i].abs_diff(want[i])).max().unwrap();
            assert!(d <= 8, "band at x={x}: expected ~{want:?}, got {got:?}");
        }
    }

    #[test]
    fn a_truncated_sample_block_stays_raw_rather_than_fabricating_pixels() {
        // The honest fallback: a row we cannot assemble keeps `format:"raw"` AND the new
        // metadata, so the caller can still reassemble it by hand.
        use lopdf::{dictionary, Stream};
        let mut doc = Document::with_version("1.5");
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 8i64, "Height" => 8i64,
                "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8i64,
            },
            vec![7u8; 9], // 9 bytes where 8*8*3 are needed
        ));
        let pages_id = doc.new_object_id();
        let contents_id = doc.add_object(Stream::new(dictionary! {}, b"q /Im0 Do Q".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img_id } },
            "Contents" => contents_id,
        });
        doc.set_object(pages_id, dictionary! {
            "Type" => "Pages", "Count" => 1i64, "Kids" => vec![page_id.into()],
        });
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);

        let rows = extract_images(&doc);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].format, "raw");
        assert_eq!(rows[0].color_space.as_deref(), Some("DeviceRGB"));
        assert_eq!(rows[0].bits_per_component, Some(8));
        assert_eq!(rows[0].data, vec![7u8; 9], "the samples are still handed back verbatim");
    }

    #[test]
    fn fonts_nested_in_a_form_xobject_are_found_and_deduped() {
        // The hand-written fixture (`gen_fixtures.py::gen_form_font`) mirrors the corpus
        // astro-ph preprint: the page's own /Font dict is EMPTY and the only font lives in
        // a form's /Resources. That returned zero rows for all 166 of its pages. The form
        // is invoked under two names, so the row must appear exactly once.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_font.pdf");
        let doc = Document::load(path).expect("form_font.pdf fixture must load");
        let fonts = extract_fonts(&doc);
        assert_eq!(fonts.len(), 1, "expected one de-duplicated form-nested font, got {fonts:?}",
                   fonts = fonts.iter().map(|f| (f.page, f.name.clone())).collect::<Vec<_>>());
        let f = &fonts[0];
        assert_eq!(f.page, 1);
        assert_eq!(f.name, "FF1");
        assert_eq!(f.subtype, "Type1");
        assert_eq!(f.base_font, "Helvetica");
        assert_eq!(f.encoding, "WinAnsiEncoding");
        assert!(!f.embedded);
        assert!(!f.has_tounicode);
    }

    #[test]
    fn page_level_fonts_still_reported_in_resource_name_order() {
        // Regression guard on the rewire: the hand-written mathfonts fixture keeps all
        // four fonts in the page's own /Resources, so the walker must reproduce exactly
        // what the non-recursive accessor produced, in the same name order.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/mathfonts.pdf");
        let doc = Document::load(path).expect("mathfonts.pdf fixture must load");
        let names: Vec<String> = extract_fonts(&doc).into_iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["F1", "F2", "F3", "F4"]);
    }

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
    }

    #[test]
    fn numeric_data_table_is_coherent() {
        let g = grid(&[
            &["Region", "Q1", "Q2", "Q3"],
            &["North", "12.5", "13.1", "11.9"],
            &["South", "9.4", "10.2", "8.8"],
        ]);
        assert!(is_coherent_grid(&g));
    }

    #[test]
    fn prose_two_column_rejected() {
        // a glossary: short term + long wrapped definition (mean words/cell > 4 in 2 cols)
        let g = grid(&[
            &["alpha", "the first letter of the Greek alphabet used widely in mathematics"],
            &["beta", "the second letter often denoting a coefficient or a regression slope"],
            &["gamma", "the third letter frequently used for the Lorentz factor in physics"],
        ]);
        assert!(!is_coherent_grid(&g));
    }

    #[test]
    fn commutative_diagram_rejected() {
        // morphism glyphs, no decimal data, not word-dominated → a diagram, not a table
        let g = grid(&[
            &["X", "", "⟨ (234) ⟩", "", "⟨ (34) ⟩"],
            &["E", "1 P", "", "A 4", "Stab(1)"],
            &["", "x", "3 12", "", ""],
            &["2", "4", "", "", ""],
        ]);
        assert!(!is_coherent_grid(&g));
    }
}
