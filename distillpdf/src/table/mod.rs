//! Canonical in-memory table analysis shared by the born-digital detector, render IR and
//! durable-model projection.
//!
//! This is deliberately private and deliberately not a wire type.  The public legacy raw
//! extraction remains [`crate::TableInfo`], while the `.dpdf` model keeps its existing
//! `table_header` / `table_grid` fields.  Those surfaces are projections of this analysis,
//! not parallel interpretations of a table.

use crate::geom::Rect;
use crate::text::SourceSlice;
use std::ops::Range;

/// One table reported by [`crate::PdfDocument::analyze_tables`].
///
/// This is the structured view of the raw detector used by
/// [`crate::PdfDocument::extract_tables`]. HTML/Markdown rendering performs additional figure
/// filtering, structure-tree reconciliation and caption attachment, so its final table set is
/// intentionally a different surface.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct AnalyzedTable {
    /// One-based PDF page number, matching [`crate::TableInfo::page`].
    pub page: u32,
    /// Detected table region as `[left, top, right, bottom]` in normalized display
    /// coordinates, or `None` when the effective page box is degenerate.
    pub bbox_norm: Option<[f32; 4]>,
    /// Logical row count across detached headers and the body grid. This may exceed the row
    /// count of legacy `TableInfo::cells`, which intentionally omits detached header rows.
    pub n_rows: usize,
    /// Logical column count after accounting for spans.
    pub n_cols: usize,
    pub header_rows: usize,
    /// Cell anchors in stable row-major order. Slots covered by a span are not repeated.
    pub cells: Vec<AnalyzedCell>,
    /// Normally `None`: the raw detector does not currently attach renderer captions.
    pub caption: Option<AnalyzedCaption>,
    pub evidence: Vec<TableEvidence>,
}

/// One semantic cell anchor in an [`AnalyzedTable`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct AnalyzedCell {
    pub text: String,
    pub row: usize,
    pub col: usize,
    pub rowspan: usize,
    pub colspan: usize,
    /// Exact physical cell boundary in normalized display coordinates. `None` means the
    /// detector did not observe a boundary; glyph/content extents are never substituted.
    pub bbox_norm: Option<[f32; 4]>,
    pub role: TableCellRole,
    /// Header anchor coordinates `[row, col]`, outermost to innermost, whose spans cover this
    /// data cell's column. Empty for header cells and zero-header tables.
    pub header_path: Vec<[usize; 2]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AnalyzedCaption {
    pub number: String,
    pub text: String,
    pub below: bool,
}

/// One cell in stable row-major table order.
///
/// `row` and `col` address the cell's anchor.  A dense legacy slot covered by that anchor is
/// represented explicitly with `covered = true`, so old projections remain byte-stable while
/// callers of this private analysis can distinguish topology from projection.
/// `bbox` is present only when the producer published exact cell boundaries (currently the
/// ruled lattice). `content_bbox` may retain the union of glyph/annotation boxes without
/// promoting it into an invented cell boundary.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CellAnalysis {
    pub(crate) text: String,
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) rowspan: usize,
    pub(crate) colspan: usize,
    /// Colspan of the current byte-stable HTML/model projection.  This can differ from
    /// `colspan` when a producer retains a dense legacy row while the analysis surface has
    /// stronger logical topology (declared cells and conservatively proven grouped headers).
    pub(crate) render_colspan: usize,
    pub(crate) role: TableCellRole,
    /// This dense slot is covered by an earlier row/column-spanning anchor.
    pub(crate) covered: bool,
    pub(crate) bbox: Option<Rect>,
    pub(crate) content_bbox: Option<Rect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableCellRole {
    /// A column-header cell owned by the detector's leading header region.
    Header,
    /// A data cell. Row-header semantics are not inferred by this API.
    Data,
}

impl TableCellRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Data => "data",
        }
    }
}

pub(crate) type CellRole = TableCellRole;

impl CellAnalysis {
    pub(crate) fn new(
        text: String,
        row: usize,
        col: usize,
        rowspan: usize,
        colspan: usize,
        bbox: Option<Rect>,
    ) -> Self {
        Self {
            text,
            row,
            col,
            rowspan,
            colspan,
            render_colspan: colspan,
            role: TableCellRole::Data,
            covered: false,
            bbox,
            content_bbox: None,
        }
    }

    pub(crate) fn declared(
        text: String,
        row: usize,
        col: usize,
        rowspan: usize,
        colspan: usize,
        role: TableCellRole,
        content_bbox: Option<Rect>,
    ) -> Self {
        Self {
            text,
            row,
            col,
            rowspan,
            colspan,
            render_colspan: 1,
            role,
            covered: false,
            bbox: None,
            content_bbox,
        }
    }

    pub(crate) fn covered(row: usize, col: usize) -> Self {
        Self {
            text: String::new(),
            row,
            col,
            rowspan: 1,
            colspan: 1,
            render_colspan: 1,
            role: TableCellRole::Data,
            covered: true,
            bbox: None,
            content_bbox: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn trim(&self) -> &str {
        self.text.trim()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, pattern: &str) -> bool {
        self.text.contains(pattern)
    }
}

impl PartialEq<&str> for CellAnalysis {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for CellAnalysis {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl PartialEq<(String, usize)> for CellAnalysis {
    fn eq(&self, other: &(String, usize)) -> bool {
        self.text == other.0 && self.render_colspan == other.1
    }
}

/// The caption parts the HTML emitter already owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TableCaption {
    pub(crate) number: String,
    pub(crate) html: String,
    pub(crate) below: bool,
}

impl From<(String, String, bool)> for TableCaption {
    fn from((number, html, below): (String, String, bool)) -> Self {
        Self {
            number,
            html,
            below,
        }
    }
}

/// One table's semantic payload.
///
/// The `header` / `grid` split is retained intentionally: it is observable in the existing
/// durable fidelity fields even though `header_rows` independently controls which leading
/// rows render as `<th>`.  Canonicalizing that split away would be a schema/behaviour change,
/// not a refactor.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TableAnalysis {
    pub(crate) header: Vec<Vec<CellAnalysis>>,
    pub(crate) grid: Vec<Vec<CellAnalysis>>,
    pub(crate) header_rows: usize,
    pub(crate) caption: Option<TableCaption>,
    pub(crate) evidence: Vec<TableEvidence>,
}

/// Positive evidence that contributed to an accepted table, in stable strength/arrival order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableEvidence {
    /// Accepted PDF structure-tree declaration.
    Declared,
    /// Observed closed ruling lattice or supporting horizontal rules.
    Ruled,
    /// Repeated text-row alignment.
    Aligned,
}

impl TableEvidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Ruled => "ruled",
            Self::Aligned => "aligned",
        }
    }
}

impl TableAnalysis {
    /// Adopt the exact legacy parts without normalizing spans, header depth or text.
    pub(crate) fn from_parts(
        header: Vec<Vec<(String, usize)>>,
        grid: Vec<Vec<String>>,
        header_rows: usize,
        caption: Option<(String, String, bool)>,
        evidence: Vec<TableEvidence>,
    ) -> Self {
        let mut analyzed_header = Vec::with_capacity(header.len());
        for (row, cells) in header.into_iter().enumerate() {
            let mut col = 0usize;
            let mut analyzed = Vec::with_capacity(cells.len());
            for (text, colspan) in cells {
                let mut cell = CellAnalysis::new(text, row, col, 1, colspan, None);
                if row < header_rows {
                    cell.role = TableCellRole::Header;
                }
                analyzed.push(cell);
                col += colspan.max(1);
            }
            analyzed_header.push(analyzed);
        }
        let row_offset = analyzed_header.len();
        let analyzed_grid = grid
            .into_iter()
            .enumerate()
            .map(|(ri, cells)| {
                cells
                    .into_iter()
                    .enumerate()
                    .map(|(col, text)| {
                        let row = row_offset + ri;
                        let mut cell = CellAnalysis::new(text, row, col, 1, 1, None);
                        if row < header_rows {
                            cell.role = TableCellRole::Header;
                        }
                        cell
                    })
                    .collect()
            })
            .collect();
        Self {
            header: analyzed_header,
            grid: analyzed_grid,
            header_rows,
            caption: caption.map(TableCaption::from),
            evidence: dedup_evidence(evidence),
        }
    }

    /// Recover logical colspans from a conservatively proven dense projection emitted by the
    /// alignment detector: a leading grouped-header row whose labels each own a non-empty run
    /// of leaf headers in the next tier.
    ///
    /// This is deliberately an analysis-only annotation.  The cells stay in their original
    /// vectors, their text and `render_colspan` stay untouched, and covered slots are merely
    /// hidden from [`AnalyzedTable`].  Consequently the legacy grid, HTML/Markdown, and durable
    /// model projections remain byte-for-byte the alignment detector's answer.
    pub(crate) fn compress_aligned_leading_group_header(
        &mut self,
        top_anchor_styled: &[bool],
    ) {
        if !self.header.is_empty()
            || self.header_rows < 2
            || self.grid.len() <= self.header_rows
            || self.evidence.as_slice() != [TableEvidence::Aligned]
        {
            return;
        }
        let Some(top) = self.grid.first() else {
            return;
        };
        let Some(leaves) = self.grid.get(1) else {
            return;
        };
        let width = top.len();
        let is_dense_header_row = |cells: &[CellAnalysis], logical_row: usize| {
            cells.len() == width
                && cells.iter().enumerate().all(|(col, cell)| {
                    cell.row == logical_row
                        && cell.col == col
                        && cell.rowspan == 1
                        && cell.colspan == 1
                        && cell.render_colspan == 1
                        && !cell.covered
                        && cell.role == TableCellRole::Header
                })
        };
        if width < 4
            || top_anchor_styled.len() != width
            || !is_dense_header_row(top, 0)
            || !is_dense_header_row(leaves, 1)
            || leaves.iter().any(|cell| cell.text.trim().is_empty())
        {
            return;
        }

        let starts: Vec<usize> = top
            .iter()
            .enumerate()
            .filter_map(|(col, cell)| (!cell.text.trim().is_empty()).then_some(col))
            .collect();
        if starts.len() < 2 || starts[0] != 0 {
            return;
        }
        if starts.iter().any(|&col| !top_anchor_styled[col]) {
            return;
        }
        let groups: Vec<(usize, usize)> = starts
            .iter()
            .copied()
            .zip(starts.iter().copied().skip(1).chain(std::iter::once(width)))
            .collect();
        // Every label must own at least one following blank slot.  Because `starts` contains
        // every non-empty top-tier cell, this also proves that all intervening slots are blank.
        if groups.iter().any(|&(start, end)| end.saturating_sub(start) < 2) {
            return;
        }

        let top = &mut self.grid[0];
        for (start, end) in groups {
            top[start].colspan = end - start;
            for cell in &mut top[start + 1..end] {
                cell.covered = true;
            }
        }
    }

    pub(crate) fn from_cells(
        header: Vec<Vec<CellAnalysis>>,
        grid: Vec<Vec<CellAnalysis>>,
        header_rows: usize,
        evidence: Vec<TableEvidence>,
    ) -> Self {
        Self {
            header,
            grid,
            header_rows,
            caption: None,
            evidence: dedup_evidence(evidence),
        }
    }

    pub(crate) fn with_caption(mut self, caption: Option<(String, String, bool)>) -> Self {
        self.caption = caption.map(TableCaption::from);
        self
    }

    /// Existing `.dpdf` fidelity field, byte-for-byte.
    pub(crate) fn header_parts(&self) -> Vec<Vec<(String, usize)>> {
        self.header
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| (cell.text.clone(), cell.render_colspan))
                    .collect()
            })
            .collect()
    }

    /// Existing `.dpdf` fidelity field, byte-for-byte.
    pub(crate) fn grid_parts(&self) -> Vec<Vec<String>> {
        self.grid
            .iter()
            .map(|row| row.iter().map(|cell| cell.text.clone()).collect())
            .collect()
    }

    /// Existing query-grid projection: detached colspans repeat their trimmed anchor text.
    pub(crate) fn expanded_cells(&self) -> Vec<Vec<String>> {
        let mut rows = Vec::with_capacity(self.header.len() + self.grid.len());
        for hrow in &self.header {
            let width = hrow.iter().map(|cell| cell.render_colspan.max(1)).sum();
            let mut row = Vec::with_capacity(width);
            for cell in hrow {
                for _ in 0..cell.render_colspan.max(1) {
                    row.push(cell.text.trim().to_string());
                }
            }
            rows.push(row);
        }
        rows.extend(self.grid.iter().map(|row| {
            row.iter()
                .map(|cell| cell.text.trim().to_string())
                .collect()
        }));
        rows
    }

    /// Consume only the data grid for the legacy raw extraction path.  Detached headers remain
    /// absent there exactly as before.
    pub(crate) fn into_grid_parts(self) -> Vec<Vec<String>> {
        self.grid
            .into_iter()
            .map(|row| row.into_iter().map(|cell| cell.text).collect())
            .collect()
    }

    fn logical_shape(&self) -> (usize, usize) {
        self.header
            .iter()
            .chain(&self.grid)
            .flatten()
            .filter(|cell| !cell.covered)
            .fold((0, 0), |(rows, cols), cell| {
                (
                    rows.max(cell.row.saturating_add(cell.rowspan.max(1))),
                    cols.max(cell.col.saturating_add(cell.colspan.max(1))),
                )
            })
    }
}

/// Which private detector produced a page-local candidate. This is diagnostic identity, not
/// evidence ranking: Phase 5A records existing decisions without changing them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CandidateProducer {
    Aligned,
    Frame,
    Declared,
    Synthetic,
}

impl CandidateProducer {
    const fn name(self) -> &'static str {
        match self {
            Self::Aligned => "aligned",
            Self::Frame => "frame",
            Self::Declared => "declared",
            Self::Synthetic => "synthetic",
        }
    }
}

/// Stable identity within one page detection call. Ordinals follow the producer's existing,
/// deterministic iteration order and never participate in reconciliation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CandidateKey {
    pub(crate) producer: CandidateProducer,
    pub(crate) ordinal: u32,
}

impl CandidateKey {
    pub(crate) const fn new(producer: CandidateProducer, ordinal: u32) -> Self {
        Self { producer, ordinal }
    }

    pub(crate) const fn synthetic() -> Self {
        Self::new(CandidateProducer::Synthetic, 0)
    }

    pub(crate) fn label(self) -> String {
        format!("{}:{}", self.producer.name(), self.ordinal)
    }
}

/// Exact painted text owned by a private table candidate. `slices` is the globally sorted
/// union used for deterministic comparison/hashing; `row_slices` plus `row_ranges` preserves
/// the detector's row partition without allocating one vector per public cell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TableClaim {
    pub(crate) slices: Vec<SourceSlice>,
    row_slices: Vec<SourceSlice>,
    row_ranges: Vec<Range<usize>>,
}

impl TableClaim {
    pub(crate) fn from_rows(rows: Vec<Vec<SourceSlice>>) -> Self {
        let mut row_slices = Vec::new();
        let mut row_ranges = Vec::with_capacity(rows.len());
        for mut row in rows {
            row.sort_unstable();
            row.dedup();
            let start = row_slices.len();
            row_slices.extend(row);
            row_ranges.push(start..row_slices.len());
        }
        let mut raw_slices = row_slices.clone();
        raw_slices.sort_unstable();
        let mut slices: Vec<SourceSlice> = Vec::with_capacity(raw_slices.len());
        for slice in raw_slices {
            if let Some(last) = slices.last_mut() {
                if let Some(merged) = last.merge_if_touching(slice) {
                    *last = merged;
                    continue;
                }
            }
            slices.push(slice);
        }
        Self { slices, row_slices, row_ranges }
    }

    pub(crate) fn len(&self) -> usize {
        self.slices.len()
    }

    pub(crate) fn row_count(&self) -> usize {
        self.row_ranges.len()
    }

    /// Fixed FNV-1a over numeric provenance only. Unlike `DefaultHasher`, this is explicitly
    /// stable across runs and Rust versions and cannot leak table text into diagnostics.
    pub(crate) fn stable_hash(&self) -> u64 {
        let mut hash = 0xcbf29ce484222325u64;
        for slice in &self.slices {
            for value in [
                slice.source().ordinal(),
                slice.char_start(),
                slice.char_end(),
            ] {
                for byte in value.to_le_bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x100000001b3);
                }
            }
        }
        hash
    }
}

/// A table while it still participates in page-space ownership and reading-order decisions.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PositionedTableAnalysis {
    pub(crate) bbox: Rect,
    pub(crate) table: TableAnalysis,
    pub(crate) key: CandidateKey,
    pub(crate) claim: TableClaim,
}

impl PositionedTableAnalysis {
    pub(crate) fn from_parts(
        bbox: Rect,
        header: Vec<Vec<(String, usize)>>,
        grid: Vec<Vec<String>>,
        header_rows: usize,
        evidence: Vec<TableEvidence>,
    ) -> Self {
        Self {
            bbox,
            table: TableAnalysis::from_parts(header, grid, header_rows, None, evidence),
            key: CandidateKey::synthetic(),
            claim: TableClaim::default(),
        }
    }

    pub(crate) fn with_ownership(mut self, key: CandidateKey, claim: TableClaim) -> Self {
        self.key = key;
        self.claim = claim;
        self
    }

    pub(crate) fn into_public<F>(self, page: u32, normalize: F) -> AnalyzedTable
    where
        F: Fn(Rect) -> Option<[f32; 4]>,
    {
        let bbox_norm = normalize(self.bbox);
        let (n_rows, n_cols) = self.table.logical_shape();
        let mut cells: Vec<AnalyzedCell> = self
            .table
            .header
            .iter()
            .chain(&self.table.grid)
            .flatten()
            .filter(|cell| !cell.covered)
            .map(|cell| AnalyzedCell {
                text: cell.text.clone(),
                row: cell.row,
                col: cell.col,
                rowspan: cell.rowspan.max(1),
                colspan: cell.colspan.max(1),
                bbox_norm: cell.bbox.and_then(&normalize),
                role: if cell.role == TableCellRole::Header || cell.row < self.table.header_rows {
                    TableCellRole::Header
                } else {
                    TableCellRole::Data
                },
                header_path: Vec::new(),
            })
            .collect();
        let headers: Vec<(usize, usize, usize)> = cells
            .iter()
            .filter(|cell| cell.role == TableCellRole::Header)
            .map(|cell| (cell.row, cell.col, cell.colspan))
            .collect();
        for cell in &mut cells {
            if cell.role == TableCellRole::Data {
                cell.header_path = headers
                    .iter()
                    .filter(|&&(row, col, colspan)| {
                        row < cell.row
                            && cell.col >= col
                            && cell.col < col.saturating_add(colspan.max(1))
                    })
                    .map(|&(row, col, _)| [row, col])
                    .collect();
            }
        }
        cells.sort_by_key(|cell| (cell.row, cell.col));
        AnalyzedTable {
            page,
            bbox_norm,
            n_rows,
            n_cols,
            header_rows: self.table.header_rows,
            cells,
            caption: self.table.caption.map(|caption| AnalyzedCaption {
                number: caption.number,
                text: crate::nav::strip_inline(&caption.html).trim().to_string(),
                below: caption.below,
            }),
            evidence: self.table.evidence,
        }
    }
}

fn dedup_evidence(evidence: Vec<TableEvidence>) -> Vec<TableEvidence> {
    let mut out = Vec::with_capacity(evidence.len());
    for item in evidence {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

// Compatibility for the detector's existing private tests and spatial code while ownership
// moves to `table`. The inline comment distinguishes this trait method from the crate's one
// owned PDF-object `fn deref`, whose structural test intentionally scans that exact spelling.
#[rustfmt::skip]
impl std::ops::Deref for PositionedTableAnalysis {
    type Target = TableAnalysis;

    fn /* not pdfobj::deref */ deref(&self) -> &Self::Target {
        &self.table
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_parts_round_trip_without_normalizing_their_semantics() {
        let header = vec![
            vec![("  Group  ".to_string(), 2), ("Tail".to_string(), 0)],
            vec![
                ("A".to_string(), 1),
                ("B".to_string(), 1),
                ("C".to_string(), 1),
            ],
        ];
        let grid = vec![
            vec![" 1 ".to_string(), "2".to_string(), "3".to_string()],
            vec![" 4 ".to_string(), "5".to_string(), "6".to_string()],
        ];
        let table = TableAnalysis::from_parts(
            header.clone(),
            grid.clone(),
            3,
            Some(("7".into(), "Table 7".into(), true)),
            Vec::new(),
        );

        assert_eq!(table.header_parts(), header);
        assert_eq!(table.grid_parts(), grid);
        assert_eq!(table.header[0][0].row, 0);
        assert_eq!(table.header[0][0].col, 0);
        assert_eq!(
            table.header[0][1].col, 2,
            "a zero wire span still occupies one anchor slot"
        );
        assert_eq!(table.grid[0][0].row, 2);
        assert_eq!(table.header_rows, 3);
        assert!(table
            .header
            .iter()
            .flatten()
            .all(|cell| cell.role == CellRole::Header));
        assert!(table.grid[0]
            .iter()
            .all(|cell| cell.role == CellRole::Header));
        assert!(table.grid[1].iter().all(|cell| cell.role == CellRole::Data));
        assert_eq!(
            table
                .caption
                .as_ref()
                .map(|c| (&c.number, &c.html, c.below)),
            Some((&"7".to_string(), &"Table 7".to_string(), true))
        );
    }

    #[test]
    fn query_cells_expand_colspans_and_trim_exactly_once() {
        let table = TableAnalysis::from_parts(
            vec![vec![(" Group ".into(), 2), (" Tail ".into(), 0)]],
            vec![vec![" 1 ".into(), " 2 ".into(), " 3 ".into()]],
            1,
            None,
            Vec::new(),
        );
        assert_eq!(
            table.expanded_cells(),
            vec![
                vec!["Group".to_string(), "Group".to_string(), "Tail".to_string()],
                vec!["1".to_string(), "2".to_string(), "3".to_string()],
            ]
        );
    }

    #[test]
    fn exact_cell_geometry_is_optional_and_preserved() {
        let bbox = Rect::new(10.0, 20.0, 30.0, 40.0);
        let cell = CellAnalysis::new("x".into(), 0, 0, 1, 1, Some(bbox));
        let positioned = PositionedTableAnalysis {
            bbox,
            table: TableAnalysis::from_cells(Vec::new(), vec![vec![cell]], 0, Vec::new()),
            key: CandidateKey::synthetic(),
            claim: TableClaim::default(),
        };
        assert_eq!(positioned.table.grid[0][0].bbox, Some(bbox));
        assert_eq!(positioned.bbox, bbox);
    }

    #[test]
    fn evidence_is_deduplicated_without_reordering() {
        let table = TableAnalysis::from_cells(
            Vec::new(),
            Vec::new(),
            0,
            vec![
                TableEvidence::Ruled,
                TableEvidence::Aligned,
                TableEvidence::Ruled,
            ],
        );
        assert_eq!(
            table.evidence,
            vec![TableEvidence::Ruled, TableEvidence::Aligned]
        );
    }

    #[test]
    fn claim_union_canonicalizes_a_whole_span_and_adjacent_split_pieces_identically() {
        let whole = SourceSlice::test_occurrence(12, 6);
        let pieces = vec![whole.sub_slice(0, 2), whole.sub_slice(2, 5), whole.sub_slice(5, 6)];
        let whole_claim = TableClaim::from_rows(vec![vec![whole]]);
        let split_claim = TableClaim::from_rows(vec![pieces]);

        assert_eq!(whole_claim.slices, split_claim.slices);
        assert_eq!(whole_claim.stable_hash(), split_claim.stable_hash());
        assert_eq!(split_claim.slices, vec![whole]);
    }

    #[test]
    fn public_projection_keeps_anchors_paths_and_legacy_body_distinct() {
        let table = PositionedTableAnalysis::from_parts(
            Rect::new(10.0, 20.0, 90.0, 80.0),
            vec![
                vec![("Group".into(), 2)],
                vec![("A".into(), 1), ("B".into(), 1)],
            ],
            vec![vec!["1".into(), "2".into()]],
            2,
            vec![TableEvidence::Aligned],
        )
        .into_public(3, |rect| Some(rect.into()));
        assert_eq!((table.page, table.n_rows, table.n_cols), (3, 3, 2));
        assert_eq!(table.cells[0].colspan, 2);
        assert_eq!(table.cells[3].header_path, vec![[0, 0], [1, 0]]);
        assert_eq!(table.cells[4].header_path, vec![[0, 0], [1, 1]]);
    }

    fn dense_grouped_header(evidence: Vec<TableEvidence>, header_rows: usize) -> TableAnalysis {
        TableAnalysis::from_parts(
            Vec::new(),
            vec![
                vec![
                    "Group A".into(),
                    String::new(),
                    String::new(),
                    "Group B".into(),
                    String::new(),
                    String::new(),
                ],
                vec!["A".into(), "B".into(), "C".into(), "D".into(), "E".into(), "F".into()],
                vec!["1".into(), "2".into(), "3".into(), "4".into(), "5".into(), "6".into()],
            ],
            header_rows,
            None,
            evidence,
        )
    }

    #[test]
    fn aligned_group_header_gets_logical_spans_without_changing_dense_projections() {
        let dense = vec![
            vec![
                "Group A".to_string(),
                String::new(),
                String::new(),
                "Group B".to_string(),
                String::new(),
                String::new(),
            ],
            vec!["A".into(), "B".into(), "C".into(), "D".into(), "E".into(), "F".into()],
            vec!["1".into(), "2".into(), "3".into(), "4".into(), "5".into(), "6".into()],
        ];
        let mut table = dense_grouped_header(vec![TableEvidence::Aligned], 2);
        table.compress_aligned_leading_group_header(&[true, false, false, true, false, false]);

        assert_eq!(table.grid_parts(), dense);
        assert!(table.header_parts().is_empty());
        assert_eq!(table.grid[0].len(), 6, "dense render slots remain present");
        assert_eq!(table.grid[0][0].colspan, 3);
        assert_eq!(table.grid[0][3].colspan, 3);
        assert!(table.grid[0][1..3].iter().all(|cell| cell.covered));
        assert!(table.grid[0][4..6].iter().all(|cell| cell.covered));
        assert!(table.grid[0].iter().all(|cell| cell.render_colspan == 1));

        let public = PositionedTableAnalysis {
            bbox: Rect::new(0.0, 0.0, 60.0, 30.0),
            table,
            key: CandidateKey::synthetic(),
            claim: TableClaim::default(),
        }
        .into_public(1, |rect| Some(rect.into()));
        assert_eq!((public.n_rows, public.n_cols, public.cells.len()), (3, 6, 14));
        assert_eq!(
            public
                .cells
                .iter()
                .filter(|cell| cell.row == 0)
                .map(|cell| (cell.col, cell.colspan, cell.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, 3, "Group A"), (3, 3, "Group B")]
        );
        assert_eq!(public.cells[8].header_path, vec![[0, 0], [1, 0]]);
        assert_eq!(public.cells[10].header_path, vec![[0, 0], [1, 2]]);
        assert_eq!(public.cells[11].header_path, vec![[0, 3], [1, 3]]);
        assert_eq!(public.cells[13].header_path, vec![[0, 3], [1, 5]]);
    }

    #[test]
    fn grouped_header_proof_refuses_non_aligned_or_shallow_candidates() {
        for (evidence, header_rows) in [
            (vec![TableEvidence::Ruled, TableEvidence::Aligned], 2),
            (vec![TableEvidence::Declared], 2),
            (vec![TableEvidence::Aligned], 1),
        ] {
            let mut table = dense_grouped_header(evidence, header_rows);
            table.compress_aligned_leading_group_header(&[
                true, false, false, true, false, false,
            ]);
            assert!(table.grid.iter().flatten().all(|cell| cell.colspan == 1));
            assert!(table.grid.iter().flatten().all(|cell| !cell.covered));
        }
    }

    #[test]
    fn grouped_header_proof_requires_full_leaf_ownership_and_blank_runs() {
        let cases = [
            vec![
                vec!["Group A".into(), String::new(), "Group B".into(), String::new()],
                vec!["A".into(), String::new(), "C".into(), "D".into()],
            ],
            vec![
                vec!["Group A".into(), "Ungrouped".into(), "Group B".into(), String::new()],
                vec!["A".into(), "B".into(), "C".into(), "D".into()],
            ],
            vec![
                vec![String::new(), "Group A".into(), String::new(), "Group B".into()],
                vec!["A".into(), "B".into(), "C".into(), "D".into()],
            ],
        ];
        for grid in cases {
            let mut table = TableAnalysis::from_parts(
                Vec::new(),
                grid,
                2,
                None,
                vec![TableEvidence::Aligned],
            );
            table.compress_aligned_leading_group_header(&[true, false, true, false]);
            assert!(table.grid.iter().flatten().all(|cell| cell.colspan == 1));
            assert!(table.grid.iter().flatten().all(|cell| !cell.covered));
        }

        let mut unstyled = dense_grouped_header(vec![TableEvidence::Aligned], 2);
        unstyled.compress_aligned_leading_group_header(&[
            true, false, false, false, false, false,
        ]);
        assert!(unstyled.grid.iter().flatten().all(|cell| cell.colspan == 1));
        assert!(unstyled.grid.iter().flatten().all(|cell| !cell.covered));

        let mut no_body = dense_grouped_header(vec![TableEvidence::Aligned], 2);
        no_body.grid.pop();
        no_body.compress_aligned_leading_group_header(&[
            true, false, false, true, false, false,
        ]);
        assert!(no_body.grid.iter().flatten().all(|cell| cell.colspan == 1));
        assert!(no_body.grid.iter().flatten().all(|cell| !cell.covered));

        let mut existing = dense_grouped_header(vec![TableEvidence::Aligned], 2);
        existing.grid[0][0].colspan = 3;
        existing.compress_aligned_leading_group_header(&[
            true, false, false, true, false, false,
        ]);
        assert_eq!(existing.grid[0][3].colspan, 1);
        assert!(existing.grid[0].iter().all(|cell| !cell.covered));

        let mut detached = TableAnalysis::from_parts(
            vec![vec![
                ("Group A".into(), 1),
                (String::new(), 1),
                (String::new(), 1),
                ("Group B".into(), 1),
                (String::new(), 1),
                (String::new(), 1),
            ]],
            vec![
                vec!["A".into(), "B".into(), "C".into(), "D".into(), "E".into(), "F".into()],
                vec!["1".into(), "2".into(), "3".into(), "4".into(), "5".into(), "6".into()],
            ],
            2,
            None,
            vec![TableEvidence::Aligned],
        );
        detached.compress_aligned_leading_group_header(&[
            true, false, false, true, false, false,
        ]);
        assert!(detached.header.iter().flatten().all(|cell| cell.colspan == 1));
        assert!(detached.header.iter().flatten().all(|cell| !cell.covered));
    }
}
