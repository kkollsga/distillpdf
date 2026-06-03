//! Hyperlink extraction from `/Annots` Link annotations.
//!
//! Two kinds, both recorded with the clickable rectangle (PDF user space):
//!   - external: a `/URI` action  -> `uri`
//!   - internal: a `/GoTo` action or `/Dest` (explicit array or named destination)
//!     -> resolved to a 1-indexed `dest_page` where possible, else the raw
//!     `dest_name` (e.g. "cite.devlin2018", "section.3.1") is kept.
//! Named destinations are resolved via the catalog `/Dests` dict and the
//! `/Names /Dests` name tree.

use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashMap;

/// A link annotation: a clickable rectangle and where it points.
pub struct Link {
    pub page: u32,
    pub rect: [f32; 4], // x0, y0, x1, y1 (PDF user space, y up)
    pub uri: Option<String>,
    pub dest_page: Option<u32>,
    pub dest_name: Option<String>,
}

fn deref<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Object> {
    match o {
        Object::Reference(r) => doc.get_object(*r).ok(),
        other => Some(other),
    }
}

fn num(o: &Object) -> f32 {
    match o {
        Object::Integer(i) => *i as f32,
        Object::Real(r) => *r,
        _ => 0.0,
    }
}

fn pdf_string(o: &Object) -> Option<String> {
    match o {
        Object::String(b, _) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
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
                Some(b"XYZ") if a.len() >= 4 => Some(num(&a[3])),
                Some(b"FitH") | Some(b"FitBH") if a.len() >= 3 => Some(num(&a[2])),
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
                Some(r) if r.len() >= 4 => [num(&r[0]), num(&r[1]), num(&r[2]), num(&r[3])],
                _ => continue,
            };

            let (mut uri, mut dest_page, mut dest_name) = (None, None, None);
            if let Some(act) = ad.get(b"A").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
                let s = act.get(b"S").and_then(|o| o.as_name()).unwrap_or(b"");
                if s == b"URI" {
                    uri = act.get(b"URI").ok().and_then(|o| deref(doc, o)).and_then(pdf_string);
                } else if s == b"GoTo" || s == b"GoToR" {
                    if let Ok(d) = act.get(b"D") {
                        let (p, n) = resolve_dest(doc, d, &page_no, &named);
                        dest_page = p;
                        dest_name = n;
                    }
                }
            } else if let Ok(d) = ad.get(b"Dest") {
                let (p, n) = resolve_dest(doc, d, &page_no, &named);
                dest_page = p;
                dest_name = n;
            }

            if uri.is_some() || dest_page.is_some() || dest_name.is_some() {
                // Normalise rect to x0<=x1, y0<=y1.
                let r = [rect[0].min(rect[2]), rect[1].min(rect[3]), rect[0].max(rect[2]), rect[1].max(rect[3])];
                out.push(Link { page: pno, rect: r, uri, dest_page, dest_name });
            }
        }
    }
    out
}
