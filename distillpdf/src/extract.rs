//! Image, font and table extraction pillars, built on lopdf's object model.
//!
//! Pure Rust: these return plain owned structs. The PyO3 layer (`src/lib.rs`) assembles the
//! Python dicts/lists from them — no pyo3 types appear in this module.

use crate::pdfobj::filters_of;
use crate::raster::{assemble_png, codec_payload, filter_to_format, image_bpc, image_color_space, normalized_jpeg_png};
use crate::text::{self, Span};
use crate::{access::DictionaryHandle, walker::ResourceScope};
use lopdf::{Dictionary, Document, ObjectId};
use rayon::prelude::*;
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
fn page_resource_dicts(
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
) -> Vec<DictionaryHandle> {
    let mut queue: VecDeque<(DictionaryHandle, u32)> = VecDeque::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    // The adapter returns outermost → page for overlay consumers. Reporting has always
    // enumerated page → outermost, so reverse it to preserve every existing row index.
    for resources in access.page_resource_chain(page_id).unwrap_or_default().into_iter().rev() {
        queue.push_back((resources, 0));
    }
    resource_bfs(access, queue, &mut seen)
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
fn appearance_resource_dicts(
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
) -> Vec<DictionaryHandle> {
    let mut queue: VecDeque<(DictionaryHandle, u32)> = VecDeque::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    for (id, stream) in crate::walker::appearance_streams(access, page_id) {
        if !seen.insert(id) {
            continue; // one appearance stream shared by several annotations
        }
        // §12.5.5: an appearance stream's resources are its OWN — it inherits nothing from
        // the page, the same rule the nested-form step below applies.
        if let Ok(resources) = stream.dictionary_entry(access, b"Resources") {
            queue.push_back((resources, 0));
        }
    }
    resource_bfs(access, queue, &mut seen)
}

/// The shared body of both resource walks: drain `queue` breadth-first, appending each
/// dictionary and then queueing the own-`/Resources` of every `/Subtype /Form` XObject it
/// names. `seen` cuts reference cycles (and is pre-seeded by the caller with whatever it
/// has already visited); [`crate::MAX_FORM_DEPTH`] caps nesting.
fn resource_bfs(
    access: &dyn crate::access::DocumentAccess,
    mut queue: VecDeque<(DictionaryHandle, u32)>,
    seen: &mut HashSet<ObjectId>,
) -> Vec<DictionaryHandle> {
    let mut out = Vec::new();
    while let Some((res, depth)) = queue.pop_front() {
        if depth >= crate::MAX_FORM_DEPTH {
            out.push(res);
            continue; // nesting cap (a self-referential form is already cut by `seen`)
        }
        let _ = res.read(|resources| {
            let Ok(xobjects) = resources.get(b"XObject") else {
                return;
            };
            let _ = crate::access::read_resolved(access, xobjects, |xobjects| {
                let Ok(xobjects) = xobjects.as_dict() else {
                    return;
                };
                for (_, value) in xobjects.iter() {
                    let Ok(id) = value.as_reference() else {
                        continue;
                    };
                    if !seen.insert(id) {
                        continue;
                    }
                    let Ok(stream) = access.stream(id) else {
                        continue;
                    };
                    if stream
                        .read(|stream| crate::walker::has_subtype(stream, b"Form"))
                        .unwrap_or(false)
                    {
                        if let Ok(resources) = stream.dictionary_entry(access, b"Resources") {
                            queue.push_back((resources, depth + 1));
                        }
                    }
                }
            });
        });
        out.push(res);
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
    access: &dyn crate::access::DocumentAccess,
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
        let Some((id, stream)) = crate::walker::xobject_at(access, xmap, &op.operands) else {
            continue; // not a name, a dangling name, or not a stream: nothing to draw
        };
        if stream
            .read(|stream| crate::walker::has_subtype(stream, b"Image"))
            .unwrap_or(false)
        {
            out.insert(id);
        } else {
            if !stream
                .read(|stream| crate::walker::has_subtype(stream, b"Form"))
                .unwrap_or(false)
            {
                continue;
            }
            if crate::walker::too_deep(depth) || !seen.insert(id) {
                continue;
            }
            let Some(scope) = crate::walker::form_scope(
                access,
                &stream,
                xmap,
                crate::walker::ScopePolicy::OverlayParent,
            ) else {
                continue;
            };
            if let Some(ops) = stream.read(crate::walker::form_ops).flatten() {
                walk_drawn(doc, access, &ops, &scope.xobjects, depth + 1, seen, out);
            }
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
fn drawn_images(
    doc: &Document,
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
) -> Option<HashSet<ObjectId>> {
    let mut xmap = crate::walker::XMap::new();
    for resources in crate::walker::page_resource_chain(access, page_id) {
        let _ = resources.read(|dictionary| {
            crate::walker::overlay_xobjects(access, dictionary, &mut xmap)
        });
    }
    // lopdf 0.44 made `get_page_content` infallible (returns `Vec<u8>`, empty when the
    // page has no/unreadable content). The old `.ok()?` bailed out on Err; an empty
    // vec now decodes to zero operations, which reaches the same empty result.
    let content = access.page_content(page_id).ok()?;
    let ops = lopdf::content::Content::decode(&content).ok()?;
    let mut out = HashSet::new();
    let mut seen = HashSet::new();
    walk_drawn(doc, access, &ops.operations, &xmap, 0, &mut seen, &mut out);
    for (id, ap) in crate::walker::appearance_streams(access, page_id) {
        if !seen.insert(id) {
            continue; // shared between annotations, or already reached from the content
        }
        let Some(scope) = crate::walker::form_scope(
            access,
            &ap,
            &crate::walker::XMap::new(),
            crate::walker::ScopePolicy::OwnOnly,
        ) else {
            continue;
        };
        if let Some(ops) = ap.read(crate::walker::form_ops).flatten() {
            walk_drawn(doc, access, &ops, &scope.xobjects, 1, &mut seen, &mut out);
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
fn reaches_image_xobject(
    access: &dyn crate::access::DocumentAccess,
    dicts: &[DictionaryHandle],
) -> bool {
    dicts.iter().any(|resources| {
        resources
            .read(|resources| {
                let value = resources.get(b"XObject").ok()?;
                crate::access::read_resolved(access, value, |xobjects| {
                    let xobjects = xobjects.as_dict().ok()?;
                    Some(xobjects.iter().any(|(_, value)| {
                        value
                            .as_reference()
                            .ok()
                            .and_then(|id| access.stream(id).ok())
                            .and_then(|stream| {
                                stream.read(|stream| {
                                    crate::walker::has_subtype(stream, b"Image")
                                })
                            })
                            .unwrap_or(false)
                    }))
                })
                .ok()
                .flatten()
            })
            .ok()
            .flatten()
            .unwrap_or(false)
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
pub fn extract_images(
    doc: &Document,
    access: &dyn crate::access::DocumentAccess,
) -> Vec<ImageInfo> {
    extract_images_inner(doc, access, true)
}

/// The body of [`extract_images`], with the resource-tree short-circuit switchable so a test
/// can assert the two paths agree ([`tests::the_short_circuit_reports_exactly_what_the_full_walk_reports`]).
/// Production always passes `true`; `false` is the full-walk oracle.
fn extract_images_inner(
    doc: &Document,
    access: &dyn crate::access::DocumentAccess,
    short_circuit: bool,
) -> Vec<ImageInfo> {
    let mut out = Vec::new();
    for (&pno, &page_id) in &doc.get_pages() {
        // The page's own resource tree first, so every `(page, index)` a page already
        // reported keeps it; the annotation appearances are appended after.
        let mut dicts = page_resource_dicts(access, page_id);
        dicts.extend(appearance_resource_dicts(access, page_id));
        // A page whose resource tree reaches no image XObject cannot report one, because
        // enumeration below runs over exactly these dictionaries and `drawn` can only
        // *remove* candidates from it — so the content walk is pure cost. On a 102-page
        // regulation with zero images that walk was lexing 2.4 MB of content streams (77% of
        // the operation, in lopdf's lexer) to conclude nothing. Both dict walks above parse
        // no operator and decompress no stream, and the scan is the enumeration loop's own
        // predicate, so skipping is by construction unobservable — not an approximation.
        if short_circuit && !reaches_image_xobject(access, &dicts) {
            continue;
        }
        let mut index = 0usize;
        let drawn = drawn_images(doc, access, page_id);
        // Dedup is across resource dictionaries only: an image the page's own /XObject
        // already listed is not re-reported when a nested form points at it too. Repeats
        // *within* one dictionary are kept, so the `index` a directly-referenced image had
        // before this walk existed is unchanged.
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut from_this_dict: Vec<ObjectId> = Vec::new();
        for res in dicts {
            seen.extend(from_this_dict.drain(..));
            let scope = ResourceScope::own(res.clone());
            let _ = res.read(|resources| {
                let Ok(xobjects) = resources.get(b"XObject") else {
                    return;
                };
                let _ = crate::access::read_resolved(access, xobjects, |xobjects| {
                    let Ok(xobjects) = xobjects.as_dict() else {
                        return;
                    };
                    for (_, value) in xobjects.iter() {
                        let Ok(id) = value.as_reference() else {
                            continue;
                        };
                        if drawn.as_ref().is_some_and(|drawn| !drawn.contains(&id))
                            || seen.contains(&id)
                        {
                            continue;
                        }
                        let Ok(stream) = access.stream(id) else {
                            continue;
                        };
                        let row = stream
                            .read(|stream| {
                                let dict = &stream.dict;
                                if !crate::walker::has_subtype(stream, b"Image") {
                                    return None;
                                }
                                let (Ok(width), Ok(height)) = (
                                    dict.get(b"Width").and_then(|object| object.as_i64()),
                                    dict.get(b"Height").and_then(|object| object.as_i64()),
                                ) else {
                                    return None;
                                };
                                let filters = image_filters(dict);
                                let mut format = filter_to_format(&Some(filters));
                                let mut data = if format == "raw" {
                                    match assemble_png(doc, access, &scope, stream) {
                                        Some(png) => {
                                            format = "png";
                                            png
                                        }
                                        None => stream.content.clone(),
                                    }
                                } else {
                                    codec_payload(stream)
                                };
                                if format == "jpeg" {
                                    if let Some(png) = normalized_jpeg_png(access, dict, &data) {
                                        format = "png";
                                        data = png;
                                    }
                                }
                                Some(ImageInfo {
                                    page: pno,
                                    index,
                                    width,
                                    height,
                                    color_space: image_color_space(
                                        doc, access, &scope, dict,
                                    ),
                                    bits_per_component: image_bpc(access, dict),
                                    format,
                                    data,
                                })
                            })
                            .flatten();
                        let Some(row) = row else {
                            continue;
                        };
                        out.push(row);
                        index += 1;
                        from_this_dict.push(id);
                    }
                });
            });
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

/// How wide a fully clear vertical lane must be, in ems of the run's mean type size, to be a
/// COLUMN separator rather than the space between two words of one cell.
///
/// Swept on the 100-document `pdf-parse-bench` tables corpus — see the sweep table in
/// `dev-docs/plans/fidelity-fix-sweep.md`.
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

/// Cluster cell LEFT edges into column anchors (gap-based, tolerance `tol`) — the PRIMARY
/// column model.
///
/// It anchors on where each column STARTS, which a wide neighbour does not disturb, and it is
/// a vote: an outlier row can only add an anchor, never remove one. That is the whole reason
/// it is asked before the whitespace-lane model, whose boundaries a single bridging row
/// deletes for the entire table — see the model-order note in `detect_tables_region`.
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
///
/// *What* is projected is the whole question, so the caller supplies the intervals:
/// projecting gap-merged cells lets one row's accidental spacing fix a column boundary for the
/// entire run, while projecting the raw WORDS makes every row vote (see [`word_lanes`]).
fn column_bands(rows: &[Vec<(f32, f32)>], bridge: usize) -> Vec<(f32, f32)> {
    let mut ev: Vec<(f32, i32)> = Vec::new();
    for r in rows {
        for &(lo, hi) in r {
            if hi > lo {
                ev.push((lo, 1));
                ev.push((hi, -1));
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

/// How much of a grid must hold measured VALUES before the equation guard stops
/// applying: the guard is skipped once `dataval * EQ_DATAVAL_DENOM >= nz`, i.e. once
/// at least `1/EQ_DATAVAL_DENOM` of the occupied cells carry a decimal or a 3+-digit
/// number. A display equation carries none of those at all.
///
/// SWEPT end-to-end on the 100-doc / 451-table `pdf-parse-bench` "2026-q1-tables-only"
/// corpus (GriTS-Doc_Con micro, md surface; one clean-worktree wheel per point,
/// changing nothing else):
///
/// ```text
///   denom     micro md   precision   recall   strict   emitted   phantoms
///   (off)      0.6798      0.4269    0.4013     181       424        39   <- guard as it was
///     2        0.7050      0.4298    0.4346     196       456        50
///     3        0.7061      0.4323    0.4390     198       458        51
///     4        0.7061      0.4323    0.4390     198       458        51
///   any >0     0.7069      0.4336    0.4412     199       459        52
/// ```
///
/// The curve SATURATES at 3 — no grid in this corpus has a data-value share between
/// 1/4 and 1/3 — so 3 sits on the plateau edge, which is where a threshold wants to be.
/// The last row is the limit case "a single data value anywhere stands the guard down".
/// It is 0.0008 micro better and is not taken: letting one cell decide is precisely the
/// failure being fixed here, and it leaves no margin for a stray decimal inside a real
/// derivation. Everything from 2 to that limit lies inside 0.002 micro, so this is a
/// plateau, not a peak, and the safer end of it is free.
const EQ_DATAVAL_DENOM: usize = 3;

/// How operator-dense a grid must be for the equation guard's STRONG trigger to fire
/// on its own, ignoring the data-value evidence above: `op * EQ_OP_DENSE >= nz`, so 2
/// means "half the occupied cells carry an operator or a Greek letter".
///
/// SWEPT with the local 25-document corpus gate (`fidelity_math_as_table`, target 0) as
/// the veto and GriTS-Doc_Con micro as the objective:
///
/// ```text
///   value   micro md   precision   recall   strict   emitted   phantoms   math_as_table
///     1      0.7137      0.4328    0.4568     206       476        56        3  VETOED
///     2      0.7061      0.4323    0.4390     198       458        51        0
/// ```
///
/// 1 buys 0.0076 micro and 13 matched tables, and pays for them with THREE real display
/// equations re-emitted as tables in the local corpus — the exact false positive this
/// guard exists to stop. A corpus-gate breach is not a trade, so 2 stands.
const EQ_OP_DENSE: usize = 2;

/// Structural ADMISSION test: why this region is not a genuine data table, or `None` when it
/// is coherent. Kept separate from column-keeping so recovering a sparse column cannot
/// silently re-admit prose/equations. Returning the reason also keeps every refusal auditable
/// under `DPDF_FLUSH` — a guard nobody can see firing is a guard nobody can sweep.
fn incoherent_reason(grid: &[Vec<String>]) -> Option<&'static str> {
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
        return Some("prose-2col");
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
        return Some("gutter-title-3col");
    }
    // Wider mis-grids: reject only when nearly every cell is a full sentence.
    if nz >= 6 && prose * 3 >= nz * 2 && mean_words > 6.0 {
        return Some("prose-wide");
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
    // A cell holding a real DATA VALUE — a decimal, or a number of three digits or
    // more. This is the crate's existing tell for "this is measured data, not
    // notation": the symbolic-matrix guard below already turns on `dataval == 0`
    // for exactly that reason ("a real data table has decimals or multi-digit
    // numbers; a matrix has only single-digit sub/superscripts").
    //
    // PARENTHESISED runs are cut out first, because the commonest thing that looks
    // like a decimal and is not a measurement is an equation NUMBER: `(3.1) y` reads
    // as data value + variable, and four of those lines are a derivation, not a
    // table (`tests/gen_tables.py` L4 / `t0_neg_equation.pdf` lock exactly that).
    // A measurement's own parenthetical — `98.48(7)`, `37.8 (n=37)` — still counts,
    // because the value sits OUTSIDE the brackets, which is the whole distinction.
    let dataval = grid
        .iter()
        .flatten()
        .filter(|c| {
            let mut depth = 0i32;
            let bare: String = c
                .chars()
                .filter(|&ch| {
                    if ch == '(' {
                        depth += 1;
                        false
                    } else if ch == ')' {
                        depth = (depth - 1).max(0);
                        false
                    } else {
                        depth == 0
                    }
                })
                .collect();
            let b = bare.as_bytes();
            // A decimal, or ONE number of three or more digits. The digits have to be
            // CONTIGUOUS: counting them across the whole cell made `1 1 2 1 1 2` — a
            // display equation's subscripts, flattened by extraction — read as a
            // measurement, which is how five real math blocks in the corpus turned into
            // tables. `27450` is a value; three separate single digits are notation.
            (0..b.len()).any(|i| b[i].is_ascii_digit() && i + 2 < b.len() && b[i + 1] == b'.' && b[i + 2].is_ascii_digit())
                || bare
                    .as_bytes()
                    .split(|c| !c.is_ascii_digit())
                    .any(|run| run.len() >= 3)
        })
        .count();
    // Reject an equation region. Two triggers, and they are NOT equally strong:
    //
    //   DENSE — half the occupied cells carry an operator or a Greek letter. That is
    //           what a relation/arrow chain looks like, and it stands on its own.
    //   WEAK  — there is at least ONE operator cell and somewhere a relation or an
    //           equation number. This one fires on a single cell.
    //
    // The weak trigger, unqualified, was deciding the fate of whole tables: a header
    // reading `α=0.90` satisfies it, and the gate meant to spare data tables
    // (`alpha_words <= nz`) compares a WORD count to a CELL count, so it is true of
    // nearly every grid and protects almost nothing. Measured on the 100-document
    // `pdf-parse-bench` tables corpus this guard fired 119 times and 87 of those grids
    // had a majority of cells holding a decimal or a multi-digit number — a display
    // equation has none of those, which is the same evidence the symbolic-matrix guard
    // below already trusts (`dataval == 0`).
    //
    // So the value evidence qualifies the WEAK trigger only. Letting it override DENSE
    // as well re-admitted three real math blocks in the local corpus (`math_AG_2606_02429`
    // twice, `econ_EM_2606_02234`) whose coefficients are large integers — a polynomial
    // has values too, and there the operator density is the honest signal.
    if nz > 0 && alpha_words <= nz {
        let dense = op * EQ_OP_DENSE >= nz;
        let weak = op >= 1 && (has_rel || eqnum);
        if dense || (weak && dataval * EQ_DATAVAL_DENOM < nz) {
            return Some("equation");
        }
    }
    // Symbolic MATRIX/array mis-detected as a table (e.g. a block matrix of
    // subscripted variables W₀, D₁Y₁, ∇W₁). Unlike the equation case above it
    // carries no '=' / eq-number and is not operator-dense — its cells are plain
    // variables. Signature: NO data values (a real data table has decimals or
    // multi-digit numbers; a matrix has only single-digit sub/superscripts), NO
    // real words, and a majority of cells are variable-like (start with a letter).
    // A numeric data table fails this (its cells start with digits and it has data
    // values), so it is unaffected.
    let letter_start = grid.iter().flatten().filter(|c| c.trim_start().chars().next().is_some_and(|ch| ch.is_alphabetic())).count();
    if nz >= 4 && dataval == 0 && alpha_words == 0 && letter_start * 2 >= nz {
        return Some("matrix");
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
        return Some("diagram");
    }
    None
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
    /// Number of leading rows in `header + grid` that are semantic header rows.
    ///
    /// This is deliberately separate from `header.len()`: `header` records detached rows
    /// that belong to the table's visible cell sequence, while this field records which of
    /// those rows should render as `<th>`. Keeping the two independent lets ownership fixes
    /// correct semantics without dropping, moving, or duplicating cells.
    pub header_rows: usize,
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
        mcid: s.mcid,
    }
}

/// How clear of text the centre lane must be, as a share of the page's rows, for the
/// **prose-free** split route (the prose route keeps its own, looser 0.88 — it has a second
/// witness). Swept — see the note on [`BASELINE_AGREE`].
const GUTTER_CLEAR: f32 = 1.0;
/// How far apart two baselines may sit, in ems of the page's mean type size, and still count
/// as ONE row for [`shared_baselines`]. Swept — see [`BASELINE_AGREE`].
const BASELINE_EPS: f32 = 0.05;
/// The share of gutter-crossing rows that must sit on one baseline for the lane to be a
/// TABLE's internal gutter rather than a page split.
///
/// **Swept on our own two corpora, not adopted from anywhere.** Measured over every page
/// whose centre lane is fully clear: the 51 two-column pages of the 100-document
/// `pdf-parse-bench` tables corpus reach **at most 0.400**, while the 52 World Bank
/// full-page ruled tables of bench100 — the hardest same-baseline case we own, because their
/// wrapped cells put continuation lines on one side only — bottom out at **0.467**. The two
/// populations do not overlap, and 0.45 sits inside the gap. On the content metric the
/// choice is a plateau (0.5214 md at 0.42/0.45/0.46, 0.5204 at 0.35, 0.5142 at 0.25) and the
/// bench100 floor gate is GREEN across it; at 0.7 the World Bank pages split and two FP
/// ceilings breach (`full-grid|paragraphs` 0.550 -> 0.576).
///
/// The tolerance matters as much as the share: at [`BASELINE_EPS`] = 0.15 em the two
/// populations OVERLAP (0.571 vs 0.500) and no threshold separates them. 0.05 em is "the
/// producer painted these on one baseline", which is the thing being asked.
const BASELINE_AGREE: f32 = 0.45;
/// How many rows must straddle the lane before [`shared_baselines`] is allowed to judge it.
/// Swept over {1, 2, 4, 8, 12}: flat at 0.5214 md up to 4, then 0.5185 at 8 and 0.5135 at 12.
const GUTTER_MIN_ROWS: usize = 4;

/// How far out of step with a run's own row pitch one line gap must be before it is read as
/// **the end of one table and the start of the next** rather than a wide row.
///
/// The run-builder in [`detect_tables_region`] takes every consecutive stretch of >=2-cell rows
/// as ONE table, and had no test for where a table ends: two tables stacked in one text column
/// are one unbroken stretch, so they came out as one grid. Measured on the 100-document
/// `pdf-parse-bench` "2026-q1-tables-only" corpus, that was the single largest content defect
/// left — 63 of 69 contaminated emissions, 587 cells belonging to a table other than the one
/// they were emitted in.
///
/// **Swept on our own corpus; nothing here is adopted from anywhere.** Four candidate boundary
/// signals were scored at every internal row boundary of all 250 joinable runs, against the
/// true cut point of the 52 fusions that join to a run (`dev-docs/bench/out/g2/`):
///
/// | signal | argmax lands on the true cut | true-cut score p50 | clean-run max p95 |
/// |---|---|---|---|
/// | column-model discontinuity | 24/52 | 0.600 | 0.800 |
/// | row cell-count change | 23/52 | 0.400 | 0.667 |
/// | **inter-row-gap outlier** | **46/52** | **2.732** | **1.414** |
///
/// Only the gap outlier separates: the other two put the true cut inside the clean population.
/// This also *disproves* the two signals the evidence bank promoted — the owners' column counts
/// disagree in 55 of 63 fusions, but that disagreement is not *localised* at the boundary, so a
/// column-model test cannot find it. `rule_banded` was tried as the discriminator the USGS
/// band-row class wants and fires on 28 of 52 fused runs against 141 of 198 clean ones — it
/// carries no information here and is not used.
///
/// The threshold was then swept ON THE CORPUS, end to end, one clean-worktree wheel each
/// (`dev-docs/bench/out/g2/corpus_sweep.md`) — micro GriTS-Doc_Con, md surface:
///
/// | x median gap | 1.6 | 1.8 | 2.0 | **2.5** | 3.0 |
/// |---|---|---|---|---|---|
/// | micro md | 0.6745 | 0.6740 | 0.6769 | **0.6798** | 0.6728 |
/// | vertical contamination | 24 | 24 | 24 | **24** | 27 |
/// | minority cells | 107 | 107 | 107 | **120** | 165 |
/// | docs scoring < 0.50 | 18 | 18 | 18 | **17** | 18 |
///
/// A single interior peak, not a cliff: below it the split starts cutting inside tables that
/// were already right (emitted tables climb to 444 while micro falls), above it stacked pairs
/// start surviving again (vertical contamination back to 27). The offline separation predicted
/// the shape — 2.5 catches 39 of the 52 fusions against 40 at 2.0, but cuts only 4 clean runs
/// against 7 — and the corpus confirmed it is the better trade.
const ROW_PITCH_BREAK: f32 = 2.5;
/// The fewest rows either side of a pitch break may have. Swept over {2, 3} at both 2.0 and
/// the chosen 2.5: at 2.5 the two are indistinguishable on micro md (0.6798 either way) and 2
/// leaves one fewer fused emission (24 v 25) and two fewer misplaced cells (120 v 122), because
/// short stacked tables are the common case rather than the exception. A part too small or too
/// sparse to be a table is refused by `flush`'s own admission and the whole split is then
/// abandoned — see [`pitch_breaks`] — so 2 gives up nothing in safety to buy that.
const ROW_PITCH_MIN_PART: usize = 2;

/// Where a run of aligned rows stops being ONE table, by its own row pitch.
///
/// Returns the row indices at which to cut (each is the first row of the next part), empty when
/// the run reads as one table. A gap of at least [`ROW_PITCH_BREAK`] times the run's median line
/// gap is the boundary: two tables stacked in a column are separated by the leading a caption or
/// a paragraph break puts there, and that space survives even when — as in 48 of the 63 measured
/// fusions — there is no text row between the two bands at all for a caption test to see.
///
/// The median is the run's own pitch, so a loosely set table is judged against itself; nothing
/// here is absolute.
fn pitch_breaks(ys: &[f32]) -> Vec<usize> {
    if ys.len() < 2 * ROW_PITCH_MIN_PART {
        return Vec::new();
    }
    let gaps: Vec<f32> = ys.windows(2).map(|w| w[0] - w[1]).collect();
    let mut sorted = gaps.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    let base = if sorted.len() % 2 == 0 { (sorted[mid - 1] + sorted[mid]) * 0.5 } else { sorted[mid] };
    if base <= 0.0 {
        return Vec::new();
    }
    (0..gaps.len())
        .map(|i| i + 1)
        .filter(|&k| {
            k >= ROW_PITCH_MIN_PART && ys.len() - k >= ROW_PITCH_MIN_PART && gaps[k - 1] >= ROW_PITCH_BREAK * base
        })
        .collect()
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
    central_split(spans).map(|(g, _)| g)
}

/// [`central_gutter`] plus the ONE fact its caller cannot re-derive: **whether a table may still
/// span the two sides.**
///
/// The split tolerates a handful of rows crossing the lane, which is what lets a two-column page
/// carrying one full-width table still be split — and the caller's rejoin then exists to put
/// that table back together. But when the lane is clear in **every** row, no element on the page
/// crosses it *at all*, so there is nothing to rejoin and an overlapping left/right pair is two
/// tables set side by side. Re-detecting across the full width there hands back exactly the
/// interleaved grid the split just prevented — `pdf-parse-bench` doc 001, whose page splits
/// cleanly and is then re-merged into
/// `| None | 68.7 | 64.4 | | | | Target Model | 0% | 100% Ratio |`.
fn central_split(spans: &[Span]) -> Option<(f32, bool)> {
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
        // A table may still span the two sides — but only if some row actually crosses the
        // lane. Where NOT ONE does, there is nothing to rejoin, whichever route got here.
        return Some((g, best.0 < rows.len()));
    }
    // The SECOND route, for the page the prose test structurally cannot see: two columns
    // that both carry TABLES. There is no wrapping prose to count, so the test above never
    // fires and the page is read whole — `rows_of` then clusters a line from the left
    // column with a line from the right into one row and the two tables come out
    // interleaved. See [`shared_baselines`] for the evidence that replaces the prose count.
    let size = spans.iter().map(|s| s.size).sum::<f32>() / spans.len().max(1) as f32;
    if best.0 as f32 >= rows.len() as f32 * GUTTER_CLEAR && !shared_baselines(&rows, g, size) {
        return Some((g, best.0 < rows.len()));
    }
    None
}

/// Share of the rows crossing `g` whose two sides sit on ONE baseline — the test that tells a
/// table's internal gutter from a page's column gutter without asking for prose.
///
/// A table row is painted as a single baseline: every cell of it, left of the gutter and right
/// of it, has the same `y`, because that is what makes it a row. Two facing text columns have
/// **independent** vertical rhythms — each column's leading starts where its own content
/// starts — so a left line and a right line share a baseline only by accident. So where
/// [`rows_of`]'s half-line band has already bound spans from both sides into one row, the
/// question "is this really one row?" has a direct answer in the geometry, and it needs no
/// prose anywhere on the page.
///
/// Returns `true` (→ do not split) when too few rows straddle `g` to judge, so the test can
/// only ever *add* a split where the evidence is present.
fn shared_baselines(rows: &[Vec<Span>], g: f32, size: f32) -> bool {
    let side = |s: &Span| s.x + s.width.max(0.0) * 0.5 < g;
    let (mut both, mut agree) = (0usize, 0usize);
    for r in rows {
        let l: Vec<f32> = r.iter().filter(|s| side(s)).map(|s| s.y).collect();
        let rr: Vec<f32> = r.iter().filter(|s| !side(s)).map(|s| s.y).collect();
        if l.is_empty() || rr.is_empty() {
            continue;
        }
        both += 1;
        let d = l
            .iter()
            .flat_map(|a| rr.iter().map(move |b| (a - b).abs()))
            .fold(f32::INFINITY, f32::min);
        if d <= size * BASELINE_EPS {
            agree += 1;
        }
    }
    both < GUTTER_MIN_ROWS || agree as f32 >= both as f32 * BASELINE_AGREE
}

/// Why a declared table was refused. Reported per page so a rejection is evidence, not a
/// silent fall-through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Refusal {
    /// Fewer than two rows resolved to content on this page — the World Bank shard shape
    /// (one table declared as 1×9 + 1×8 + 4×13, the first two of which are not tables).
    TooFewRows,
    /// Fewer than two columns after span expansion.
    TooFewCols,
    /// The cells name marked content that this page does not paint, or name nothing at all.
    Unresolved,
    /// The resolved content occupies no area — a table with nowhere to be.
    Degenerate,
}

/// A page's declared tables after the trust rule has run.
pub(crate) struct Declared {
    /// Accepted declarations, ready to place like any other detected table.
    pub tables: Vec<PosTable>,
    /// Refusals, in declaration order.
    pub refused: Vec<Refusal>,
}

/// **L0** — the tables the page *declares* (`/StructTreeRoot`), resolved against the spans it
/// actually paints.
///
/// The declaration supplies the structure — how many rows, how many columns, which cells are
/// headers, which cells span — and the page supplies the content: each cell's `/MCID`s select
/// the glyph runs [`crate::text`] stamped with that id, and each `/OBJR` selects the widget
/// annotation whose appearance carries a form field's value. Nothing here is inferred and
/// nothing is thresholded; the only judgement is the trust rule, which decides whether the
/// declaration *resolved*, never whether it is well-shaped:
///
/// - **≥2 rows** with content on this page. A declaration that fragments into single-row
///   shards is not describing a table, whatever it says.
/// - **≥2 columns** after span expansion.
/// - **≥2 cells** resolving to real content, so a tree pointing at nothing is refused rather
///   than emitted as an empty grid.
/// - a **non-degenerate region**, since the region is what masks inference off the area.
///
/// A refusal is a fall-through, not an error: the page is then extracted exactly as an
/// untagged page would be. That asymmetry is the whole safety argument for L0 — it can only
/// act where a declaration exists *and* resolves, so every path it does not touch is
/// byte-identical to before.
///
/// `spans` and `annots` must be in the same (display) space; the returned tables are too.
pub(crate) fn declared_pos_tables(declared: &[crate::structtree::DeclaredTable], spans: &[Span], annots: &[(ObjectId, crate::geom::Rect)]) -> Declared {
    use crate::geom::Rect;
    let mut out = Declared { tables: Vec::new(), refused: Vec::new() };
    if declared.is_empty() {
        return out;
    }
    let mut by_mcid: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, s) in spans.iter().enumerate() {
        if let Some(m) = s.mcid {
            by_mcid.entry(m).or_default().push(i);
        }
    }
    // Spans no marked-content sequence claimed. A widget's value is painted by an appearance
    // stream, which carries no `/MCID`, so an `/OBJR` cell collects by geometry — but only
    // from the unclaimed pool, so it can never steal another cell's declared content.
    let free: Vec<usize> = spans.iter().enumerate().filter(|(_, s)| s.mcid.is_none()).map(|(i, _)| i).collect();
    let sbox = |i: usize| {
        let s = &spans[i];
        Rect::new(s.x, s.y, s.x + s.width.max(s.size * 0.3), s.y + s.size)
    };

    for t in declared {
        let cols = t.cols();
        // Resolve every cell: its span set, its box, and whether it resolved at all.
        let rows: Vec<Vec<(String, Rect, bool)>> = t
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|c| {
                        let mut idx: Vec<usize> = c.mcids.iter().filter_map(|m| by_mcid.get(m)).flatten().copied().collect();
                        let mut bx = Rect::EMPTY;
                        let mut resolved = !idx.is_empty();
                        for o in &c.objs {
                            let Some(r) = annots.iter().find(|(id, _)| id == o).map(|(_, r)| *r) else { continue };
                            resolved = true;
                            bx = bx.union(r);
                            idx.extend(free.iter().copied().filter(|&i| {
                                let b = sbox(i);
                                r.contains((b.x0 + b.x1) * 0.5, (b.y0 + b.y1) * 0.5)
                            }));
                        }
                        idx.sort_unstable();
                        idx.dedup();
                        bx = idx.iter().fold(bx, |a, &i| a.union(sbox(i)));
                        (cell_text(spans, &idx), bx, resolved)
                    })
                    .collect()
            })
            .collect();
        // Rows the page really carries. `structtree` filtered by what the tree *claims*;
        // this filters by what the page *paints* — a claim about a page that does not paint
        // it is exactly the stale tag the trust rule exists for.
        let live: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].iter().any(|c| c.2)).collect();
        let filled = rows.iter().flatten().filter(|c| c.2 && !c.0.trim().is_empty()).count();
        let region = rows.iter().flatten().filter(|c| c.2).fold(Rect::EMPTY, |a, c| a.union(c.1));
        let refusal = if live.len() < 2 {
            Some(Refusal::TooFewRows)
        } else if cols < 2 {
            Some(Refusal::TooFewCols)
        } else if filled < 2 {
            Some(Refusal::Unresolved)
        } else if !region.is_valid() || region.width() <= 1.0 || region.height() <= 1.0 {
            Some(Refusal::Degenerate)
        } else {
            None
        };
        if let Some(r) = refusal {
            out.refused.push(r);
            continue;
        }
        let kept: Vec<&Vec<(String, Rect, bool)>> = live.iter().map(|&i| &rows[i]).collect();
        let spans_of: Vec<Vec<(usize, usize)>> =
            live.iter().map(|&i| t.rows[i].iter().map(|c| (c.rowspan, c.colspan)).collect()).collect();
        let grid = lay_out_grid(&kept, &spans_of, cols);
        // Leading rows that are entirely header cells become the table's `<th>` rows; a
        // header cell further down stays a `<td>`, which is the same simplification the
        // inference path makes and costs nothing the scorer sees.
        let nhdr = live
            .iter()
            .take_while(|&&i| !t.rows[i].is_empty() && t.rows[i].iter().all(|c| c.header))
            .count()
            .min(grid.len().saturating_sub(1));
        let header: Vec<Vec<(String, usize)>> =
            grid[..nhdr].iter().map(|r| r.iter().map(|c| (c.clone(), 1usize)).collect()).collect();
        out.tables.push(PosTable {
            y_top: region.y1,
            y_bottom: region.y0,
            x_left: region.x0,
            x_right: region.x1,
            grid: grid[nhdr..].to_vec(),
            header,
            header_rows: nhdr,
        });
    }
    out
}

/// One declared cell's text: its spans read as lines (top-down, left-to-right), each line's
/// words merged by the same typographic rules the inference path uses, lines joined by a
/// space. A cell is normally one run; a wrapped one is two or three.
fn cell_text(spans: &[Span], idx: &[usize]) -> String {
    if idx.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    for row in rows_of(idx.iter().map(|&i| clone_span(&spans[i])).collect()) {
        let line = row_cells(&row).into_iter().map(|c| c.text).collect::<Vec<_>>().join(" ");
        if !line.trim().is_empty() {
            parts.push(line.trim().to_string());
        }
    }
    parts.join(" ")
}

/// Expand a declared table into a rectangular `cols`-wide grid, honouring `/RowSpan` and
/// `/ColSpan`: the spanning cell's text lands in its first covered position and the positions
/// it continues over are empty. The declared merge is preserved as *shape* — the grid is the
/// one a reader sees — while the emitted HTML stays a plain cell matrix; scoring merges needs
/// a ground-truth extension this phase does not have.
fn lay_out_grid(rows: &[&Vec<(String, crate::geom::Rect, bool)>], spans: &[Vec<(usize, usize)>], cols: usize) -> Vec<Vec<String>> {
    let cols = cols.max(1);
    let mut occupied: Vec<Vec<bool>> = vec![vec![false; cols]; rows.len()];
    let mut grid: Vec<Vec<String>> = vec![vec![String::new(); cols]; rows.len()];
    for (r, row) in rows.iter().enumerate() {
        let mut c = 0usize;
        for (i, cell) in row.iter().enumerate() {
            while c < cols && occupied[r][c] {
                c += 1;
            }
            if c >= cols {
                break;
            }
            let (rs, cs) = spans[r].get(i).copied().unwrap_or((1, 1));
            grid[r][c] = cell.0.clone();
            for dr in 0..rs.max(1) {
                for dc in 0..cs.max(1) {
                    if let Some(slot) = occupied.get_mut(r + dr).and_then(|row| row.get_mut(c + dc)) {
                        *slot = true;
                    }
                }
            }
            c += cs.max(1);
        }
    }
    grid
}

/// A word's painted x-extent. `width` is the glyph advance the text walker measured; a span
/// that arrived without one (no font metrics) is given a half-em per character so it still
/// occupies the page rather than collapsing to a point.
pub(crate) fn span_extent(s: &Span) -> (f32, f32) {
    let t = s.text.trim();
    let w = if s.width > 0.1 { s.width } else { t.chars().count() as f32 * s.size * 0.5 };
    (s.x, s.x + w)
}

/// Cut one span at every boundary in `bounds` that falls strictly inside its painted extent,
/// returning the pieces left to right (one piece — a clone — when nothing cuts it).
///
/// **This is the binding primitive a ruled grid needs.** A cell boundary the producer *drew*
/// does not care where the text walker chose to end a run: it can fall in the middle of one.
/// Placing such a run by its centroid puts every character of it on the side the midpoint
/// happened to fall, which is how a grid that is exactly right can be filled with the wrong
/// contents. Cutting it puts each character where the page put it.
///
/// **Why a splitting operation and not a wider `Span`.** [`crate::text::decode_words`] already
/// splits each show operator at spaces and at visible kerns, so a `Span` is a *word*, not a
/// text run — and a word is short. Retaining a per-glyph offset vector on every span would
/// cost one allocation per word on every page of every document, for an exactness that was
/// measured and is not there: across the 100-document `pdf-parse-bench` tables corpus, of the
/// 1074 emitted cells that were two ground-truth cells run together, **1067 separate at a
/// space and 7 inside a token** — and those 7 are punctuation artefacts (`0.3,` `-622`), not
/// column cuts. So the character positions inside a word are interpolated across the word's
/// own measured advance, which is exact at every space (where the cuts are) and off by at most
/// a fraction of one glyph anywhere else.
fn split_span_at(s: &Span, bounds: &[f32]) -> Vec<Span> {
    let (x0, x1) = span_extent(s);
    let n = s.text.chars().count();
    if n < 2 || x1 <= x0 {
        return vec![clone_span(s)];
    }
    let per = (x1 - x0) / n as f32;
    // Character index at which each interior boundary falls, ascending and deduplicated.
    let mut cuts: Vec<usize> = bounds
        .iter()
        .filter(|&&b| b > x0 && b < x1)
        .map(|&b| (((b - x0) / per).round() as usize).clamp(1, n - 1))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    if cuts.is_empty() {
        return vec![clone_span(s)];
    }
    let chars: Vec<char> = s.text.chars().collect();
    let mut out = Vec::with_capacity(cuts.len() + 1);
    let mut from = 0usize;
    for &c in cuts.iter().chain(std::iter::once(&n)) {
        let text: String = chars[from..c].iter().collect();
        if !text.trim().is_empty() {
            out.push(Span {
                x: x0 + per * from as f32,
                width: per * (c - from) as f32,
                text,
                ..clone_span(s)
            });
        }
        from = c;
    }
    if out.is_empty() { vec![clone_span(s)] } else { out }
}

/// The text of one lattice cell: its pieces read as lines (top-down, left-to-right), each
/// line's words joined by the same typographic rules the inference path uses.
fn lattice_cell_text(pieces: &[Span]) -> String {
    if pieces.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    for row in rows_of(pieces.iter().map(clone_span).collect()) {
        let line = row_cells(&row).into_iter().map(|c| c.text).collect::<Vec<_>>().join(" ");
        if !line.trim().is_empty() {
            parts.push(line.trim().to_string());
        }
    }
    parts.join(" ")
}

/// **L1 → L2** — one candidate table region and the evidence it carries, with no opinion
/// about which type it is.
///
/// Detection never reads this struct's *interpretation*: L1 decides that a region is a
/// candidate, and only then does [`classify`] read the evidence to pick an L3 handler. That
/// separation is the structural guarantee — adding a type below cannot change what is
/// detected, because nothing in L1 consults the type table.
pub(crate) struct Candidate<'a> {
    /// The ruling frame that vouched for this region (`None` for a region only text alignment
    /// found).
    pub frame: Option<&'a crate::lattice::Frame>,
    /// Long horizontal rules crossing this region — the booktabs signal.
    pub long_h: usize,
    /// Vertical rules inside it.
    pub v_rules: usize,
    /// The grid the ALIGNMENT path already built here, when it built one.
    pub aligned: Option<PosTable>,
}

/// An **L3 grid handler**: turn one candidate into a table, or refuse it.
type L3 = fn(&Candidate, &[Span]) -> Option<PosTable>;

/// **L2 — the table types, as DATA.**
///
/// One row per type: a name, the rule that recognises it, and the L3 handler it dispatches to.
/// Rows are read in order and the first match wins, so the last row must accept everything.
///
/// Adding a type is a row here, the handler it names, and a gate cell — *nothing else*. In
/// particular nothing in L1 reads this table, so a new row cannot change detection recall;
/// [`tests::a_new_type_reaches_l3_without_touching_l1`] executes that claim.
pub(crate) struct TypeRule {
    pub name: &'static str,
    pub matches: fn(&Candidate) -> bool,
    pub handler: L3,
}

pub(crate) const TABLE_TYPES: &[TypeRule] = &[
    // full-grid — the producer published the cell boundaries geometrically, both ways. The
    // rules ARE the grid, so blank cells and in-grid band titles are as visible as any other.
    TypeRule { name: "full-grid", matches: |c| c.frame.is_some(), handler: l3_ruled },
    // column-ruled — verticals with no row-band rule. MEASURED ABSENT from the corpus (the
    // gate records the null result); declared so a later reader knows it was measured, not
    // overlooked, and served by the alignment path until a real one turns up.
    TypeRule { name: "column-ruled", matches: |c| c.v_rules >= 2 && c.long_h < 2, handler: l3_aligned },
    // booktabs — horizontal rules band the rows, alignment gives the columns. Our largest lead
    // over pymupdf; the handler is the untouched alignment path, which is how it is protected.
    TypeRule { name: "booktabs", matches: |c| c.long_h >= 2, handler: l3_aligned },
    // borderless — alignment is the only signal there is.
    TypeRule { name: "borderless", matches: |_| true, handler: l3_aligned },
];

/// The first type rule that accepts this candidate. The table's last row accepts everything,
/// so this cannot fail for a well-formed table; a truncated one falls back to alignment.
pub(crate) fn classify<'t>(types: &'t [TypeRule], c: &Candidate) -> Option<&'t TypeRule> {
    types.iter().find(|t| (t.matches)(c))
}

/// L2 + L3 for one candidate: classify, then dispatch.
fn build_table(types: &[TypeRule], c: &Candidate, spans: &[Span]) -> Option<PosTable> {
    (classify(types, c)?.handler)(c, spans)
}

/// **L3, alignment** — the type-agnostic text path's own answer, handed back unchanged.
///
/// booktabs and borderless publish nothing geometric, so there is nothing here to read but
/// where the words sit; that work already happened in [`detect_tables_region`]. Keeping this
/// handler an identity is deliberate: it is what makes "outside any frame, behaviour is
/// byte-identical" a property of the code rather than a hope.
fn l3_aligned(c: &Candidate, _spans: &[Span]) -> Option<PosTable> {
    c.aligned.clone()
}

/// Infer a leading stack of uniform header tiers from structure and ownership evidence.
///
/// A grouped header refines from a sparse parent tier into progressively more occupied child
/// tiers, ending at the fully named leaf columns.  Every row in that chain has explicit header
/// styling somewhere in the row.  Requiring every span to share the style would discard valid
/// mixed-content headers: a nested paragraph can retain its own font while its sibling cells
/// carry the table's header face.  The refinement chain is what makes one styled cell safe.
/// This deliberately stops at the first full row, so later full-width bold band titles are
/// body rows, and it refuses a sparse first data row when the producer supplied no header
/// styling.  There are no text/style cut-offs here: the evidence is the exact grid occupancy,
/// strict refinement, and the span parser's binary bold ownership.
fn uniform_header_depth(
    grid: &[Vec<String>],
    row_is_header_styled: impl Fn(usize) -> bool,
) -> usize {
    let Some(first) = grid.first() else {
        return 1;
    };
    let ncols = first.len();
    if ncols < 2 {
        return 1;
    }
    let occupied = |row: &[String]| row.iter().filter(|cell| !cell.trim().is_empty()).count();
    let mut previous = occupied(first);
    if previous == 0 || previous == ncols || !row_is_header_styled(0) {
        return 1;
    }
    for (ri, row) in grid.iter().enumerate().skip(1) {
        let current = occupied(row);
        if current <= previous || !row_is_header_styled(ri) {
            return 1;
        }
        if current == ncols {
            return ri + 1;
        }
        previous = current;
    }
    1
}

/// A lattice bigger than this is a chart's gridlines or a calendar of nothing; building a grid
/// of that size from spans is pure cost.
const MAX_LATTICE_CELLS: usize = 4096;
/// Fewer populated cells than this and the lattice is decoration, not a table.
const MIN_LATTICE_FILLED: usize = 4;
/// The share of a frame's words a column line may pass THROUGH before the frame is refused as
/// not-a-table's-ruling, in percent.
///
/// SWEPT on our own corpora, not adopted from anywhere. The 88-document bench100 corpus offers
/// 444 closed-cell frames: **237 cut no word at all and 302 cut fewer than 5 %** — a real
/// table's ruling does not cross its own text — while 52 cut 60 % or more (chart gridlines,
/// decorative borders, a map's graticule). Between 5 % and 20 % sit 59 frames, and the
/// committed fixture `tests/fixtures_pdf/map_label_grid.pdf` page 2 — a ruling whose columns
/// are narrower than the labels inside them — sits at 4/16 = 25 %. 10 is inside the sparse
/// band, 2.5× clear of that fixture, and it is where both corpora score best: bench100
/// `full-grid|tables` 0.642 at 10 vs 0.638 at 5 and 0.629 at 0, with the pdf-parse-bench
/// content metric flat (0.4928 / 0.4938 / 0.4943 — a 0.0015 spread).
const LATTICE_CUT_PCT: usize = 10;
/// How much of an inferred table's HEIGHT a ruling frame must span before it may replace it.
/// Swept — see the note at the L1b replacement in [`detect_tables_pos`].
const FRAME_COVERS: f32 = 0.5;

/// **L3, ruled** — read the grid straight off the frame's lattice, binding text by GEOMETRIC
/// CONTAINMENT.
///
/// Every cell is a band × band rectangle the producer drew, so a cell nobody typed into is
/// still a cell and a full-width band title is still one row — the two things text clustering
/// cannot see.
///
/// Binding is containment, not centroid: a run is filtered to its row band, then **cut at
/// every column boundary that falls inside it** ([`split_span_at`]) so each piece lands in the
/// cell that actually contains it. A run straddling a column rule is split between the two
/// cells rather than dumped wholly into the one its midpoint fell in — which is the whole
/// reason this handler was held back when the lattice geometry landed.
///
/// The only judgement is admission: a lattice with almost no text in it is a figure's
/// gridlines or a form's decorative border, not a table.
fn l3_ruled(c: &Candidate, spans: &[Span]) -> Option<PosTable> {
    let f = c.frame?;
    let axes = f.axes();
    let (ncols, nrows) = (axes.ncols(), axes.nrows());
    if ncols < 2 || nrows < 2 || ncols * nrows > MAX_LATTICE_CELLS {
        return None;
    }
    let bound = crate::grid::bind_contained(&axes, spans, span_extent, split_span_at);
    // A grid line THROUGH a word is evidence against the lattice, not for it. Containment is
    // only meaningful where the producer meant the line to bound a cell; a map's graticule, a
    // chart's gridlines and a decorative rule all cross whatever text is under them, and
    // splitting words on them turns readable labels into shrapnel
    // (`tests/fixtures_pdf/map_label_grid.pdf`: `Guerneville` → `Guernevil` + `le`). A real
    // table's ruling essentially never cuts a word, so the tolerance is a floor against
    // measurement noise rather than a budget.
    if bound.seen > 0 && bound.cut * 100 > bound.seen * LATTICE_CUT_PCT {
        return None;
    }
    let grid: Vec<Vec<String>> = (0..bound.nrows)
        .map(|r| {
            (0..bound.ncols)
                .map(|k| lattice_cell_text(&bound.cells[r * bound.ncols + k]))
                .collect()
        })
        .collect();
    let ruled_header_rows = uniform_header_depth(&grid, |ri| {
        let row = &bound.cells[ri * bound.ncols..(ri + 1) * bound.ncols];
        row.iter()
            .flatten()
            .any(|span| !span.text.trim().is_empty() && span.bold)
    });
    // ADMISSION. A lattice is evidence of cell boundaries, not of a table: a chart's gridlines
    // and a form's decorative border draw one too. Require real content, spread over at least
    // two rows AND two columns — one populated row is a caption strip, one populated column is
    // a list in a box.
    //
    // DISPROVED, and recorded so it is not re-tried: admitting an EMPTY lattice on the strength
    // of the ruling alone (the IRS 1040's 24 blank amount boxes, which the ground truth does
    // count) was built and measured. It moved `full-grid` by -0.002 and added false tables,
    // because on those pages the declared structure L0 reads already emits several tables and
    // the empty ladder becomes one more. The ruling-only admission is not the answer there.
    let filled = grid.iter().flatten().filter(|t| !t.trim().is_empty()).count();
    let rows_used = grid.iter().filter(|r| r.iter().any(|t| !t.trim().is_empty())).count();
    let cols_used = (0..ncols).filter(|&k| grid.iter().any(|r| !r[k].trim().is_empty())).count();
    if filled < MIN_LATTICE_FILLED || rows_used < 2 || cols_used < 2 {
        return None;
    }
    Some(PosTable {
        y_top: axes.bbox.y1,
        y_bottom: axes.bbox.y0,
        x_left: axes.bbox.x0,
        x_right: axes.bbox.x1,
        grid,
        header: Vec::new(),
        // A merged top tier can leave no closed cells and therefore sit just outside the
        // lattice frame.  Where alignment independently found the same region, its uniform
        // tier chain supplies semantic ownership only; the ruled grid/cells/bbox stay exact.
        header_rows: c
            .aligned
            .as_ref()
            .map_or(ruled_header_rows, |t| ruled_header_rows.max(t.header_rows)),
    })
}

/// Does a ruling frame own the region an inferred table claims — i.e. are they the same table,
/// read twice?
///
/// Two tests, because inference fails in two directions. **Area**: half of the smaller region
/// coincides — the rule L0 applies to a declaration, and it catches both an over-split (several
/// fragments inside the frame) and an over-merge (the frame's block plus its neighbours read as
/// one wide grid). **Columns**: the two share a column model and their rows overlap at all —
/// which catches the partial frame, where the ruling closes only the header band of a table
/// whose body the alignment path read separately. Emitting both would count one table twice.
fn owns(outer: &crate::geom::Rect, t: &PosTable) -> bool {
    let tr = crate::geom::Rect::new(t.x_left, t.y_bottom, t.x_right, t.y_top);
    let area = outer.overlap_area(tr) >= 0.5 * tr.area().min(outer.area()).max(1.0);
    let same_columns = outer.overlap_w(tr) >= 0.6 * outer.width().min(tr.width()).max(1.0) && outer.overlap_h(tr) > 0.0;
    area || same_columns
}

/// How many merged row bands cross a region — the `long_h` evidence L2 reads to tell a
/// booktabs candidate from a borderless one.
fn bands_over(bands: &[(f32, f32, f32)], r: &crate::geom::Rect) -> usize {
    bands.iter().filter(|&&(a, b, y)| y >= r.y0 && y <= r.y1 && b.min(r.x1) - a.max(r.x0) >= r.width() * 0.7).count()
}

/// Detect tables. On a two-column page we split down the middle and detect each
/// side independently — a clean centre gutter guarantees nothing spans it, so the
/// two sides are genuinely separate (this is what stops adjacent-column prose from
/// merging into a phantom wide table). Otherwise (single column, or a full-width
/// element across the centre) the whole page is one region.
///
/// `rules` is the page's ruling ([`crate::vector::PageRules`]) — L1's SECOND evidence source.
/// [`crate::lattice::h_bands`] merges the horizontal rules into row bands, which the alignment
/// detector consults where alignment alone cannot decide (see [`rule_banded`]); and
/// [`crate::lattice::frames`] derives closed-cell geometry, which seeds a candidate on its own.
/// That is how a ruled table with blank cells (invisible to text clustering by construction)
/// and one whose in-grid band titles terminate every text run are found at all. Frame evidence
/// only ever ADDS a candidate or replaces the fragments inference made inside that same frame;
/// it never deletes content and never re-splits a cell.
pub fn detect_tables_pos(spans: &[Span], rules: &crate::vector::PageRules) -> Vec<PosTable> {
    // Tables are built from upright text only — rotated labels (axis titles etc.) must
    // not perturb gutter detection or column structure (they're figure labels).
    let upright: Vec<Span> = spans.iter().filter(|s| s.angle.abs() < 0.01).map(clone_span).collect();
    let spans = &upright[..];
    // The row BANDS the page rules, merged. Booktabs evidence: a rule above the header and one
    // under the last row, which is all a booktabs table publishes and all the alignment path
    // needs to trust a run too short to trust on alignment alone.
    let bands = if rules.h.is_empty() { Vec::new() } else { crate::lattice::h_bands(rules) };
    let mut out = detect_aligned_tables(spans, &bands);
    // ── L1b: the ruling ────────────────────────────────────────────────────────────────
    let frames = if rules.h.is_empty() || rules.v.is_empty() { Vec::new() } else { crate::lattice::frames(rules) };
    if !frames.is_empty() {
        let mut framed: Vec<PosTable> = Vec::new();
        for f in &frames {
            let aligned = out
                .iter()
                .filter(|t| owns(&f.bbox, t))
                .max_by(|a, b| {
                    let region = |t: &PosTable| {
                        crate::geom::Rect::new(t.x_left, t.y_bottom, t.x_right, t.y_top)
                    };
                    // Header ownership comes from the candidate at the TOP of the ruled
                    // frame.  A band row can split alignment into several owned fragments;
                    // choosing the largest would select a body fragment and discard the
                    // independent header evidence (the kitchen-sink fixture is exactly this
                    // shape).  Overlap area breaks ties without iteration-order dependence.
                    a.y_top.total_cmp(&b.y_top).then(
                        f.bbox
                            .overlap_area(region(a))
                            .total_cmp(&f.bbox.overlap_area(region(b))),
                    )
                })
                .cloned();
            let c = Candidate {
                frame: Some(f),
                long_h: bands_over(&bands, &f.bbox),
                v_rules: 0,
                aligned,
            };
            let built = build_table(TABLE_TYPES, &c, spans);
            // Per-page dispatch trace, off unless asked for (`DPDF_TABLES=1`), the same idiom
            // as `DPDF_L0`: which type each frame classified as and whether L3 kept it. A
            // dispatch nobody can see is a dispatch nobody audits.
            if std::env::var_os("DPDF_TABLES").is_some() {
                eprintln!(
                    "  L2 frame {}x{} @[{:.0},{:.0}]-[{:.0},{:.0}] -> {} {}",
                    f.xs.len() - 1,
                    f.ys.len() - 1,
                    f.bbox.x0,
                    f.bbox.y0,
                    f.bbox.x1,
                    f.bbox.y1,
                    classify(TABLE_TYPES, &c).map(|t| t.name).unwrap_or("-"),
                    if built.is_some() { "kept" } else { "refused" }
                );
            }
            if let Some(t) = built {
                framed.push(t);
            }
        }
        if !framed.is_empty() {
            // A frame REPLACES the fragments inference made inside it — but only where it
            // actually covers them. A ruled table whose lower rows are not closed on all four
            // sides produces a frame much shorter than the run the alignment path read, and
            // letting that frame evict the longer table drops the uncovered rows out of the
            // table and back into the body as prose (measured: World Bank status pages 17 of
            // wbD34466311 and wbD34466295, two paragraphs each where the ground truth wants
            // none — the false-positive breach that held this dispatch back). So the frame's
            // answer stands only where it spans at least FRAME_COVERS of the aligned table's
            // height; where the aligned table reaches well past the ruling, the aligned answer
            // is the one that keeps every row, and the frame's is dropped rather than emitted
            // beside it — emitting both is the doc-002 duplicate defect, not a compromise.
            //
            // 0.5 swept over {0, 0.5, 0.75, 0.9, 1.0}: bench100 `full-grid|tables` peaks there
            // (0.647, vs 0.632 at 0.75 and 0.642 at 0.9/1.0) and the pdf-parse-bench content
            // metric is within 0.0006 of its unguarded value (0.4925 vs 0.4928 md), while 0.75
            // and above cost it 0.007. Below 0.5 `owns` is already the binding constraint.
            let pad = 2.0;
            let covers = |r: &crate::geom::Rect, t: &PosTable| {
                let tr = crate::geom::Rect::new(t.x_left, t.y_bottom, t.x_right, t.y_top);
                owns(r, t) && r.overlap_h(tr) + pad >= FRAME_COVERS * (t.y_top - t.y_bottom)
            };
            crate::grid::reconcile_preferred(
                &mut out,
                framed,
                |t| crate::geom::Rect::new(t.x_left, t.y_bottom, t.x_right, t.y_top),
                owns,
                covers,
            );
        }
    }
    // Every surviving alignment table goes through the same L2/L3 dispatch, with no frame
    // evidence to offer — so it reaches an alignment handler, which hands it back unchanged.
    // The round trip is not ceremony: it is what makes the identity a property of the code.
    out.into_iter()
        .filter_map(|t| {
            let c = Candidate { frame: None, long_h: 0, v_rules: 0, aligned: Some(t) };
            build_table(TABLE_TYPES, &c, spans)
        })
        .collect()
}

/// L1a — the text-alignment detector: the whole page, or one lane per text column.
fn detect_aligned_tables(spans: &[Span], bands: &[(f32, f32, f32)]) -> Vec<PosTable> {
    match central_split(spans) {
        None => detect_lanes(spans, bands),
        Some((g, rejoin)) => {
            // Split down the gutter and detect each side independently (this is what
            // stops adjacent-column prose from merging into a phantom wide table).
            let side = |left: bool| -> Vec<Span> {
                spans.iter().filter(|s| (s.x + s.width.max(0.0) * 0.5 < g) == left).map(clone_span).collect()
            };
            let lt = detect_tables_region(&side(true), bands);
            let rt = detect_tables_region(&side(false), bands);
            // A full-width table (e.g. BERT's GLUE table) was split into a left half
            // and a right half that occupy the SAME rows. Detect that: a left-side
            // table whose vertical extent overlaps a right-side table is one table cut
            // in two. Re-detect across the FULL width within just that vertical band
            // (prose outside the band can't interfere) to recover the whole table. A
            // single-column table beside prose has no mate (prose isn't a table), so
            // it is kept as-is — no cross-column bleed.
            // ...but only where a full-width element can EXIST at all — see
            // [`central_split`]. On a page whose gutter no row crosses, an overlapping pair is
            // two tables set side by side, and this re-detection would rebuild the interleaved
            // grid the split just prevented.
            crate::grid::rejoin_split_pairs(
                &lt,
                rt,
                rejoin,
                |t| crate::geom::Rect::new(t.x_left, t.y_bottom, t.x_right, t.y_top),
                |yb, yt| {
                    let pad = 2.0;
                    let band: Vec<Span> = spans
                        .iter()
                        .filter(|s| s.y >= yb - pad && s.y <= yt + pad)
                        .map(clone_span)
                        .collect();
                    detect_tables_region(&band, bands)
                },
            )
        }
    }
}

/// Is this run of rows bracketed above and below by a rule that spans it?
///
/// The booktabs signature, read literally: a horizontal band above the header and another under
/// the last row, each covering most of the run's width and sitting within `pad` of it (so a rule
/// belonging to something else further up the page cannot vouch for anything). Bands arrive
/// already merged ([`crate::lattice::h_bands`]) — a rule drawn in three abutting pieces is one
/// boundary, and read as three it spans nothing.
fn rule_banded(bands: &[(f32, f32, f32)], x0: f32, x1: f32, y_lo: f32, y_hi: f32, pad: f32) -> bool {
    let w = x1 - x0;
    if w < 60.0 {
        return false; // too narrow to be a table's width; two aligned words, not two rows
    }
    let spans = |&(a, b, _): &(f32, f32, f32)| b.min(x1) - a.max(x0) >= w * 0.7;
    let above = bands.iter().any(|r| spans(r) && r.2 > y_hi && r.2 <= y_hi + pad);
    let below = bands.iter().any(|r| spans(r) && r.2 < y_lo && r.2 >= y_lo - pad);
    above && below
}

/// Whether a set of rows carries wrapping PROSE between `lo` and `hi`: a line that is a
/// single wide cell of ≥4 words spanning most of the lane. This is the same test
/// [`central_gutter`] makes of each half, factored out so an n-column page is judged by
/// exactly the rule a two-column page is.
fn prose_lines_in(rows: &[Vec<Span>], lo: f32, hi: f32, min_w: f32) -> usize {
    rows.iter()
        .filter(|r| {
            let side: Vec<Span> =
                r.iter().filter(|s| { let c = s.x + s.width.max(0.0) * 0.5; c >= lo && c < hi }).map(clone_span).collect();
            let cells = row_cells(&side);
            cells.len() == 1 && cells[0].text.split_whitespace().count() >= 4 && (cells[0].end - cells[0].x) > min_w
        })
        .count()
}

/// The gutters of a page laid out in **three or more** text columns, left to right.
///
/// [`central_gutter`] answers the two-column question by scanning the middle third for the
/// single clearest lane — which is exactly right for two columns and structurally unable to
/// see three. A three-column page has no clean *centre* lane at all, so the whole page was
/// treated as one region and adjacent columns' lines clustered into phantom rows: measured on
/// `gov_usgs_usgs70277647` p1, three blocks of newsletter prose came back as 13×3, 19×3 and
/// 7×3 tables. This finds every clear lane instead of the best one.
///
/// The admission rule is the two-column rule applied to every resulting column: each must
/// carry ≥3 lines of wrapping prose. A TABLE's internal gutters are clear too — that is what a
/// column gutter is — and its columns are not prose, which is what tells the two apart.
///
/// Returns empty for a page with fewer than two gutters; the two-column case stays with
/// [`central_gutter`], byte for byte.
pub(crate) fn column_gutters(spans: &[Span]) -> Vec<f32> {
    let rows = rows_of(spans.iter().map(clone_span).collect());
    if rows.len() < 6 {
        return Vec::new();
    }
    let span_r = |s: &Span| (s.x, s.x + s.width.max(s.size * 0.3));
    let x0 = spans.iter().map(|s| s.x).fold(f32::INFINITY, f32::min);
    let x1 = spans.iter().map(|s| span_r(s).1).fold(f32::NEG_INFINITY, f32::max);
    let width = x1 - x0;
    if !width.is_finite() || width <= 1.0 {
        return Vec::new();
    }
    let need = rows.len() as f32 * 0.88;
    let step = (width / 400.0).max(0.5);
    // Maximal runs of x crossed by almost no row. The margins are excluded: a page's outer
    // whitespace is clear by definition and is not a gutter.
    let mut lanes: Vec<(f32, f32)> = Vec::new();
    let mut run: Option<(f32, f32)> = None;
    let mut x = x0 + width * 0.08;
    let hi = x1 - width * 0.08;
    while x <= hi {
        let clear = rows.iter().filter(|r| !r.iter().any(|s| { let (a, b) = span_r(s); a <= x && x <= b })).count();
        if clear as f32 >= need {
            run = Some(run.map_or((x, x), |(a, _)| (a, x)));
        } else if let Some(r) = run.take() {
            lanes.push(r);
        }
        x += step;
    }
    if let Some(r) = run.take() {
        lanes.push(r);
    }
    // A gutter is a LANE, not the one-step gap between two words.
    lanes.retain(|&(a, b)| b - a >= width * 0.015);
    if lanes.len() < 2 {
        return Vec::new();
    }
    let gutters: Vec<f32> = lanes.iter().map(|&(a, b)| (a + b) * 0.5).collect();
    let mut edges: Vec<f32> = vec![f32::NEG_INFINITY];
    edges.extend(gutters.iter().copied());
    edges.push(f32::INFINITY);
    for w in edges.windows(2) {
        let (lo, hi) = (w[0], w[1]);
        // The lane's own width, for the "spans most of the column" test: unbounded at the
        // page edges, so clamp to the text extent.
        let (plo, phi) = (lo.max(x0 - 1.0), hi.min(x1 + 1.0));
        if prose_lines_in(&rows, lo, hi, (phi - plo) * 0.5) < 3 || phi - plo < width * 0.1 {
            return Vec::new(); // not a multi-column PROSE layout — treat the page whole
        }
    }
    gutters
}

/// Detect tables on a page that has no clean CENTRE gutter: either three-or-more prose columns
/// (each detected independently, then rejoined where one table spans them) or, far more often,
/// a single region.
fn detect_lanes(spans: &[Span], bands: &[(f32, f32, f32)]) -> Vec<PosTable> {
    let gutters = column_gutters(spans);
    if gutters.len() < 2 {
        return detect_tables_region(spans, bands);
    }
    let lane_of = |s: &Span| gutters.iter().filter(|&&g| s.x + s.width.max(0.0) * 0.5 >= g).count();
    let nlanes = gutters.len() + 1;
    let per_lane: Vec<Vec<PosTable>> =
        (0..nlanes).map(|k| detect_tables_region(&spans.iter().filter(|s| lane_of(s) == k).map(clone_span).collect::<Vec<_>>(), bands)).collect();
    // A table spanning the columns was cut into pieces occupying the SAME rows. Chain each
    // lane's table to an unused overlapping one to its right, then re-detect across the full
    // width within only that vertical band. A single-column table has no mate and is retained.
    crate::grid::rejoin_lane_chains(
        &per_lane,
        |t| crate::geom::Rect::new(0.0, t.y_bottom, 1.0, t.y_top),
        |yb, yt| {
            let pad = 2.0;
            let band: Vec<Span> = spans.iter().filter(|s| s.y >= yb - pad && s.y <= yt + pad).map(clone_span).collect();
            detect_tables_region(&band, bands)
        },
    )
}

/// Detect tables within a single region (one text column, or the whole page):
/// runs of >=3 consecutive multi-cell rows sharing >=2 aligned columns (occupied
/// in a majority of rows). Rejects word-positioned prose (words merge to a cell).
fn detect_tables_region(spans: &[Span], bands: &[(f32, f32, f32)]) -> Vec<PosTable> {
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
                // A column of the NEIGHBOURHOOD, not only of the anchor this line will join.
                // The narrow reading missed the commonest wrapped cell there is: a status
                // value ("Not / Effective") whose two lines both sit under the *header's*
                // Status column while the data row beside them has nothing in that column at
                // all — there is no cell to line up with in the row it belongs to, because the
                // column is named one row further up. Widened to the whole region it is far
                // too greedy (measured: it swallowed the label lines of three IRS forms and
                // destroyed their tables), so the neighbourhood is a couple of lines.
                let near = avg_size * 4.0;
                let names_column = anchors.iter().any(|&i| {
                    (celled[i].0 - celled[ti].0).abs() <= near && celled[i].1.iter().any(|c| (c.x - cx).abs() <= tol)
                });
                if !names_column {
                    continue;
                }
                let mut best: Option<(usize, f32)> = None;
                for &ai in &anchors {
                    let dy = (celled[ai].0 - celled[ti].0).abs();
                    if dy > avg_size * 1.8 {
                        continue; // not vertically adjacent -> not the same wrapped cell
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

    let trace = std::env::var_os("DPDF_FLUSH").is_some();
    if trace {
        eprintln!("REGION avg_size={avg_size:.1} tol={tol:.1} rows={}", celled.len());
        for (y, cs, _) in &celled {
            eprintln!(
                "  row y={y:7.1} n={} :: {}",
                cs.len(),
                cs.iter().map(|c| format!("[{:.0}-{:.0}]{}", c.x, c.end, c.text)).collect::<Vec<_>>().join(" | ")
            );
        }
    }

    let flush = |run: &Vec<&(f32, Vec<Cell>, Vec<Span>)>, headers: &[&(f32, Vec<Cell>, Vec<Span>)], tables: &mut Vec<PosTable>| {
        if trace {
            eprintln!(
                "FLUSH run={} hdr={} y={:.1}..{:.1}",
                run.len(),
                headers.len(),
                run.last().map_or(0.0, |r| r.0),
                run.first().map_or(0.0, |r| r.0)
            );
        }
        if run.len() < 2 {
            if trace {
                eprintln!("  REJECT run.len()<2");
            }
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
        // A run of three aligned rows is a table on its own evidence. A run of TWO is not —
        // two lines of anything can accidentally align, and admitting them on alignment alone
        // is the classic false-positive flood. But a two-row run bracketed by a rule above and
        // a rule below, each running the width of the run, is exactly what a booktabs table
        // IS, and the ruling is not accidental. Measured: five World Bank status pages publish
        // their disbursement and key-dates tables as header + one data row between two rules,
        // and no amount of alignment work can reach them.
        if run.len() < 3 {
            // …and at least three columns in every row. Two rows and two columns is a BOX, and
            // a boxed pair of fields is the commonest ruled thing on a page that is not a
            // table: measured on `space_moon_lunar_surface_databook_nasa`, the running header
            // ("Revision | Document No | Effective Date | Page") is a 2x2 ruled block repeated
            // on all 70 pages, and admitting it took that document from 25 tables to 93. A
            // real header-plus-one-row table is wide — the World Bank disbursement tables this
            // rule exists for are seven and eight columns.
            if owned.iter().any(|r| r.len() < 3) {
                if trace {
                    eprintln!("  REJECT 2-row run with a <3-col row");
                }
                return;
            }
            let (y_lo, y_hi) = (run.last().map_or(0.0, |r| r.0), run.first().map_or(0.0, |r| r.0));
            if !rule_banded(bands, x_left, x_right, y_lo, y_hi, avg_size * 3.0) {
                if trace {
                    eprintln!("  REJECT 2-row run not rule_banded");
                }
                return;
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
            let raw_rows: Vec<&[Span]> = run.iter().map(|(_, _, spans)| spans.as_slice()).collect();
            let bound = crate::grid::bind_rows_by_center(&kept, &raw_rows, |s| {
                let txt = s.text.trim();
                let w = if s.width > 0.1 { s.width } else { txt.chars().count() as f32 * s.size * 0.5 };
                s.x + w * 0.5
            });
            let grid: Vec<Vec<String>> = bound
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|spans| {
                            let mut cell = String::new();
                            let mut end = f32::NEG_INFINITY;
                            // The grid a consumer reads preserves the text layer's spacing
                            // after the geometric core has allocated pieces to cells.
                            for &s in spans {
                                let txt = s.text.trim();
                                if txt.is_empty() {
                                    continue;
                                }
                                let w = if s.width > 0.1 { s.width } else { txt.chars().count() as f32 * s.size * 0.5 };
                                if !cell.is_empty()
                                    && !crate::textutil::glyph_adjacent(s.x - end, s.size)
                                    && join_space(&cell, txt)
                                {
                                    cell.push(' ');
                                }
                                cell.push_str(txt);
                                end = s.x + w;
                            }
                            cell
                        })
                        .collect()
                })
                .collect();
            if min_fill > 0.0 {
                let total = grid.len() * kept.len();
                let filled = grid.iter().flatten().filter(|c| !c.trim().is_empty()).count();
                if total == 0 || (filled as f32) < min_fill * total as f32 {
                    return None;
                }
            }
            match incoherent_reason(&grid) {
                None => Some((grid, kept.iter().map(|b| b.0).collect())),
                Some(why) => {
                    if trace {
                        eprintln!("    INCOHERENT[{why}] {}x{} :: {:?}", grid.len(), kept.len(), &grid[..grid.len().min(3)]);
                    }
                    None
                }
            }
        };
        // WHITESPACE-LANE band columns: keys on where text SITS, so right-aligned numerics
        // stay distinct and a header-named sparse column survives. Now the FALLBACK — see the
        // model order below.
        let band_kept: Vec<(f32, f32)> = {
            let cell_rows: Vec<Vec<(f32, f32)>> =
                owned.iter().map(|r| r.iter().map(|c| (c.x, c.end)).collect()).collect();
            let bands = column_bands(&cell_rows, 0);
            if bands.len() < 2 {
                Vec::new()
            } else {
                let mut occ = vec![0usize; bands.len()];
                for row in &cell_rows {
                    let mut hit = vec![false; bands.len()];
                    for &(lo, hi) in row {
                        if let Some(bi) = crate::grid::column_band_index(&bands, (lo + hi) * 0.5) {
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
        // LEFT-X clustering (PRIMARY), as bands [anchor_k, anchor_{k+1}). Recovers the
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
        // MODEL CHOICE — ask BOTH, keep the one that resolved more columns.
        //
        // This used to be a fixed order, lanes then left-x, and the order was the single
        // largest cause of our table-CONTENT loss. The two models fail in opposite directions.
        // A lane is a boundary only where *no* row paints across it, so **one** bridging row —
        // a caption, a line interleaved from the facing text column, a wide row label — deletes
        // that column boundary for every row of the table. Left-x clustering is a vote: an
        // outlier row can add an anchor, never remove one. But left-x has the opposite failure,
        // a sparse column whose few values never reach the ≥50% occupancy bar (the wide
        // header-named table `tests/test_table_columns.py` locks), and there the lane model is
        // the one that is right.
        //
        // Neither order is therefore correct, and picking by *columns resolved* is: a column
        // boundary is positive evidence, both answers have already passed their own admission
        // test (`incoherent_reason`, and a ≥0.5 density bar on left-x that keeps a sparse symbol
        // scatter out), so the model that found more real boundaries is the model that read the
        // table. On a tie the lane answer stands, which is what keeps the sparse-column lock.
        //
        // MEASURED on the 100-document / 451-table `pdf-parse-bench` "2026-q1-tables-only"
        // corpus (GriTS-Doc_Con, `dev-docs/bench/scripts/table_content_metric.py`), changing
        // NOTHING else:
        //
        //   lanes first (was)  : micro 0.4177 md / 0.4203 html, 44/451 strict, 1067 emitted
        //                        cells that were two truth cells run together, 200 truth
        //                        tables missed
        //   left-x first       : micro 0.4995 / 0.5033, 79 strict, 647 run-together, 168 missed
        //                        — but it breaks the sparse-column lock (10 columns → 3)
        //   more columns wins  : micro 0.4910 / 0.4947, 78 strict, 672 run-together, 165 missed,
        //                        header association 0.1456 (best of the three), lock intact
        //
        // The 0.008 micro that "left-x first" buys over this is bought by giving up a committed
        // structural guarantee, so it is not taken.
        let (grid, kept_x) = match {
            let (lx, nb) = (leftx_kept(), band_kept.len());
            let nl = lx.len();
            let by_alignment = try_model(lx, 0.5);
            let by_lanes = try_model(band_kept, 0.0);
            if trace {
                eprintln!(
                    "  leftx_kept={nl} -> {:?}   band_kept={nb} -> {:?}",
                    by_alignment.as_ref().map(|a| a.1.len()),
                    by_lanes.as_ref().map(|a| a.1.len())
                );
            }
            match (by_alignment, by_lanes) {
                (Some(a), Some(b)) => Some(if a.1.len() > b.1.len() { a } else { b }),
                (x, y) => x.or(y),
            }
        } {
            Some(gx) => gx,
            None => {
                if trace {
                    eprintln!("  REJECT both models None");
                }
                return;
            }
        };
        if trace {
            eprintln!("  ADMIT {}x{}", grid.len(), kept_x.len());
        }

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
        // The upward attachment walk deliberately preserves visible content even when it
        // crosses a prior aligned run: G3 proved that clamping the walk fragments the only
        // complete/scoring emission on these pages. But rows from that already-owned run are
        // not semantic headers of the later run. Keep every attached row, bbox and cell in
        // place; only bound `<th>` ownership at the run boundary. A detached-only prefix keeps
        // its existing depth (genuine grouped/multi-tier headers), while an attachment that
        // reclaimed any >=2-cell run has exactly the original leading header row.
        let reclaimed_prior_run = headers.iter().any(|(_, cells, _)| cells.len() >= 2);
        let uniform_header_rows = uniform_header_depth(&grid, |ri| {
            run.get(ri).is_some_and(|(_, _, spans)| {
                spans
                    .iter()
                    .any(|span| !span.text.trim().is_empty() && span.bold)
            })
        });
        let header_rows = if reclaimed_prior_run {
            1
        } else if header.is_empty() {
            uniform_header_rows
        } else {
            header.len()
        };
        tables.push(PosTable {
            y_top,
            y_bottom: run.last().map(|(y, _, _)| *y).unwrap_or(0.0),
            x_left,
            x_right,
            grid,
            header_rows,
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
        // Where does this run stop being ONE table? A run is a stretch of aligned rows, and two
        // tables stacked in one column are one unbroken stretch — the boundary is in the row
        // PITCH, not in the text (see [`pitch_breaks`]).
        let ys: Vec<f32> = run_slice.iter().map(|(y, _, _)| *y).collect();
        let breaks = pitch_breaks(&ys);
        if breaks.is_empty() {
            flush(&run_slice, &headers, &mut tables);
            continue;
        }
        // Only the FIRST part inherits the stranded header rows walked up above the run; a part
        // below an interior break has no rows between it and the part above, so its own first
        // row is its header — which is what `flush`'s band model already reads.
        let before = tables.len();
        let mut prev = 0usize;
        for &k in breaks.iter().chain(std::iter::once(&run_slice.len())) {
            let part: Vec<&(f32, Vec<Cell>, Vec<Span>)> = run_slice[prev..k].to_vec();
            let hdr: Vec<&(f32, Vec<Cell>, Vec<Span>)> = if prev == 0 { headers.clone() } else { Vec::new() };
            flush(&part, &hdr, &mut tables);
            prev = k;
        }
        // A split that does not produce at least two tables has not found a boundary — it has
        // cut a table into a piece that survives and a piece that `flush` refuses, and emitting
        // only the survivor DROPS rows. 62 of the 63 fused emissions still matched a truth
        // table, so the run is mostly right and the split must never make it worse: fall back
        // to the whole run, which is exactly what was emitted before this test existed.
        //
        // …but only if the whole run is a table AT ALL. When the un-split run is refused too,
        // the fallback traded the one part that passed every guard for NOTHING — measured on
        // the 100-doc corpus, that happened 8 times (docs 018, 033, 045, 058, 059, 069 twice,
        // 070), and six of those documents carry an unmatched truth table on exactly that
        // region. A survivor is a worse answer than both parts; it is a strictly better answer
        // than silence. So the truncate is now conditional on the re-flush actually replacing
        // what it discards.
        if tables.len() - before < 2 {
            let survivors: Vec<PosTable> = tables.split_off(before);
            flush(&run_slice, &headers, &mut tables);
            if tables.len() == before {
                tables.extend(survivors);
            }
        }
    }
    tables
}

fn detect_tables(spans: Vec<Span>, rules: &crate::vector::PageRules) -> Vec<Vec<Vec<String>>> {
    detect_tables_pos(&spans, rules).into_iter().map(|t| t.grid).collect()
}


/// Extract tables from all pages as owned [`TableInfo`] rows (row-major grids).
///
/// Detection runs per page in PARALLEL: `extract_spans` is an independent read-only walk with
/// its own [`crate::WalkBudget`], and `detect_tables` is pure over one page's spans, so no
/// page can see another. The rows are re-sorted by page number before they are flattened —
/// completion order decides nothing — which makes the output byte-identical to the sequential
/// loop, including each page's internal table order.
pub fn extract_tables(
    doc: &Document,
    access: &dyn crate::access::DocumentAccess,
    raw: &[u8],
) -> Vec<TableInfo> {
    let pages = doc.get_pages();
    let mut per_page: Vec<(u32, Vec<Vec<Vec<String>>>)> = pages
        .par_iter()
        .map(|(&pno, &page_id)| {
            let rules = crate::vector::page_rules(doc, access, page_id);
            (pno, detect_tables(text::extract_spans(access, page_id, raw), &rules))
        })
        .collect();
    per_page.sort_by_key(|(pno, _)| *pno);
    per_page
        .into_iter()
        .flat_map(|(pno, grids)| grids.into_iter().map(move |cells| TableInfo { page: pno, cells }))
        .collect()
}

/// Does this font dict (or its descendant) carry an embedded font program?
fn font_embedded(access: &dyn crate::access::DocumentAccess, dict: &Dictionary) -> bool {
    let embedded = |object: &lopdf::Object| {
        object.as_dict().ok().is_some_and(|descriptor| {
            descriptor.has(b"FontFile")
                || descriptor.has(b"FontFile2")
                || descriptor.has(b"FontFile3")
        })
    };
    // Type0: descriptor lives on the descendant font.
    if let Ok(descriptor) = dict.get(b"FontDescriptor") {
        if let Ok(result) = crate::access::read_resolved(access, descriptor, embedded) {
            // A resolved non-dictionary suppresses the descendant fallback, matching eager.
            return result;
        }
    }
    dict.get(b"DescendantFonts")
        .ok()
        .and_then(|descendants| {
            crate::access::read_resolved(access, descendants, |descendants| {
                let first = descendants.as_array().ok()?.first()?;
                crate::access::read_resolved(access, first, |descendant| {
                    let descriptor = descendant
                        .as_dict()
                        .ok()?
                        .get(b"FontDescriptor")
                        .ok()?;
                    crate::access::read_resolved(access, descriptor, embedded).ok()
                })
                .ok()
                .flatten()
            })
            .ok()
            .flatten()
        })
        .unwrap_or(false)
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
pub fn extract_fonts(access: &dyn crate::access::DocumentAccess) -> Vec<FontInfo> {
    let mut out = Vec::new();
    for page in access.pages().unwrap_or_default() {
        let (pno, page_id) = (page.number, page.id);
        // De-duplicated per page by (resource name, font object id): one font shared by
        // several forms is one row, while the same name bound to different objects in the
        // page and in a form is two. `BTreeMap` keeps rows in resource-name order, which
        // is the order the non-recursive accessor produced them in.
        let mut fonts: BTreeMap<
            (Vec<u8>, Option<ObjectId>),
            (String, String, String, bool, bool),
        > = BTreeMap::new();
        for res in page_resource_dicts(access, page_id) {
            // A form's fonts live in its OWN /Resources (PDF 32000-1 §8.10.2) — the same
            // rule text.rs:1213 follows when it decodes a form's content.
            let _ = res.read(|resources| {
                let Ok(fonts_object) = resources.get(b"Font") else {
                    return;
                };
                let _ = crate::access::read_resolved(access, fonts_object, |fonts_dict| {
                    let Ok(fonts_dict) = fonts_dict.as_dict() else {
                        return;
                    };
                    for (name, value) in fonts_dict.iter() {
                        let parsed = crate::access::read_resolved(access, value, |font| {
                            let dict = font.as_dict().ok()?;
                            let subtype = dict
                                .get(b"Subtype")
                                .and_then(|object| object.as_name())
                                .map(|name| String::from_utf8_lossy(name).into_owned())
                                .unwrap_or_default();
                            let base_font = dict
                                .get(b"BaseFont")
                                .and_then(|object| object.as_name())
                                .map(|name| String::from_utf8_lossy(name).into_owned())
                                .unwrap_or_default();
                            let encoding = dict
                                .get(b"Encoding")
                                .ok()
                                .and_then(|object| object.as_name().ok())
                                .map(|name| String::from_utf8_lossy(name).into_owned())
                                .unwrap_or_else(|| "custom".to_string());
                            Some((
                                subtype,
                                base_font,
                                encoding,
                                font_embedded(access, dict),
                                dict.has(b"ToUnicode"),
                            ))
                        })
                        .ok()
                        .flatten();
                        if let Some(parsed) = parsed {
                            fonts
                                .entry((name.clone(), value.as_reference().ok()))
                                .or_insert(parsed);
                        }
                    }
                });
            });
        }
        for ((name, _), (subtype, base_font, encoding, embedded, has_tounicode)) in fonts {
            out.push(FontInfo {
                page: pno,
                name: String::from_utf8_lossy(&name).into_owned(),
                subtype,
                base_font,
                encoding,
                embedded,
                has_tounicode,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::test_adapter;

    #[test]
    fn uniform_header_tiers_refine_to_the_leaf_columns_and_stop_there() {
        let grid = vec![
            vec!["".into(), "Campaign".into(), "".into(), "".into()],
            vec!["".into(), "Site A".into(), "".into(), "Site B".into()],
            vec!["ID".into(), "Min".into(), "Mean".into(), "Max".into()],
            vec!["R1".into(), "1".into(), "2".into(), "3".into()],
        ];
        assert_eq!(uniform_header_depth(&grid, |ri| ri <= 2), 3);
        assert_eq!(
            uniform_header_depth(&grid, |_| false),
            1,
            "style owns the tiers"
        );

        let full_first = vec![
            vec!["A".into(), "B".into()],
            vec!["bold band".into(), "".into()],
        ];
        assert_eq!(
            uniform_header_depth(&full_first, |_| true),
            1,
            "a later styled band cannot extend a complete leaf header"
        );
    }

    /// The owned form-XObject raster fixture (`tests/gen_fixtures.py::gen_form_image`).
    fn form_image_doc() -> Document {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_image.pdf");
        Document::load(path).expect("form_image.pdf fixture must load")
    }

    /// One committed fixture's page 1, detected exactly as the product detects it: spans and
    /// ruling from the same page, through the same entry point.
    fn detect_fixture(name: &str) -> Vec<PosTable> {
        let path = format!("{}/../tests/fixtures_pdf/{name}", env!("CARGO_MANIFEST_DIR"));
        let doc = Document::load(&path).unwrap_or_else(|e| panic!("{name} must load: {e}"));
        let raw = std::fs::read(&path).expect("fixture readable");
        let page = *doc.get_pages().get(&1).expect("page 1");
        let spans = crate::text::extract_spans(&test_adapter(&doc), page, &raw);
        detect_tables_pos(&spans, &crate::vector::page_rules(&doc, &test_adapter(&doc), page))
    }

    #[test]
    fn a_ruled_grid_publishes_its_own_rows_and_columns_where_the_text_cannot() {
        // `tests/gen_fixtures.py::gen_ruled_blank_cells`. Two shapes that are invisible to text
        // clustering *by construction*: a column nobody typed in (nothing aligns there) and a
        // full-width band title (a ONE-cell row, which ends the run of multi-cell rows and cuts
        // the table in two). The RULING states both plainly — six row bands, four column bands,
        // every cell closed on all four sides — and the ruled handler reads it, cell for cell.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/ruled_blank_cells.pdf");
        let doc = Document::load(path).expect("ruled_blank_cells.pdf must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let page = *doc.get_pages().get(&1).expect("page 1");
        let rules = crate::vector::page_rules(&doc, &test_adapter(&doc), page);
        let frames = crate::lattice::frames(&rules);
        assert_eq!(frames.len(), 1, "one ruled frame, got {}", frames.len());
        assert_eq!(frames[0].xs.len(), 5, "4 column bands: {:?}", frames[0].xs);
        assert_eq!(frames[0].ys.len(), 7, "6 row bands, band titles included: {:?}", frames[0].ys);

        // The whole page, through the production entry point: ONE table, 6x4 — the blank third
        // column is a column and each band title is its own row, neither of which text
        // clustering can see.
        let spans = crate::text::extract_spans(&test_adapter(&doc), page, &raw);
        let tables = detect_tables_pos(&spans, &rules);
        assert_eq!(tables.len(), 1, "one table, got {shape:?}", shape = tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>());
        assert_eq!((tables[0].grid.len(), tables[0].grid[0].len()), (6, 4));
        assert_eq!(tables[0].grid[0], vec!["Site", "Depth", "", "Yield"]);
        assert_eq!(tables[0].grid[1], vec!["Northern district", "", "", ""]);
        assert_eq!(tables[0].grid[5], vec!["Delta", "42.5", "", "128"]);
    }

    #[test]
    fn a_run_that_straddles_a_column_rule_is_split_between_the_two_cells() {
        // The binding primitive, on its own. A cell boundary the producer DREW does not care
        // where the text walker ended a run; placing the run by its centroid puts every
        // character on one side of a line that visibly crosses it.
        let s = Span {
            x: 100.0,
            y: 0.0,
            size: 10.0,
            width: 40.0, // 8 chars, 5pt each
            text: "ABCDEFGH".into(),
            bold: false,
            italic: false,
            mono: false,
            angle: 0.0,
            font: 0,
            mcid: None,
        };
        // A boundary at 120 is 4 characters in.
        let pieces = split_span_at(&s, &[100.0, 120.0, 140.0]);
        assert_eq!(pieces.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), vec!["ABCD", "EFGH"]);
        assert_eq!(pieces[1].x, 120.0, "the second piece starts at the boundary");
        assert_eq!(pieces[0].width + pieces[1].width, 40.0, "the advance is conserved");
        // The frame's own edges are not interior, so a run inside one cell is left whole.
        assert_eq!(split_span_at(&s, &[100.0, 140.0]).len(), 1);
        // A single character cannot be cut.
        let one = Span { text: "X".into(), width: 5.0, ..clone_span(&s) };
        assert_eq!(split_span_at(&one, &[102.0]).len(), 1);
    }

    #[test]
    fn a_lattice_whose_lines_run_through_its_words_is_not_a_table() {
        // `map_label_grid.pdf` page 2: a 4x4 label grid inside a ruling whose 62pt columns are
        // narrower than the labels in them, so a column line passes through 4 of the 16 words.
        // Binding by containment there splits `Guerneville` into `Guernevil` + `le`, which is
        // how the fixture's own "no extracted word goes unrendered" check catches it. Lines
        // through a quarter of the words are evidence the cells are not the cells, so the
        // frame is refused and the alignment reading — which keeps every word whole — stands.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/map_label_grid.pdf");
        let doc = Document::load(path).expect("map_label_grid.pdf must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let page = *doc.get_pages().get(&2).expect("page 2");
        let rules = crate::vector::page_rules(&doc, &test_adapter(&doc), page);
        let frames = crate::lattice::frames(&rules);
        assert!(!frames.is_empty(), "the ruling does close cells — that is the point");
        let spans = crate::text::extract_spans(&test_adapter(&doc), page, &raw);
        let c = Candidate { frame: Some(&frames[0]), long_h: 0, v_rules: 0, aligned: None };
        assert_eq!(classify(TABLE_TYPES, &c).map(|t| t.name), Some("full-grid"), "it classifies");
        assert!(l3_ruled(&c, &spans).is_none(), "…and the handler refuses it");
    }

    #[test]
    fn a_new_type_reaches_l3_without_touching_l1() {
        // The structural guarantee, executed: adding a type is a row in the data table plus an
        // L3 handler, and it cannot change DETECTION — nothing in L1 reads the type table.
        // Register a dummy type that claims every framed candidate and returns a fixed 1x1
        // grid, and show (a) the classifier dispatches to it, (b) the candidates L1 produced
        // are exactly the same ones.
        fn l3_dummy(_c: &Candidate, _s: &[Span]) -> Option<PosTable> {
            Some(PosTable {
                y_top: 1.0,
                y_bottom: 0.0,
                x_left: 0.0,
                x_right: 1.0,
                grid: vec![vec!["dummy".into()]],
                header: Vec::new(),
                header_rows: 1,
            })
        }
        const EXTENDED: &[TypeRule] = &[
            TypeRule { name: "dummy", matches: |c| c.frame.is_some(), handler: l3_dummy },
            TypeRule { name: "borderless", matches: |_| true, handler: l3_aligned },
        ];
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/ruled_blank_cells.pdf");
        let doc = Document::load(path).expect("ruled_blank_cells.pdf must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let page = *doc.get_pages().get(&1).expect("page 1");
        let spans = crate::text::extract_spans(&test_adapter(&doc), page, &raw);
        let rules = crate::vector::page_rules(&doc, &test_adapter(&doc), page);
        let frames = crate::lattice::frames(&rules);
        assert_eq!(frames.len(), 1, "L1 found one frame");

        let framed = Candidate { frame: Some(&frames[0]), long_h: 0, v_rules: 0, aligned: None };
        assert_eq!(classify(TABLE_TYPES, &framed).map(|t| t.name), Some("full-grid"));
        assert_eq!(classify(EXTENDED, &framed).map(|t| t.name), Some("dummy"), "the new row wins");
        assert_eq!(build_table(EXTENDED, &framed, &spans).map(|t| t.grid), Some(vec![vec!["dummy".to_string()]]));

        // L1 is untouched: the same page, detected with the production table, still produces
        // the same frame — the type table changed what a candidate BECOMES, never what is found.
        assert_eq!(crate::lattice::frames(&rules).len(), frames.len());
        assert_eq!(crate::lattice::frames(&rules)[0].xs, frames[0].xs);
    }

    #[test]
    fn a_booktabs_table_with_a_wrapped_cell_is_one_table_not_three() {
        // `tests/gen_fixtures.py::gen_booktabs_wrapped`. This is the shape we lead pymupdf on
        // by an order of magnitude, and the shape a ruling path most easily damages: the
        // under-header rule looks like a row boundary and the wrapped Description line looks
        // like a row. Nothing here closes a cell, so the lattice must find nothing at all.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/booktabs_wrapped.pdf");
        let doc = Document::load(path).expect("booktabs_wrapped.pdf must load");
        let page = *doc.get_pages().get(&1).expect("page 1");
        let rules = crate::vector::page_rules(&doc, &test_adapter(&doc), page);
        assert!(crate::lattice::frames(&rules).is_empty(), "horizontal rules alone close no cell");
        let tables = detect_fixture("booktabs_wrapped.pdf");
        assert_eq!(tables.len(), 1, "one table, got {}", tables.len());
        let g = &tables[0].grid;
        assert_eq!(g.len(), 4, "the wrapped line folds into its cell: {g:?}");
        assert!(g[2][1].contains("long") && g[2][1].contains("sweeps"), "wrapped cell joined: {g:?}");
    }

    #[test]
    fn a_three_column_page_yields_its_one_real_table_and_no_phantoms() {
        // `tests/gen_fixtures.py::gen_three_column_prose`, the `gov_usgs_usgs70277647` p1
        // class. With no clean CENTRE gutter the page was read whole and the three columns'
        // lines clustered into rows across the gutters — three phantom N×3 grids of prose.
        let tables = detect_fixture("three_column_prose.pdf");
        assert_eq!(tables.len(), 1, "exactly one table, got {}: {:?}", tables.len(), tables.iter().map(|t| t.grid.len()).collect::<Vec<_>>());
        assert_eq!(tables[0].grid[0][0].trim(), "Zone", "and it is the ruled one: {:?}", tables[0].grid);
    }

    #[test]
    fn two_facing_columns_of_tables_are_two_tables_not_one_wide_one() {
        // `tests/gen_fixtures.py::gen_two_column_tables`, the `pdf-parse-bench` doc-001 class.
        // `central_gutter` would split this page — but only where each side shows wrapping
        // PROSE, and a page whose two columns are both tables has none to show. So the page
        // was read whole and `rows_of`'s half-line band bound a left-column line to a
        // right-column line: `| Model | BLEU | Rate | Corpus | Size | Split |`, one 7x6 grid
        // interleaving two unrelated 7x3 tables.
        let tables = detect_fixture("two_column_tables.pdf");
        assert_eq!(
            tables.len(),
            2,
            "two tables, got {}: {:?}",
            tables.len(),
            tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>()
        );
        for t in &tables {
            assert_eq!(t.grid[0].len(), 3, "three columns each, not six: {:?}", t.grid[0]);
        }
        let heads: Vec<&str> = tables.iter().map(|t| t.grid[0][0].trim()).collect();
        assert!(heads.contains(&"Model") && heads.contains(&"Corpus"), "one table per column: {heads:?}");
    }

    #[test]
    fn two_tables_stacked_in_one_column_are_two_tables() {
        // `tests/gen_fixtures.py::gen_stacked_tables`. The run-builder takes every consecutive
        // stretch of >=2-cell rows as ONE table and had no test for where a table ends, so a
        // stacked pair came back as one 12x3 grid. Measured on the `pdf-parse-bench` tables
        // corpus this was the largest remaining content defect: 63 of 69 contaminated
        // emissions, 587 misplaced cells. Nothing sits between the two bands here — that is
        // the majority shape (48 of the 63) — so only the row PITCH can see the boundary.
        let tables = detect_fixture("stacked_tables.pdf");
        assert_eq!(
            tables.len(),
            2,
            "two tables, got {}: {:?}",
            tables.len(),
            tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>()
        );
        for t in &tables {
            assert_eq!(t.grid[0].len(), 3, "three columns each: {:?}", t.grid[0]);
            assert_eq!(t.grid.len(), 6, "six rows each — neither band lost a row: {:?}", t.grid);
        }
        let heads: Vec<&str> = tables.iter().map(|t| t.grid[0][0].trim()).collect();
        assert!(heads.contains(&"Model") && heads.contains(&"Corpus"), "one table per band: {heads:?}");
    }

    #[test]
    fn a_wide_gap_inside_one_table_does_not_end_it() {
        // `tests/gen_fixtures.py::gen_banded_one_table`, the negative twin of the test above
        // and the direct tension with bench100's USGS 6-way-split class: a table gives its
        // header air, and a full-width band row inside one table must NOT terminate it (the
        // stranded-header machinery landed for that class depends on it). Both wide gaps here
        // are 1.47x the body pitch, inside the swept 2.5x break — which is what makes
        // [`ROW_PITCH_BREAK`] a threshold rather than "there is extra space here".
        let tables = detect_fixture("banded_one_table.pdf");
        assert_eq!(
            tables.len(),
            1,
            "one table, got {}: {:?}",
            tables.len(),
            tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>()
        );
        assert_eq!(tables[0].grid.len(), 7, "all seven rows, band included: {:?}", tables[0].grid);
        assert!(
            tables[0].grid.iter().any(|r| r[0].trim() == "Northern Basin"),
            "the band row is still inside it: {:?}",
            tables[0].grid
        );
    }

    #[test]
    fn one_greek_equals_sign_in_a_header_does_not_delete_a_data_table() {
        // `tests/gen_fixtures.py::gen_alpha_header_data_table`, from `pdf-parse-bench` doc 069
        // page 1. The equation guard fired on `op >= 1 && has_rel` — satisfied by the SINGLE
        // header cell `a=0.90` — and the gate meant to spare data tables (`alpha_words <= nz`)
        // compares a word count to a cell count, so it is true of nearly every grid. A clean
        // 10x3 table of confidence intervals was refused outright. [`EQ_DATAVAL_DENOM`] is what
        // stands the guard down here: most of these cells hold measured values.
        let tables = detect_fixture("alpha_header_data_table.pdf");
        assert_eq!(
            tables.len(),
            1,
            "one table, got {}: {:?}",
            tables.len(),
            tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>()
        );
        assert_eq!(tables[0].grid[0].len(), 3, "three columns: {:?}", tables[0].grid[0]);
        assert_eq!(tables[0].grid.len(), 10, "all ten rows: {:?}", tables[0].grid);
        assert!(
            tables[0].grid.iter().any(|r| r[0].trim() == "AEDGA"),
            "the last row survived: {:?}",
            tables[0].grid
        );
    }

    #[test]
    fn a_display_equation_is_still_not_a_table() {
        // `tests/gen_fixtures.py::gen_display_equation_block`, the NEGATIVE twin of the test
        // above and the lower bracket on [`EQ_DATAVAL_DENOM`]. Three aligned `lhs = rhs (n)`
        // lines are a clean 3x3 run geometrically; only the CONTENT separates them from a data
        // table, and the separator is that a derivation carries no measured values at all.
        // Relaxing the guard on anything weaker than that re-opens this.
        let tables = detect_fixture("display_equation_block.pdf");
        assert!(
            tables.is_empty(),
            "a derivation is not a table, got {}: {:?}",
            tables.len(),
            tables.iter().map(|t| t.grid.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_pitch_split_keeps_its_survivor_when_the_whole_run_is_refused() {
        // `tests/gen_fixtures.py::gen_split_survivor_table`. `pitch_breaks` cuts this run into a
        // prose block `flush` refuses and a data table it admits. "A split must yield at least
        // two tables" then discarded the admitted one and re-flushed the whole run — which is
        // refused too, because the prose is back in it. The table was traded for silence.
        // Measured on the 100-document corpus: 8 occurrences (docs 018, 033, 045, 058, 059,
        // 069 twice, 070). Falling back is only right when the fallback produces something.
        let tables = detect_fixture("split_survivor_table.pdf");
        assert_eq!(
            tables.len(),
            1,
            "the survivor is emitted, got {}: {:?}",
            tables.len(),
            tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>()
        );
        assert_eq!(tables[0].grid[0].len(), 3, "three columns: {:?}", tables[0].grid[0]);
        assert!(
            tables[0].grid.iter().any(|r| r[0].trim() == "Corpus"),
            "and it is the data band, not the prose: {:?}",
            tables[0].grid
        );
    }

    /// RED LEDGER (phase G3) — the contained-duplicate defect, root-caused and measured, with a
    /// fix that is not landed because it breaches a gated bench100 cell.
    ///
    /// A one-cell SECTION HEADING set inside a table on its own leading ends the aligned run, and
    /// the header walk above the NEXT run has no lower bound — so it climbs over the heading and
    /// over every row the previous run already emitted, and each run below the first re-publishes
    /// all of them as *header* rows. The page comes out as NESTED tables, each a strict row-prefix
    /// of the next. `pdf-parse-bench` doc 033 publishes one 27-row table five times over, at 7,
    /// 12, 17, 23 and 27 rows; the trace is `dev-docs/bench/out/g3/trace/033.flush`. Corpus-wide:
    /// 17 emissions in 22 containment pairs over 12 documents, 21 of the 22 a strict row-prefix,
    /// 16 of the 17 pure phantoms the matcher never used.
    ///
    /// Reading such a row as INTERIOR to the run (`dev-docs/bench/out/g3/spanning_row.patch`)
    /// fixes it and is a large measured win — micro md 0.7061 → 0.7545, table precision 0.4323 →
    /// **0.5184**, containment 22 pairs → 1, phantoms 51 → 9, and the torture corpus's worst
    /// class `t2|band_rows` 0.0417 → 0.4861 — but it costs three gated bench100 cells:
    /// `full-grid|paragraphs` recall 0.450 → 0.438 and the FP ceilings of `full-grid|paragraphs`
    /// (0.550 → 0.576) and `ALL|paragraphs` (0.624 → 0.632), on four pages (World Bank
    /// wbD34466311#17, wbD34466295#17, wbD34466172#12 and IRS f1040#2), where a table the run
    /// change alters spills rows back into the body as prose. A floor breach is a failed phase,
    /// not a trade-off (plan §0.4), and two fix-forward attempts did not close it: an
    /// emit-nothing fallback (recall −0.016 → −0.012, breach stands) and a ruled-row guard (no
    /// effect on the breach, and it costs `booktabs|tables` 0.660 → 0.593).
    ///
    /// The fixtures stay so the evidence does; the day the paragraph regression is closed,
    /// promotion is deleting two `#[ignore]` lines. See `dev-docs/plans/consider-for-future.md`.
    #[test]
    fn a_section_break_cannot_claim_prior_table_rows_as_headers() {
        // G7 is narrower than the ignored G3 content fix below: keep every currently attached
        // row and every emission, but never render an already-owned aligned run as a stack of
        // semantic headers. This stays green if G3 is eventually fixed at source too.
        let tables = detect_fixture("header_backwalk_table.pdf");
        assert!(!tables.is_empty());
        assert!(
            tables.iter().all(|t| t.header_rows <= 1),
            "prior runs are data, not headers: {:?}",
            tables
                .iter()
                .map(|t| (t.header_rows, t.header.len(), t.grid.len()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore = "G3 red ledger: fixed by spanning_row.patch, which breaches full-grid|paragraphs"]
    fn a_section_heading_inside_a_table_does_not_publish_it_three_times() {
        let tables = detect_fixture("section_heading_table.pdf");
        assert_eq!(
            tables.len(),
            1,
            "one table, got {}: {:?}",
            tables.len(),
            tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>()
        );
        let t = &tables[0];
        assert_eq!(t.grid.len(), 10, "header, six data rows and all three headings: {:?}", t.grid);
        // Including the one in the FIRST body position, which has no run pitch above it — the
        // torture corpus's `band_rows` shape.
        for want in ["Upper sequence", "Encoder Stack", "Decoder Stack", "Zeta"] {
            assert!(t.grid.iter().any(|r| r[0].trim() == want), "{want} is inside it: {:?}", t.grid);
        }
    }

    /// The negative twin of the entry above, and it passes TODAY — a one-cell line with air on
    /// both sides (2.7x the body pitch) ends the run, which is the behaviour any future
    /// spanning-row rule must keep. It is not in the red ledger: it locks the bound, not the fix.
    #[test]
    fn a_heading_with_air_around_it_still_ends_the_table() {
        let tables = detect_fixture("heading_ends_table.pdf");
        assert_eq!(
            tables.len(),
            2,
            "two tables, got {}: {:?}",
            tables.len(),
            tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>()
        );
        assert!(
            !tables.iter().any(|t| t.grid.iter().any(|r| r[0].trim() == "Second Study")),
            "the heading is not a row of either: {:?}",
            tables.iter().map(|t| t.grid.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_split_page_is_not_re_merged_where_nothing_crosses_the_gutter() {
        // `tests/gen_fixtures.py::gen_two_column_tables_prose`, and the ACTUAL doc-001 defect.
        // This page has prose, so it always split correctly — and was then handed straight
        // back by the rejoin that recovers a table the gutter cut in half. Measured: with the
        // rejoin unguarded this fixture returns one 6x6 grid.
        let tables = detect_fixture("two_column_tables_prose.pdf");
        assert_eq!(tables.len(), 2, "two tables, got {}: {:?}", tables.len(), tables.iter().map(|t| (t.grid.len(), t.grid[0].len())).collect::<Vec<_>>());
        for t in &tables {
            assert_eq!(t.grid[0].len(), 3, "three columns each, not six: {:?}", t.grid[0]);
        }
    }

    #[test]
    fn a_wide_tables_own_gutter_is_not_a_page_split() {
        // The other side of the same test. `booktabs_wrapped.pdf` is ONE table whose middle
        // column leaves a lane that is clear in every row — the exact geometry the split
        // route keys on — but its rows are painted on one baseline, so `shared_baselines`
        // refuses. A wrapped continuation line sits on a baseline of its own and must not be
        // enough to flip that.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/booktabs_wrapped.pdf");
        let doc = Document::load(path).expect("booktabs_wrapped.pdf must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let page = *doc.get_pages().get(&1).expect("page 1");
        let spans = crate::text::extract_spans(&test_adapter(&doc), page, &raw);
        assert_eq!(central_gutter(&spans), None, "a table's own gutter is not a page split");
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
        let spans = crate::text::extract_spans(&test_adapter(&doc), page, &raw);
        let tables = detect_tables_pos(&spans, &crate::vector::page_rules(&doc, &test_adapter(&doc), page));
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
    fn parallel_table_detection_yields_the_sequential_row_order() {
        // `extract_tables` fans its pages out over rayon. Rows are ordered by page and, within
        // a page, by detection order — neither may come from completion order, so compare
        // against an independent sequential oracle and repeat it (a race shows up as an
        // occasional disagreement, never a permanent one).
        let mut with_tables = 0usize;
        for path in crate::doc::tests::fixture_pdfs() {
            let Ok(raw) = std::fs::read(&path) else { continue };
            let Ok(doc) = Document::load_mem(&raw) else { continue }; // encrypted / damaged
            let mut want: Vec<(u32, Vec<Vec<String>>)> = Vec::new();
            for (&pno, &page_id) in &doc.get_pages() {
                for grid in detect_tables(text::extract_spans(&test_adapter(&doc), page_id, &raw), &crate::vector::page_rules(&doc, &test_adapter(&doc), page_id)) {
                    want.push((pno, grid));
                }
            }
            if !want.is_empty() {
                with_tables += 1;
            }
            for run in 0..5 {
                let got: Vec<(u32, Vec<Vec<String>>)> = extract_tables(&doc, &test_adapter(&doc), &raw).into_iter().map(|t| (t.page, t.cells)).collect();
                assert_eq!(got, want, "run {run} of {} disagrees with the sequential scan", path.display());
            }
        }
        assert!(with_tables >= 3, "the sweep must cover documents that actually detect tables, got {with_tables}");
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
        let paths = crate::doc::tests::fixture_pdfs();
        let mut with_images = 0usize;
        for p in &paths {
            let Ok(doc) = Document::load(p) else { continue }; // encrypted / deliberately damaged
            let short = extract_images_inner(&doc, &test_adapter(&doc), true);
            let full = extract_images_inner(&doc, &test_adapter(&doc), false);
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
        let rows = extract_images(&doc, &test_adapter(&doc));
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
        let rows = extract_images(&doc, &test_adapter(&doc));
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
        let dicts = page_resource_dicts(&test_adapter(&doc), page_id);
        assert_eq!(dicts.len(), 3, "page + the two form resource dicts, each once");

        // …and the image inside the cyclic form is still reported, exactly once.
        let rows = extract_images(&doc, &test_adapter(&doc));
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
        let rows = extract_images(&doc, &test_adapter(&doc));
        let dims: Vec<(u32, usize, i64, i64)> = rows.iter().map(|i| (i.page, i.index, i.width, i.height)).collect();
        // page 1 paints /ImDrawn (40x30) only; page 2 paints only the form, whose content
        // paints /ImInForm (42x32). /ImNever (41x31) and /ImFormNever (43x33) are listed
        // in the very same resource dictionaries and must not appear.
        assert_eq!(dims, vec![(1, 0, 40, 30), (2, 0, 42, 32)]);

        // The reachability walk still sees all four — the filter is the `Do` walk, not a
        // narrower resource tree (which extract_fonts shares).
        let page1 = *doc.get_pages().get(&1).expect("page 1");
        let reachable: usize = page_resource_dicts(&test_adapter(&doc), page1)
            .iter()
            .map(|resources| {
                resources
                    .read(|resources| {
                        let value = resources.get(b"XObject").ok()?;
                        crate::access::read_resolved(&test_adapter(&doc), value, |value| {
                            value.as_dict().ok().map(|xobjects| xobjects.iter().count())
                        })
                        .ok()
                        .flatten()
                    })
                    .ok()
                    .flatten()
                    .unwrap_or(0)
            })
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
            extract_images(&doc, &test_adapter(&doc)).iter().map(|i| (i.page, i.index, i.width, i.height)).collect();
        assert_eq!(
            dims,
            vec![(1, 0, 40, 30), (1, 1, 10, 10), (1, 2, 12, 12), (1, 3, 15, 15), (1, 4, 16, 16)],
            "the page's own image keeps index 0; the appearances are appended after it"
        );
        // What must NOT be there, and why each would be a different bug:
        let sizes: Vec<i64> = extract_images(&doc, &test_adapter(&doc)).iter().map(|i| i.width).collect();
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
            page_resource_dicts(&test_adapter(&doc), page1).len(),
            1,
            "extract_fonts sees the page's own /Resources only — no appearance dictionary"
        );
        assert_eq!(
            appearance_resource_dicts(&test_adapter(&doc), page1).len(),
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
        let rows = extract_images(&doc, &test_adapter(&doc));
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
        let rows = extract_images(&doc, &test_adapter(&doc));

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
        for r in extract_images(&doc, &test_adapter(&doc)) {
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
        let rows = extract_images(&doc, &test_adapter(&doc));
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

        let rows = extract_images(&doc, &test_adapter(&doc));
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
        let fonts = extract_fonts(&test_adapter(&doc));
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
        let names: Vec<String> = extract_fonts(&test_adapter(&doc)).into_iter().map(|f| f.name).collect();
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
        assert!(incoherent_reason(&g).is_none());
    }

    #[test]
    fn prose_two_column_rejected() {
        // a glossary: short term + long wrapped definition (mean words/cell > 4 in 2 cols)
        let g = grid(&[
            &["alpha", "the first letter of the Greek alphabet used widely in mathematics"],
            &["beta", "the second letter often denoting a coefficient or a regression slope"],
            &["gamma", "the third letter frequently used for the Lorentz factor in physics"],
        ]);
        assert!(incoherent_reason(&g).is_some());
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
        assert!(incoherent_reason(&g).is_some());
    }

    // ---------------------------------------------------------------- L0: the trust rule

    /// The tagged fixture (`tests/gen_fixtures.py::gen_tagged_table`), loaded with its spans.
    fn tagged() -> (Document, Vec<Span>, ObjectId) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/tagged_table.pdf");
        let doc = Document::load(path).expect("tagged_table.pdf fixture must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let page = *doc.get_pages().get(&1).expect("page 1");
        let spans = crate::text::extract_spans(&test_adapter(&doc), page, &raw);
        (doc, spans, page)
    }

    #[test]
    fn the_page_content_carries_the_marked_content_ids_the_tree_names() {
        // Without this the declaration is unusable: a `/TD` can name `/MCID 3` all it likes,
        // but nothing ties it to glyphs until the text walk tracks `BDC`/`EMC`. No
        // marked-content handling existed anywhere in the crate before this phase.
        let (_doc, spans, _page) = tagged();
        let marked: Vec<&Span> = spans.iter().filter(|s| s.mcid.is_some()).collect();
        assert_eq!(marked.len(), 13, "the fixture marks 13 runs, got {}", marked.len());
        let alpha = spans.iter().find(|s| s.text == "Alpha").expect("the fixture paints Alpha");
        assert_eq!(alpha.mcid, Some(3));
        let plain = spans.iter().find(|s| s.text == "Ridge").expect("the undeclared grid is painted");
        assert_eq!(plain.mcid, None, "content outside every BDC belongs to no sequence");
    }

    #[test]
    fn a_declared_table_is_emitted_with_the_declared_grid_and_the_shards_are_refused() {
        // All three trust-rule outcomes in one page: the 3x3 with `/ColSpan 2` + `/RowSpan 2`
        // is accepted and expanded exactly as declared; the one-row and one-column `/Table`
        // elements are refused (both shapes occur in the measurement corpus — one World Bank
        // table is declared as 1x9 + 1x8 + 4x13 for a single 2x12 grid).
        let (doc, spans, page) = tagged();
        let declared = crate::structtree::declared_tables(&test_adapter(&doc));
        let annots = crate::walker::annot_rects(&test_adapter(&doc), page);
        let out = declared_pos_tables(&declared[&page], &spans, &annots);
        assert_eq!(out.refused, vec![Refusal::TooFewRows, Refusal::TooFewCols]);
        assert_eq!(out.tables.len(), 1);
        let t = &out.tables[0];
        assert_eq!(t.header_rows, 1, "the declaration carries exactly one TH row");
        assert_eq!(t.header, vec![vec![("Region".into(), 1), (String::new(), 1), ("Total".into(), 1)]]);
        assert_eq!(t.grid, vec![
            vec!["North".to_string(), "Alpha".into(), "11".into()],
            vec![String::new(), "Beta".into(), "22".into()],
        ], "the /RowSpan 2 cell holds column 0 of the row below it");
    }

    #[test]
    fn a_declared_table_with_no_th_cells_has_no_semantic_header() {
        // Same generated L0 fixture and geometry, with the accepted declaration's cells read
        // as TD. This isolates the state the old `header.is_empty() => row 0 is TH` fallback
        // could not represent; exact declarations must not acquire an inferred header.
        let (doc, spans, page) = tagged();
        let mut declared = crate::structtree::declared_tables(&test_adapter(&doc));
        for row in &mut declared.get_mut(&page).expect("page is declared")[0].rows {
            for cell in row {
                cell.header = false;
            }
        }
        let annots = crate::walker::annot_rects(&test_adapter(&doc), page);
        let out = declared_pos_tables(&declared[&page], &spans, &annots);
        assert_eq!(out.tables.len(), 1);
        assert!(out.tables[0].header.is_empty());
        assert_eq!(out.tables[0].header_rows, 0);
    }

    #[test]
    fn a_declaration_whose_cells_resolve_to_nothing_is_refused() {
        // The stale tag: the tree survives an edit that removed the content it named. An
        // empty grid is worse than no grid, so the page falls back to inference.
        let (doc, _spans, page) = tagged();
        let declared = crate::structtree::declared_tables(&test_adapter(&doc));
        let out = declared_pos_tables(&declared[&page], &[], &[]);
        assert!(out.tables.is_empty());
        assert!(out.refused.iter().all(|r| *r == Refusal::TooFewRows), "got {:?}", out.refused);
    }
}
