//! Hyperlink extraction from `/Annots` Link annotations.
//!
//! Three kinds, all recorded with the clickable rectangle (PDF user space):
//!   - external: a `/URI` action  -> `uri`
//!   - internal: a `/GoTo` action or `/Dest` (explicit array or named destination)
//!     -> resolved to a 1-indexed `dest_page` where possible, else the raw
//!     `dest_name` (e.g. "cite.devlin2018", "section.3.1") is kept.
//!   - remote: a `/GoToR` or `/Launch` action -> the target file from `/F` lands
//!     in `remote_file`, and a `/GoToR` `/D` (which addresses the REMOTE
//!     document, so it must never be resolved against this one) keeps its name in
//!     `dest_name`.
//! Named destinations are resolved via the catalog `/Dests` dict and the
//! `/Names /Dests` name tree.

use crate::pdfobj::{decode_text_string, deref, num_deref};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashMap;

/// A link annotation: a clickable rectangle and where it points.
pub struct Link {
    pub page: u32,
    pub rect: [f32; 4], // x0, y0, x1, y1 (PDF user space, y up)
    pub uri: Option<String>,
    pub dest_page: Option<u32>,
    pub dest_name: Option<String>,
    /// `/GoToR` and `/Launch` only: the target file from the action's `/F` file
    /// specification. A `dest_name` set alongside it addresses THAT file, not this one.
    pub remote_file: Option<String>,
}

impl Link {
    /// Which of the three kinds this link is: `"uri"`, `"remote"` or `"internal"`.
    ///
    /// The discriminator is derived, not stored — the three fields are populated by
    /// mutually exclusive branches of [`extract_links`] — and the ORDER of the tests is the
    /// rule: a `/GoToR` carries a `remote_file` and may also carry a `dest_name`, so
    /// `remote` must be decided before `internal` or a remote link reads as a jump inside
    /// this document. It lived in the PyO3 binding, where no Rust consumer could reach it
    /// and where the ordering rule was one edit away from being lost.
    pub fn kind(&self) -> &'static str {
        if self.uri.is_some() {
            "uri"
        } else if self.remote_file.is_some() {
            "remote"
        } else {
            "internal"
        }
    }
}

fn pdf_string(o: &Object) -> Option<String> {
    match o {
        Object::String(b, _) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

/// Read a PDF file specification (`/GoToR`'s `/F`): either a bare string, or a filespec
/// dictionary whose path lives under `/UF` (Unicode, preferred), `/F`, or one of the
/// legacy platform keys. Decoded as PDF text so UTF-16BE `/UF` values come out readable.
fn file_spec(doc: &Document, o: &Object) -> Option<String> {
    match deref(doc, o)? {
        Object::String(b, _) => Some(decode_text_string(b)),
        Object::Dictionary(d) => [&b"UF"[..], b"F", b"DOS", b"Mac", b"Unix"].iter().find_map(|k| {
            d.get(k).ok().and_then(|v| deref(doc, v)).and_then(|v| match v {
                Object::String(b, _) => Some(decode_text_string(b)),
                _ => None,
            })
        }),
        _ => None,
    }
    .filter(|s| !s.trim().is_empty())
}

/// Resolve a destination value (explicit `[pageRef /XYZ …]` array, or a dict with
/// a `/D` array) to a 1-indexed page number plus the target y (top) when present.
/// `/XYZ left top zoom` → top is element 3; `/FitH top` / `/FitBH top` → element 2.
fn dest_to_pos(doc: &Document, v: &Object, page_no: &HashMap<ObjectId, u32>) -> Option<(u32, Option<f32>)> {
    match deref(doc, v)? {
        Object::Array(a) => {
            let p = match a.first() {
                Some(Object::Reference(r)) => page_no.get(r).copied()?,
                _ => return None,
            };
            let y = match a.get(1).and_then(|o| o.as_name().ok()) {
                // Array VALUES, so `num_deref`: `/XYZ 72 15 0 R 0` is legal, and reading it
                // with the direct-only `num` puts the anchor at y=0 (the page bottom).
                Some(b"XYZ") if a.len() >= 4 => Some(num_deref(doc, &a[3])),
                Some(b"FitH") | Some(b"FitBH") if a.len() >= 3 => Some(num_deref(doc, &a[2])),
                _ => None,
            };
            Some((p, y))
        }
        Object::Dictionary(d) => d.get(b"D").ok().and_then(|o| dest_to_pos(doc, o, page_no)),
        _ => None,
    }
}

fn dest_to_page(doc: &Document, v: &Object, page_no: &HashMap<ObjectId, u32>) -> Option<u32> {
    dest_to_pos(doc, v, page_no).map(|(p, _)| p)
}

/// Resolve a link destination to `(dest_page, dest_name)`. A named destination
/// keeps its name even when the page is resolved (useful as an anchor id).
fn resolve_dest(
    doc: &Document,
    dest: &Object,
    page_no: &HashMap<ObjectId, u32>,
    named: &HashMap<Vec<u8>, u32>,
) -> (Option<u32>, Option<String>) {
    match dest {
        Object::Array(_) => (dest_to_page(doc, dest, page_no), None),
        // A destination NAME is a byte string used as a name-tree KEY, not a PDF text
        // string: it must be read verbatim (never through `pdfobj::decode_text_string`)
        // or it stops matching the `/Dests` entry it names. Lossy is correct here.
        Object::Name(n) | Object::String(n, _) => (
            named.get(n).copied(),
            Some(String::from_utf8_lossy(n).into_owned()),
        ),
        Object::Reference(_) => deref(doc, dest)
            .map(|d| resolve_dest(doc, d, page_no, named))
            .unwrap_or((None, None)),
        _ => (None, None),
    }
}

/// Recurse a name-tree node, collecting `name -> target page`.
fn walk_name_tree(
    doc: &Document,
    tree: &Dictionary,
    page_no: &HashMap<ObjectId, u32>,
    out: &mut HashMap<Vec<u8>, u32>,
) {
    if let Some(kids) = tree.get(b"Kids").ok().and_then(|o| o.as_array().ok()) {
        for k in kids {
            if let Some(d) = k.as_reference().ok().and_then(|r| doc.get_dictionary(r).ok()) {
                walk_name_tree(doc, d, page_no, out);
            }
        }
    }
    if let Some(names) = tree.get(b"Names").ok().and_then(|o| o.as_array().ok()) {
        let mut i = 0;
        while i + 1 < names.len() {
            if let Object::String(key, _) = &names[i] {
                if let Some(p) = dest_to_page(doc, &names[i + 1], page_no) {
                    out.insert(key.clone(), p);
                }
            }
            i += 2;
        }
    }
}

/// Collect all named destinations (catalog `/Dests` dict + `/Names /Dests` tree).
fn collect_named(doc: &Document, page_no: &HashMap<ObjectId, u32>) -> HashMap<Vec<u8>, u32> {
    let mut out = HashMap::new();
    let cat = match doc.catalog() {
        Ok(c) => c,
        Err(_) => return out,
    };
    if let Some(dests) = cat.get(b"Dests").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        for (k, v) in dests.iter() {
            if let Some(p) = dest_to_page(doc, v, page_no) {
                out.insert(k.clone(), p);
            }
        }
    }
    if let Some(names) = cat.get(b"Names").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        if let Some(tree) = names.get(b"Dests").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
            walk_name_tree(doc, tree, page_no, &mut out);
        }
    }
    out
}

/// A named destination target: its name and where it lands (1-indexed page + the
/// top y when the destination specifies one). Used to place anchor ids so internal
/// links resolve to the exact reference / figure / equation, not just the page.
pub struct NamedDest {
    pub name: String,
    pub page: u32,
    pub y: Option<f32>,
}

fn walk_name_tree_pos(doc: &Document, tree: &Dictionary, page_no: &HashMap<ObjectId, u32>, out: &mut Vec<NamedDest>) {
    if let Some(kids) = tree.get(b"Kids").ok().and_then(|o| o.as_array().ok()) {
        for k in kids {
            if let Some(d) = k.as_reference().ok().and_then(|r| doc.get_dictionary(r).ok()) {
                walk_name_tree_pos(doc, d, page_no, out);
            }
        }
    }
    if let Some(names) = tree.get(b"Names").ok().and_then(|o| o.as_array().ok()) {
        let mut i = 0;
        while i + 1 < names.len() {
            if let Object::String(key, _) = &names[i] {
                if let Some((p, y)) = dest_to_pos(doc, &names[i + 1], page_no) {
                    // Name-tree key: a byte string, not a text string — see `resolve_dest`.
                    out.push(NamedDest { name: String::from_utf8_lossy(key).into_owned(), page: p, y });
                }
            }
            i += 2;
        }
    }
}

/// All named destinations (catalog `/Dests` dict + `/Names /Dests` tree) with the
/// page and y where each lands.
pub fn named_destinations(doc: &Document) -> Vec<NamedDest> {
    let pages = doc.get_pages();
    let page_no: HashMap<ObjectId, u32> = pages.iter().map(|(&n, &id)| (id, n)).collect();
    let mut out = Vec::new();
    let cat = match doc.catalog() {
        Ok(c) => c,
        Err(_) => return out,
    };
    if let Some(dests) = cat.get(b"Dests").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        for (k, v) in dests.iter() {
            if let Some((p, y)) = dest_to_pos(doc, v, &page_no) {
                // `/Dests` key: a byte string, not a text string — see `resolve_dest`.
                out.push(NamedDest { name: String::from_utf8_lossy(k).into_owned(), page: p, y });
            }
        }
    }
    if let Some(names) = cat.get(b"Names").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        if let Some(tree) = names.get(b"Dests").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
            walk_name_tree_pos(doc, tree, &page_no, &mut out);
        }
    }
    out
}

/// One entry of the PDF's own table of contents (the `/Outlines` bookmark tree).
pub struct OutlineEntry {
    pub level: u8, // 0-based nesting depth (top level = 0)
    pub title: String,
    pub page: u32, // 1-indexed target page; 0 if the destination didn't resolve
}

/// The PDF's author-supplied table of contents, read from the catalog `/Outlines`
/// tree (titles + GoTo destinations), in reading order with nesting depth. Empty when
/// the document has no outline. This is the document's OWN TOC — distinct from the one
/// distillPDF synthesises from detected headings.
pub fn outline(doc: &Document) -> Vec<OutlineEntry> {
    let pages = doc.get_pages();
    let page_no: HashMap<ObjectId, u32> = pages.iter().map(|(&n, &id)| (id, n)).collect();
    let named = collect_named(doc, &page_no);
    let cat = match doc.catalog() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let root = match cat.get(b"Outlines").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let first = match root.get(b"First").ok().and_then(|o| o.as_reference().ok()) {
        Some(r) => r,
        None => return Vec::new(), // present but empty
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk_outline(doc, first, &page_no, &named, 0, &mut seen, &mut out);
    out
}

/// Resolve an outline item's destination (`/Dest` array/name, or `/A` GoTo `/D`) to a
/// page + optional y. Named destinations fall back to the names map (page only).
fn outline_dest(doc: &Document, item: &Dictionary, page_no: &HashMap<ObjectId, u32>, named: &HashMap<Vec<u8>, u32>) -> (u32, Option<f32>) {
    let resolve = |d: &Object| -> Option<(u32, Option<f32>)> {
        match deref(doc, d)? {
            Object::Array(_) => dest_to_pos(doc, d, page_no),
            Object::Name(n) | Object::String(n, _) => named.get(n).map(|&p| (p, None)),
            _ => None,
        }
    };
    if let Ok(dest) = item.get(b"Dest") {
        if let Some(r) = resolve(dest) {
            return r;
        }
    }
    if let Some(a) = item.get(b"A").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        if a.get(b"S").and_then(|o| o.as_name()).ok() == Some(&b"GoTo"[..]) {
            if let Ok(d) = a.get(b"D") {
                if let Some(r) = resolve(d) {
                    return r;
                }
            }
        }
    }
    (0, None)
}

/// Walk a sibling chain (`/Next`), recursing into children (`/First`). A visited set
/// guards against cyclic `/Next` links in malformed outlines; depth is capped.
fn walk_outline(
    doc: &Document,
    node: ObjectId,
    page_no: &HashMap<ObjectId, u32>,
    named: &HashMap<Vec<u8>, u32>,
    depth: u8,
    seen: &mut std::collections::HashSet<ObjectId>,
    out: &mut Vec<OutlineEntry>,
) {
    if depth as u32 > crate::MAX_FORM_DEPTH {
        return;
    }
    let mut cur = node;
    loop {
        if !seen.insert(cur) {
            break; // cycle
        }
        let item = match doc.get_dictionary(cur) {
            Ok(d) => d.clone(),
            Err(_) => break,
        };
        // `/Title` is frequently an indirect reference (hyperref/pdfTeX writes
        // `/Title 5 0 R` pointing at a UTF-16BE string), so deref before matching —
        // without this the title decodes empty and the entry is dropped below.
        let title = match item.get(b"Title").ok().and_then(|o| deref(doc, o)) {
            Some(Object::String(b, _)) => decode_text_string(b),
            _ => String::new(),
        };
        let (page, _y) = outline_dest(doc, &item, page_no, named);
        if !title.trim().is_empty() {
            out.push(OutlineEntry { level: depth, title: title.trim().to_string(), page });
        }
        if let Some(first) = item.get(b"First").ok().and_then(|o| o.as_reference().ok()) {
            walk_outline(doc, first, page_no, named, depth + 1, seen, out);
        }
        match item.get(b"Next").ok().and_then(|o| o.as_reference().ok()) {
            Some(next) => cur = next,
            None => break,
        }
    }
}

/// Extract every Link annotation across the document.
pub fn extract_links(doc: &Document) -> Vec<Link> {
    let pages = doc.get_pages();
    let page_no: HashMap<ObjectId, u32> = pages.iter().map(|(&n, &id)| (id, n)).collect();
    let named = collect_named(doc, &page_no);
    let mut out = Vec::new();

    for (&pno, &pid) in &pages {
        let dict = match doc.get_dictionary(pid) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let annots = dict
            .get(b"Annots")
            .ok()
            .and_then(|o| deref(doc, o))
            .and_then(|o| o.as_array().ok());
        let annots = match annots {
            Some(a) => a,
            None => continue,
        };
        for a in annots {
            let ad = match deref(doc, a).and_then(|o| o.as_dict().ok()) {
                Some(d) => d,
                None => continue,
            };
            let is_link = ad.get(b"Subtype").and_then(|o| o.as_name()).map(|n| n == b"Link").unwrap_or(false);
            if !is_link {
                continue;
            }
            let rect = ad.get(b"Rect").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_array().ok());
            let rect = match rect {
                // Ditto: an annotation `/Rect` entry may be an indirect number, and reading
                // one as 0.0 collapses the clickable box to the page corner.
                Some(r) if r.len() >= 4 => {
                    [num_deref(doc, &r[0]), num_deref(doc, &r[1]), num_deref(doc, &r[2]), num_deref(doc, &r[3])]
                }
                _ => continue,
            };

            let (mut uri, mut dest_page, mut dest_name, mut remote_file) = (None, None, None, None);
            if let Some(act) = ad.get(b"A").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
                let s = act.get(b"S").and_then(|o| o.as_name()).unwrap_or(b"");
                if s == b"URI" {
                    uri = act.get(b"URI").ok().and_then(|o| deref(doc, o)).and_then(pdf_string);
                } else if s == b"GoTo" {
                    if let Ok(d) = act.get(b"D") {
                        let (p, n) = resolve_dest(doc, d, &page_no, &named);
                        dest_page = p;
                        dest_name = n;
                    }
                } else if s == b"GoToR" || s == b"Launch" {
                    // Both carry a `/F` file specification. `/Launch` additionally allows
                    // per-platform launch dictionaries whose own `/F` holds the path.
                    remote_file = act
                        .get(b"F")
                        .ok()
                        .and_then(|o| file_spec(doc, o))
                        .or_else(|| {
                            [&b"Win"[..], b"Mac", b"Unix"].iter().find_map(|k| {
                                act.get(k).ok().and_then(|o| deref(doc, o)).and_then(|o| file_spec(doc, o))
                            })
                        });
                    // A `/GoToR` destination addresses the REMOTE file: never resolve it
                    // against this document's pages or named-destination map.
                    dest_name = match act.get(b"D").ok().and_then(|o| deref(doc, o)) {
                        // Byte string / name-tree key in the REMOTE file, not a text
                        // string — see `resolve_dest`.
                        Some(Object::Name(n)) | Some(Object::String(n, _)) => {
                            Some(String::from_utf8_lossy(n).into_owned())
                        }
                        _ => None,
                    };
                }
            } else if let Ok(d) = ad.get(b"Dest") {
                let (p, n) = resolve_dest(doc, d, &page_no, &named);
                dest_page = p;
                dest_name = n;
            }

            if uri.is_some() || dest_page.is_some() || dest_name.is_some() || remote_file.is_some() {
                // Normalise rect to x0<=x1, y0<=y1.
                let r = [rect[0].min(rect[2]), rect[1].min(rect[3]), rect[0].max(rect[2]), rect[1].max(rect[3])];
                out.push(Link { page: pno, rect: r, uri, dest_page, dest_name, remote_file });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owned hand-written links fixture (`tests/gen_fixtures.py::gen_links`).
    fn links_doc() -> Document {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/links.pdf");
        Document::load(path).expect("links.pdf fixture must load")
    }

    /// `tests/gen_fixtures.py::gen_indirect_numbers` — a `/Rect` and two destination tops
    /// written as indirect references.
    fn indirect_doc() -> Document {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/indirect_numbers.pdf");
        Document::load(path).expect("indirect_numbers.pdf fixture must load")
    }

    fn link_of(uri: Option<&str>, dest_page: Option<u32>, dest_name: Option<&str>, remote: Option<&str>) -> Link {
        Link {
            page: 1,
            rect: [0.0; 4],
            uri: uri.map(String::from),
            dest_page,
            dest_name: dest_name.map(String::from),
            remote_file: remote.map(String::from),
        }
    }

    #[test]
    fn link_kind_decides_remote_before_internal() {
        assert_eq!(link_of(Some("https://x"), None, None, None).kind(), "uri");
        assert_eq!(link_of(None, Some(3), None, None).kind(), "internal");
        assert_eq!(link_of(None, None, Some("cite.devlin2018"), None).kind(), "internal");
        // The ordering rule: a /GoToR carries a remote file AND a dest_name that addresses
        // THAT file. Testing dest_name first would report it as a jump inside this document.
        assert_eq!(link_of(None, None, Some("sec.2"), Some("other.pdf")).kind(), "remote");
        assert_eq!(link_of(None, None, None, None).kind(), "internal", "a bare link degrades, it does not panic");
    }

    #[test]
    fn every_extracted_link_reports_one_of_the_three_kinds() {
        let doc = links_doc();
        let links = extract_links(&doc);
        assert!(!links.is_empty());
        assert!(links.iter().all(|l| ["uri", "internal", "remote"].contains(&l.kind())));
    }

    #[test]
    fn an_annotation_rect_with_indirect_entries_keeps_its_area() {
        // `/Rect [72 696 13 0 R 14 0 R]`. A dictionary value may legally be indirect, but the
        // direct-only `num` reader returned 0.0 for one, collapsing the clickable box to
        // `[72, 696, 0, 0]` — an inverted, zero-area rectangle at the page corner.
        let doc = indirect_doc();
        // The premise: the fixture's rect really is written with indirect entries.
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        let annots = doc.get_dictionary(page_id).unwrap().get(b"Annots").unwrap().as_array().unwrap();
        let annot = deref(&doc, &annots[0]).unwrap().as_dict().unwrap();
        let r = annot.get(b"Rect").unwrap().as_array().unwrap();
        assert!(matches!(r[2], Object::Reference(_)) && matches!(r[3], Object::Reference(_)));

        let links = extract_links(&doc);
        assert_eq!(links.len(), 1, "the fixture carries exactly one link");
        assert_eq!(links[0].uri.as_deref(), Some("https://example.com/indirect"));
        assert_eq!(links[0].rect, [72.0, 696.0, 420.0, 714.0], "indirect /Rect entries must resolve, not read 0");
    }

    #[test]
    fn a_destination_top_written_indirectly_still_points_at_its_target() {
        // `/XYZ 72 15 0 R 0` and `/FitH 16 0 R`: the anchor y is what places the target id on
        // the rendered page. Read as 0.0 it lands at the page BOTTOM, not at the section.
        let doc = indirect_doc();
        let mut dests = named_destinations(&doc);
        dests.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = dests.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["fig.indirect", "sec.indirect"]);
        assert!(dests.iter().all(|d| d.page == 2), "both resolve to page 2");
        assert_eq!(dests[0].y, Some(700.0), "/XYZ top read indirectly");
        assert_eq!(dests[1].y, Some(640.0), "/FitH top read indirectly");
    }

    #[test]
    fn outline_keeps_entries_whose_title_is_an_indirect_utf16be_string() {
        // hyperref/pdfTeX writes `/Title 14 0 R`. Matching only a direct `Object::String`
        // decoded such titles to "" and dropped the whole entry (20 of 54 corpus docs
        // returned an empty outline).
        let doc = links_doc();
        let entries = outline(&doc);
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["Métodos y Análisis §2", "Appendix A Notation"]);
        assert_eq!(entries[0].page, 2, "indirect-title entry must still resolve its /Dest");
        assert_eq!(entries[1].page, 1);
        assert!(entries.iter().all(|e| e.level == 0));
    }

    #[test]
    fn gotor_and_launch_links_survive_with_their_target_file() {
        // Both actions address another document, so nothing resolved in this one and the
        // rows were dropped by the keep-condition — the links vanished entirely.
        let doc = links_doc();
        let links = extract_links(&doc);
        let remote: Vec<&Link> = links.iter().filter(|l| l.remote_file.is_some()).collect();
        assert_eq!(remote.len(), 2, "expected the /GoToR + /Launch rows among {} links", links.len());

        // /GoToR with a bare-string /F.
        let gotor = remote[0];
        assert_eq!(gotor.remote_file.as_deref(), Some("appendix_other.pdf"));
        assert_eq!(gotor.page, 1);
        assert_eq!(gotor.rect, [72.0, 652.0, 420.0, 670.0]);
        // `/D [0 /Fit]` points into the OTHER file: it must not be resolved here.
        assert_eq!(gotor.dest_page, None);
        assert_eq!(gotor.dest_name, None);

        // /Launch with a filespec dictionary.
        let launch = remote[1];
        assert_eq!(launch.remote_file.as_deref(), Some("datasheet_launch.pdf"));
        assert_eq!(launch.rect, [72.0, 630.0, 420.0, 648.0]);

        // The URI and in-document GoTo rows are untouched.
        assert_eq!(links.len(), 4);
        assert!(links.iter().any(|l| l.uri.as_deref() == Some("https://example.com/distillpdf")));
        assert!(links.iter().any(|l| l.dest_name.as_deref() == Some("cite.smith2020") && l.dest_page == Some(2)));
    }

    #[test]
    fn file_spec_reads_both_bare_strings_and_filespec_dictionaries() {
        let doc = Document::new();
        let bare = Object::String(b"other.pdf".to_vec(), lopdf::StringFormat::Literal);
        assert_eq!(file_spec(&doc, &bare).as_deref(), Some("other.pdf"));
        // A filespec dict prefers /UF (Unicode) over /F.
        let mut d = Dictionary::new();
        d.set("F", Object::String(b"legacy.pdf".to_vec(), lopdf::StringFormat::Literal));
        let uf = [&b"\xfe\xff"[..], &"rapport-år.pdf".encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<u8>>()].concat();
        d.set("UF", Object::String(uf, lopdf::StringFormat::Literal));
        assert_eq!(file_spec(&doc, &Object::Dictionary(d.clone())).as_deref(), Some("rapport-år.pdf"));
        d.remove(b"UF");
        assert_eq!(file_spec(&doc, &Object::Dictionary(d)).as_deref(), Some("legacy.pdf"));
        // Empty / non-string specs yield nothing rather than a blank target.
        let empty = Object::String(Vec::new(), lopdf::StringFormat::Literal);
        assert_eq!(file_spec(&doc, &empty), None);
        assert_eq!(file_spec(&doc, &Object::Null), None);
    }
}
