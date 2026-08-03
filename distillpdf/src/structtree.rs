//! **L0 — the table structure a PDF *declares***.
//!
//! A tagged PDF publishes its logical structure in `/StructTreeRoot`: `/Table` → `/TR` →
//! `/TH`/`/TD`, with `/RowSpan`/`/ColSpan` attributes and, at the leaves, the *marked-content
//! identifiers* (`/MCID`) that tie each cell to the exact glyphs on the page. Where that
//! declaration exists, rows, columns, header cells and spans are **stated**, not inferred —
//! there is no threshold to tune and no false positive to risk.
//!
//! This module reads the declaration and nothing else. It answers "what does the file say is
//! here", never "is the file right"; the trust rule that decides whether a declaration is
//! usable lives at the call site ([`crate::html`]), because only there are the page's actual
//! spans available to resolve the `/MCID`s against. That separation is deliberate — a walker
//! that also judged would have to be re-run to change the judgement.
//!
//! **Scope, measured.** In the 88-document bench100 corpus, 38 documents carry a
//! `/StructTreeRoot` and 27 carry `/Table` elements, covering 31 of the 84 ground-truth table
//! pages — including 23 of the 46 full-grid pages, the shape inference is weakest on. An
//! untagged document never reaches past the first statement of [`declared_tables`], which is
//! why L0 cannot regress the inference paths: it has nothing to say about them.
//!
//! **Not gospel.** The same corpus carries declarations that fragment (one World Bank table
//! is declared as three shards, two of them a single row), declarations whose cells resolve
//! to no content on the page, and row counts that disagree with a human reading. Hence the
//! trust rule — and hence the caps below, because a structure tree is attacker-controlled
//! data like every other part of the file.

use crate::pdfobj::deref;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::{HashMap, HashSet};

/// One declared table cell.
pub(crate) struct DeclaredCell {
    /// `/TH` rather than `/TD`.
    pub header: bool,
    /// `/RowSpan` (default 1), clamped.
    pub rowspan: usize,
    /// `/ColSpan` (default 1), clamped.
    pub colspan: usize,
    /// Marked-content ids of this cell's content **on the page this table was filed under**.
    pub mcids: Vec<u32>,
    /// `/OBJR` referents — a widget annotation carrying the cell's value, which is how every
    /// fillable form in the corpus (all of the IRS ones) files its data cells.
    pub objs: Vec<ObjectId>,
}

/// A declared table, restricted to one page.
///
/// A `/Table` that runs across a page break is filed under **each** page it reaches, carrying
/// only the rows with content there — the same table read once per page, which is how a
/// page-at-a-time renderer has to see it.
pub(crate) struct DeclaredTable {
    pub rows: Vec<Vec<DeclaredCell>>,
}

impl DeclaredTable {
    /// Column count after span expansion: the widest row's summed `/ColSpan`.
    pub(crate) fn cols(&self) -> usize {
        self.rows.iter().map(|r| r.iter().map(|c| c.colspan).sum::<usize>()).max().unwrap_or(0)
    }
}

/// Every `/Table` the document declares, keyed by the page object it draws on.
///
/// Empty for an untagged document, reached without touching an object beyond the catalogue
/// lookup — the fast path that makes L0 inert where there is no declaration.
pub(crate) fn declared_tables(doc: &Document) -> HashMap<ObjectId, Vec<DeclaredTable>> {
    let mut out: HashMap<ObjectId, Vec<DeclaredTable>> = HashMap::new();
    let Some(root) = doc.catalog().ok().and_then(|c| sub_dict_of(doc, c, b"StructTreeRoot")).cloned() else {
        return out;
    };
    let mut w = Walk { doc, roles: role_map(doc, &root), classes: class_map(doc, &root), nodes: 0, visited: HashSet::new() };
    let mut tables: Vec<RawTable> = Vec::new();
    let pg = page_of(&root);
    for k in kids(doc, &root) {
        w.find_tables(&k, 0, pg, &mut tables);
    }
    for t in tables {
        for (page, tbl) in t.split_by_page() {
            out.entry(page).or_default().push(tbl);
        }
    }
    out
}

/// A `/Table` as declared, before the per-page split: every content reference carries the
/// page it lives on, because an element's `/Pg` is inherited and a cell may straddle a break.
struct RawTable {
    rows: Vec<Vec<RawCell>>,
}

struct RawCell {
    header: bool,
    rowspan: usize,
    colspan: usize,
    mcids: Vec<(ObjectId, u32)>,
    objs: Vec<(ObjectId, ObjectId)>,
}

impl RawCell {
    fn pages(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.mcids.iter().map(|(p, _)| *p).chain(self.objs.iter().map(|(p, _)| *p))
    }
}

impl RawTable {
    /// One [`DeclaredTable`] per page this table reaches, carrying that page's rows.
    ///
    /// A row is "on" a page when at least one of its cells has content there. Cells with no
    /// content at all stay in the row — an empty `/TD` is still a column, and dropping it
    /// would silently narrow the declared grid on exactly the documents that need L0 most
    /// (an IRS form is mostly empty cells by design).
    fn split_by_page(self) -> Vec<(ObjectId, DeclaredTable)> {
        let mut pages: Vec<ObjectId> = Vec::new();
        for p in self.rows.iter().flatten().flat_map(RawCell::pages) {
            if !pages.contains(&p) {
                pages.push(p);
            }
        }
        pages
            .into_iter()
            .map(|pg| {
                let rows: Vec<Vec<DeclaredCell>> = self
                    .rows
                    .iter()
                    .filter(|r| r.iter().any(|c| c.pages().any(|p| p == pg)))
                    .map(|r| {
                        r.iter()
                            .map(|c| DeclaredCell {
                                header: c.header,
                                rowspan: c.rowspan,
                                colspan: c.colspan,
                                mcids: c.mcids.iter().filter(|(p, _)| *p == pg).map(|(_, m)| *m).collect(),
                                objs: c.objs.iter().filter(|(p, _)| *p == pg).map(|(_, o)| *o).collect(),
                            })
                            .collect()
                    })
                    .collect();
                (pg, DeclaredTable { rows })
            })
            .filter(|(_, t)| !t.rows.is_empty())
            .collect()
    }
}

struct Walk<'a> {
    doc: &'a Document,
    roles: HashMap<Vec<u8>, Vec<u8>>,
    classes: HashMap<Vec<u8>, (usize, usize)>,
    /// Structure elements entered — the work bound (see [`crate::MAX_STRUCT_NODES`]).
    nodes: usize,
    /// Indirect elements already entered, so a `/K` cycle terminates. Direct (inline)
    /// dictionaries cannot form a cycle and are not tracked.
    visited: HashSet<ObjectId>,
}

impl Walk<'_> {
    /// The structure type of an element, with `/RoleMap` applied. A producer may name its
    /// types anything (`/TableCellHeading`, as USGS does) as long as the role map says what
    /// they mean. One hop only: a map that sends A→B→A is malformed, and chasing it would
    /// put a cycle in the hot path for nothing — the spec's own example is a single hop.
    fn role(&self, d: &Dictionary) -> Vec<u8> {
        let Some(raw) = d.get(b"S").ok().and_then(|o| deref(self.doc, o)).and_then(|o| o.as_name().ok()) else {
            return Vec::new();
        };
        self.roles.get(raw).cloned().unwrap_or_else(|| raw.to_vec())
    }

    /// Resolve an object to a structure-element dictionary, honouring the cycle guard and the
    /// node budget. `None` means "stop here" — exhausted, cyclic, or not an element.
    fn elem(&mut self, o: &Object) -> Option<Dictionary> {
        if self.nodes >= crate::MAX_STRUCT_NODES {
            return None;
        }
        self.nodes += 1;
        match o {
            Object::Dictionary(d) => Some(d.clone()),
            Object::Reference(id) => {
                if !self.visited.insert(*id) {
                    return None;
                }
                self.doc.get_dictionary(*id).ok().cloned()
            }
            _ => None,
        }
    }

    /// Depth-first hunt for `/Table` elements, carrying the inherited `/Pg`.
    fn find_tables(&mut self, o: &Object, depth: u32, pg: Option<ObjectId>, out: &mut Vec<RawTable>) {
        if depth > crate::MAX_STRUCT_DEPTH || out.len() >= crate::MAX_STRUCT_TABLES {
            return;
        }
        let Some(d) = self.elem(o) else { return };
        let pg = page_of(&d).or(pg);
        if self.role(&d) == b"Table" {
            let mut rows = Vec::new();
            self.read_rows(&d, pg, 0, &mut rows);
            if !rows.is_empty() {
                out.push(RawTable { rows });
            }
            // A table nested in a table cell is legal and vanishingly rare; the outer
            // declaration already covers the region, so descending would double-count it.
            return;
        }
        for k in kids(self.doc, &d) {
            self.find_tables(&k, depth + 1, pg, out);
        }
    }

    /// Rows of one `/Table`: its own `/TR` kids, plus those inside `/THead`/`/TBody`/`/TFoot`.
    fn read_rows(&mut self, d: &Dictionary, pg: Option<ObjectId>, depth: u32, rows: &mut Vec<Vec<RawCell>>) {
        if depth > crate::MAX_STRUCT_DEPTH {
            return;
        }
        for k in kids(self.doc, d) {
            if rows.len() >= crate::MAX_STRUCT_ROWS {
                return;
            }
            let Some(kd) = self.elem(&k) else { continue };
            let kpg = page_of(&kd).or(pg);
            match self.role(&kd).as_slice() {
                b"TR" => {
                    let cells = self.read_cells(&kd, kpg);
                    if !cells.is_empty() {
                        rows.push(cells);
                    }
                }
                b"THead" | b"TBody" | b"TFoot" => self.read_rows(&kd, kpg, depth + 1, rows),
                _ => {}
            }
        }
    }

    fn read_cells(&mut self, row: &Dictionary, pg: Option<ObjectId>) -> Vec<RawCell> {
        let mut cells = Vec::new();
        for k in kids(self.doc, row) {
            if cells.len() >= crate::MAX_STRUCT_COLS {
                break;
            }
            let Some(cd) = self.elem(&k) else { continue };
            let role = self.role(&cd);
            let header = role == b"TH";
            if !header && role != b"TD" {
                continue;
            }
            let cpg = page_of(&cd).or(pg);
            let (rowspan, colspan) = self.spans_of(&cd);
            let mut c = RawCell { header, rowspan, colspan, mcids: Vec::new(), objs: Vec::new() };
            self.harvest(&cd, cpg, 0, &mut c);
            cells.push(c);
        }
        cells
    }

    /// `/RowSpan` and `/ColSpan`, from the element's `/A` attribute dictionaries (one, or an
    /// array of them) and from its `/C` class, clamped so a hostile `/ColSpan 2000000000`
    /// cannot become an allocation.
    fn spans_of(&self, d: &Dictionary) -> (usize, usize) {
        let clamp = |v: usize| v.clamp(1, crate::MAX_STRUCT_COLS);
        let (mut rs, mut cs) = (1usize, 1usize);
        if let Some(cls) = d.get(b"C").ok().and_then(|o| deref(self.doc, o)).and_then(|o| o.as_name().ok()) {
            if let Some(&(r, c)) = self.classes.get(cls) {
                rs = r;
                cs = c;
            }
        }
        let doc = self.doc;
        let mut apply = |o: &Object| {
            if let Some(a) = deref(doc, o).and_then(|o| o.as_dict().ok()) {
                if let Some(v) = int_key(doc, a, b"RowSpan") {
                    rs = clamp(v);
                }
                if let Some(v) = int_key(doc, a, b"ColSpan") {
                    cs = clamp(v);
                }
            }
        };
        if let Some(a) = d.get(b"A").ok().and_then(|o| deref(doc, o)) {
            match a {
                Object::Array(items) => items.iter().take(crate::MAX_STRUCT_COLS).for_each(&mut apply),
                other => apply(other),
            }
        }
        (clamp(rs), clamp(cs))
    }

    /// A cell's content references: `/MCID` integers (bare kids, or `/MCR` dictionaries) and
    /// `/OBJR` referents. Descends through whatever wrappers the producer put between the
    /// cell and its content (`/P`, `/Span`, `/Link`, a vendor role, …).
    fn harvest(&mut self, d: &Dictionary, pg: Option<ObjectId>, depth: u32, out: &mut RawCell) {
        if depth > crate::MAX_STRUCT_DEPTH {
            return;
        }
        for k in kids(self.doc, d) {
            if out.mcids.len() + out.objs.len() >= crate::MAX_STRUCT_CELL_REFS {
                return;
            }
            // An integer kid is an MCID in the content stream of the inherited `/Pg` — and it
            // may be *indirect*. Every World Bank document in the corpus writes its cell
            // content as `/K [521 0 R]` where object 521 is the integer `2`; reading only
            // direct integers found 34 declared tables in that file and not one cell's
            // content, which the trust rule then correctly (and uselessly) refused.
            match deref(self.doc, &k) {
                Some(Object::Integer(n)) => {
                    if let (Some(p), Ok(m)) = (pg, u32::try_from(*n)) {
                        out.mcids.push((p, m));
                    }
                }
                _ => {
                    let Some(kd) = self.elem(&k) else { continue };
                    let kpg = page_of(&kd).or(pg);
                    if let (Some(p), Some(m)) = (kpg, int_key(self.doc, &kd, b"MCID").and_then(|m| u32::try_from(m).ok())) {
                        out.mcids.push((p, m));
                    } else if let (Some(p), Ok(Object::Reference(id))) = (kpg, kd.get(b"Obj")) {
                        out.objs.push((p, *id));
                    } else {
                        self.harvest(&kd, kpg, depth + 1, out);
                    }
                }
            }
        }
    }
}

/// `/K`, normalised to a list: the spec allows a single kid, an array, or nothing.
fn kids(doc: &Document, d: &Dictionary) -> Vec<Object> {
    match d.get(b"K").ok().and_then(|o| deref(doc, o)) {
        Some(Object::Array(a)) => a.iter().take(crate::MAX_STRUCT_KIDS).cloned().collect(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    }
}

/// The `/Pg` an element names, if any — inherited by its kids (§14.7.4.2). Read raw: a
/// `deref` would resolve the reference and lose the identity we need.
fn page_of(d: &Dictionary) -> Option<ObjectId> {
    match d.get(b"Pg") {
        Ok(Object::Reference(id)) => Some(*id),
        _ => None,
    }
}

fn int_key(doc: &Document, d: &Dictionary, key: &[u8]) -> Option<usize> {
    usize::try_from(d.get(key).ok().and_then(|o| deref(doc, o))?.as_i64().ok()?).ok()
}

fn sub_dict_of<'a>(doc: &'a Document, d: &'a Dictionary, key: &[u8]) -> Option<&'a Dictionary> {
    d.get(key).ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok())
}

fn role_map(doc: &Document, root: &Dictionary) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut m = HashMap::new();
    if let Some(rm) = sub_dict_of(doc, root, b"RoleMap") {
        for (k, v) in rm.iter().take(crate::MAX_STRUCT_KIDS) {
            if let Some(n) = deref(doc, v).and_then(|o| o.as_name().ok()) {
                m.insert(k.to_vec(), n.to_vec());
            }
        }
    }
    m
}

/// `/ClassMap`: named attribute sets a `/C` may point at instead of spelling `/A` out.
fn class_map(doc: &Document, root: &Dictionary) -> HashMap<Vec<u8>, (usize, usize)> {
    let mut m = HashMap::new();
    let Some(cm) = sub_dict_of(doc, root, b"ClassMap") else { return m };
    for (k, v) in cm.iter().take(crate::MAX_STRUCT_KIDS) {
        let (mut rs, mut cs) = (None, None);
        let mut read = |o: &Object| {
            if let Some(a) = deref(doc, o).and_then(|o| o.as_dict().ok()) {
                rs = int_key(doc, a, b"RowSpan").or(rs);
                cs = int_key(doc, a, b"ColSpan").or(cs);
            }
        };
        match deref(doc, v) {
            Some(Object::Array(items)) => items.iter().take(crate::MAX_STRUCT_COLS).for_each(&mut read),
            Some(other) => read(other),
            None => {}
        }
        if rs.is_some() || cs.is_some() {
            m.insert(k.to_vec(), (rs.unwrap_or(1).clamp(1, crate::MAX_STRUCT_COLS), cs.unwrap_or(1).clamp(1, crate::MAX_STRUCT_COLS)));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    /// A document whose catalogue points at `root`, with one page object the tree can name.
    fn doc_with_tree(build: impl FnOnce(&mut Document, ObjectId) -> ObjectId) -> Document {
        let mut doc = Document::with_version("1.5");
        let page = doc.add_object(dictionary! { "Type" => "Page" });
        let root = build(&mut doc, page);
        let pages = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 });
        let cat = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages, "StructTreeRoot" => root
        });
        doc.trailer.set("Root", Object::Reference(cat));
        doc
    }

    /// `/TD` with the given MCID kids, as a direct element dictionary.
    fn td(page: ObjectId, mcids: Vec<Object>) -> Object {
        Object::Dictionary(dictionary! { "S" => "TD", "Pg" => page, "K" => mcids })
    }

    fn tr(cells: Vec<Object>) -> Object {
        Object::Dictionary(dictionary! { "S" => "TR", "K" => cells })
    }

    fn table(rows: Vec<Object>) -> Object {
        Object::Dictionary(dictionary! { "S" => "Table", "K" => rows })
    }

    #[test]
    fn a_cell_that_names_its_content_indirectly_still_resolves() {
        // `/K [N 0 R]` where object N is an integer. Legal (PDF 32000-1 §7.3.10 — only
        // content-stream operands may not be indirect) and the shape EVERY World Bank
        // document in the measurement corpus uses. Reading direct integers only found 34
        // declared tables in one such file and not one cell's content, so the trust rule
        // refused the lot and L0 was inert on a whole source.
        let doc = doc_with_tree(|doc, page| {
            let indirect = doc.add_object(Object::Integer(7));
            let t = table(vec![
                tr(vec![td(page, vec![Object::Integer(1)]), td(page, vec![Object::Reference(indirect)])]),
                tr(vec![td(page, vec![Object::Integer(2)]), td(page, vec![Object::Integer(3)])]),
            ]);
            doc.add_object(dictionary! { "K" => t })
        });
        let found = declared_tables(&doc);
        let page = *doc.get_pages().get(&1).expect("one page");
        let t = &found.get(&page).expect("the page carries the declaration")[0];
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0][1].mcids, vec![7], "the indirect MCID must resolve like a direct one");
    }

    #[test]
    fn a_producers_own_type_names_are_read_through_the_role_map() {
        // USGS names its cells `/TableCellHeading`; a role map is the file's own dictionary
        // for what its vocabulary means, and ignoring it loses the table entirely.
        let doc = doc_with_tree(|doc, page| {
            let cell = Object::Dictionary(dictionary! { "S" => "MyCell", "Pg" => page, "K" => vec![Object::Integer(0)] });
            let row = Object::Dictionary(dictionary! { "S" => "MyRow", "K" => vec![cell.clone()] });
            let row2 = Object::Dictionary(dictionary! { "S" => "MyRow", "K" => vec![cell] });
            let t = Object::Dictionary(dictionary! { "S" => "MyTable", "K" => vec![row, row2] });
            doc.add_object(dictionary! {
                "RoleMap" => dictionary! { "MyTable" => "Table", "MyRow" => "TR", "MyCell" => "TH" },
                "K" => t
            })
        });
        let page = *doc.get_pages().get(&1).expect("one page");
        let found = declared_tables(&doc);
        let t = &found.get(&page).expect("role-mapped table found")[0];
        assert_eq!(t.rows.len(), 2);
        assert!(t.rows[0][0].header, "a role-mapped /TH is still a header cell");
    }

    #[test]
    fn rows_inside_a_tbody_belong_to_the_table() {
        // The World Bank shape: `/Table` → `/TBody` → `/TR`. A walk that only looks for `/TR`
        // among a table's direct kids reads every such table as having no rows at all.
        let doc = doc_with_tree(|doc, page| {
            let body = Object::Dictionary(dictionary! { "S" => "TBody", "K" => vec![
                tr(vec![td(page, vec![Object::Integer(0)]), td(page, vec![Object::Integer(1)])]),
                tr(vec![td(page, vec![Object::Integer(2)]), td(page, vec![Object::Integer(3)])]),
            ]});
            doc.add_object(dictionary! { "K" => table(vec![body]) })
        });
        let page = *doc.get_pages().get(&1).expect("one page");
        assert_eq!(declared_tables(&doc)[&page][0].rows.len(), 2);
    }

    #[test]
    fn spans_come_from_the_attribute_dictionary_and_are_clamped() {
        let doc = doc_with_tree(|doc, page| {
            let wide = Object::Dictionary(dictionary! {
                "S" => "TH", "Pg" => page, "K" => vec![Object::Integer(0)],
                "A" => dictionary! { "O" => "Table", "ColSpan" => 4, "RowSpan" => 2 }
            });
            let hostile = Object::Dictionary(dictionary! {
                "S" => "TD", "Pg" => page, "K" => vec![Object::Integer(1)],
                "A" => vec![Object::Dictionary(dictionary! { "O" => "Table", "ColSpan" => 2_000_000_000i64 })]
            });
            doc.add_object(dictionary! { "K" => table(vec![tr(vec![wide]), tr(vec![hostile])]) })
        });
        let page = *doc.get_pages().get(&1).expect("one page");
        let t = &declared_tables(&doc)[&page][0];
        assert_eq!((t.rows[0][0].rowspan, t.rows[0][0].colspan), (2, 4));
        assert_eq!(t.rows[1][0].colspan, crate::MAX_STRUCT_COLS, "a hostile span is clamped, never allocated");
        assert_eq!(t.cols(), crate::MAX_STRUCT_COLS);
    }

    #[test]
    fn a_cyclic_structure_tree_terminates() {
        // `/K` is a general pointer in a hostile file: an element may name an ancestor. The
        // cycle guard is for indirect elements, which is the only way a cycle can be written.
        let mut doc = Document::with_version("1.5");
        let page = doc.add_object(dictionary! { "Type" => "Page" });
        let a = doc.add_object(dictionary! { "S" => "Sect", "Pg" => page });
        let b = doc.add_object(dictionary! { "S" => "Sect", "Pg" => page, "K" => vec![Object::Reference(a)] });
        if let Ok(d) = doc.get_object_mut(a).and_then(|o| o.as_dict_mut()) {
            d.set("K", vec![Object::Reference(b)]);
        }
        let root = doc.add_object(dictionary! { "K" => vec![Object::Reference(a)] });
        let pages = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 });
        let cat = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages, "StructTreeRoot" => root
        });
        doc.trailer.set("Root", Object::Reference(cat));
        assert!(declared_tables(&doc).is_empty(), "a cycle must terminate with no tables, not hang");
    }

    #[test]
    fn an_untagged_document_costs_one_lookup_and_yields_nothing() {
        let mut doc = Document::with_version("1.5");
        let page = doc.add_object(dictionary! { "Type" => "Page" });
        let pages = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 });
        let cat = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", Object::Reference(cat));
        assert!(declared_tables(&doc).is_empty());
    }

    #[test]
    fn a_table_that_straddles_a_page_break_is_filed_under_both_pages() {
        // Its rows are split by where their content actually lives, so a page-at-a-time
        // renderer sees the part of the table that is on the page it is rendering.
        let mut doc = Document::with_version("1.5");
        let p1 = doc.add_object(dictionary! { "Type" => "Page" });
        let p2 = doc.add_object(dictionary! { "Type" => "Page" });
        let t = table(vec![
            tr(vec![td(p1, vec![Object::Integer(0)]), td(p1, vec![Object::Integer(1)])]),
            tr(vec![td(p1, vec![Object::Integer(2)]), td(p1, vec![Object::Integer(3)])]),
            tr(vec![td(p2, vec![Object::Integer(0)]), td(p2, vec![Object::Integer(1)])]),
        ]);
        let root = doc.add_object(dictionary! { "K" => t });
        let pages = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![p1.into(), p2.into()], "Count" => 2 });
        let cat = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages, "StructTreeRoot" => root
        });
        doc.trailer.set("Root", Object::Reference(cat));
        let found = declared_tables(&doc);
        assert_eq!(found[&p1][0].rows.len(), 2);
        assert_eq!(found[&p2][0].rows.len(), 1);
    }
}
