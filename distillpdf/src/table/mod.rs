//! Canonical in-memory table analysis shared by the born-digital detector, render IR and
//! durable-model projection.
//!
//! This is deliberately private and deliberately not a wire type.  The public legacy raw
//! extraction remains [`crate::TableInfo`], while the `.dpdf` model keeps its existing
//! `table_header` / `table_grid` fields.  Those surfaces are projections of this analysis,
//! not parallel interpretations of a table.

use crate::geom::{PageTurn, Rect};
use crate::text::{SourceSlice, Span};
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
    /// The proven Phase 6B ruled tier is physically part of the table even though it remains
    /// in `header` for exact colspan rendering/model fidelity.  Only that producer opts into
    /// prepending its anchor-plus-blank expansion to the legacy raw grid.
    legacy_prepend_header: bool,
    /// Exact provenance bit for the same proven tier. HTML carries this as an internal marker
    /// so only this table expands colspans when transformed to Markdown.
    proven_leading_tier: bool,
    /// Non-wire, proof-carrying continuation state. It is absent for inferred/declared/model
    /// tables and is never serialized or projected onto the public API.
    continuation: Option<ContinuationProof>,
    /// Final accepted source owner. Unlike `TableClaim`, this remains exact after a table
    /// crosses a page boundary because every slice carries its originating page.
    ownership: Option<ProvenOwnership>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PageSourceSlice {
    page: u32,
    slice: SourceSlice,
}

#[derive(Clone, Debug, PartialEq)]
struct ContinuationDraft {
    cell_boxes: Vec<Vec<Rect>>,
    leading_styled: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum ContinuationProof {
    Draft(ContinuationDraft),
    Proven(ProvenContinuation),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProvenContinuation {
    last_page: u32,
    turn: i32,
    page_box_bits: [u32; 4],
    column_bits: Vec<u32>,
    cell_x_bits: Vec<(u32, u32)>,
    leading_styled: bool,
    terminal_owner: bool,
    leading_owner: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProvenOwnership {
    anchor_page: u32,
    key: CandidateKey,
    row_claims: Vec<Vec<PageSourceSlice>>,
    union_claims: Vec<PageSourceSlice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnershipDiagnostic {
    pub(crate) anchor_page: u32,
    pub(crate) key: CandidateKey,
    pub(crate) claim_rows: usize,
    pub(crate) slices: usize,
    pub(crate) hash: u64,
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
            legacy_prepend_header: false,
            proven_leading_tier: false,
            continuation: None,
            ownership: None,
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
            legacy_prepend_header: false,
            proven_leading_tier: false,
            continuation: None,
            ownership: None,
        }
    }

    /// Attach only the exact L3 lattice facts. Page/turn, final producer and boundary
    /// ownership are intentionally unavailable until page reconciliation has finished.
    pub(crate) fn set_ruled_continuation_draft(
        &mut self,
        cell_boxes: Vec<Vec<Rect>>,
        leading_styled: bool,
    ) {
        self.continuation = Some(ContinuationProof::Draft(ContinuationDraft {
            cell_boxes,
            leading_styled,
        }));
    }

    fn proven_continuation(&self) -> Option<&ProvenContinuation> {
        match self.continuation.as_ref()? {
            ContinuationProof::Proven(proof) => Some(proof),
            ContinuationProof::Draft(_) => None,
        }
    }

    pub(crate) fn ownership_diagnostic(&self) -> Option<OwnershipDiagnostic> {
        let ownership = self.ownership.as_ref()?;
        let multi_page = ownership
            .union_claims
            .iter()
            .any(|claim| claim.page != ownership.anchor_page);
        let mut hash = 0xcbf29ce484222325u64;
        for claim in &ownership.union_claims {
            let mut values = Vec::with_capacity(4);
            if multi_page {
                values.push(claim.page);
            }
            values.extend([
                claim.slice.source().ordinal(),
                claim.slice.char_start(),
                claim.slice.char_end(),
            ]);
            for value in values {
                for byte in value.to_le_bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x100000001b3);
                }
            }
        }
        Some(OwnershipDiagnostic {
            anchor_page: ownership.anchor_page,
            key: ownership.key,
            claim_rows: ownership.row_claims.len(),
            slices: ownership.union_claims.len(),
            hash,
        })
    }

    fn dense_row_signature(row: &[CellAnalysis]) -> Option<Vec<(&str, usize, usize)>> {
        (!row.is_empty()
            && row.iter().enumerate().all(|(col, cell)| {
                cell.col == col
                    && cell.rowspan == 1
                    && cell.colspan == 1
                    && cell.render_colspan == 1
                    && !cell.covered
            }))
        .then(|| row.iter().map(|cell| (cell.text.as_str(), cell.rowspan, cell.colspan)).collect())
    }

    /// Merge one already-proven fragment. The proof decides whether the leading row is a
    /// repeated header; text equality by itself never suppresses data.
    pub(crate) fn merge_proven_fragment(&mut self, next: &TableAnalysis) -> bool {
        let (Some(anchor), Some(follow)) = (
            self.proven_continuation().cloned(),
            next.proven_continuation().cloned(),
        ) else {
            return false;
        };
        if anchor.last_page.checked_add(1) != Some(follow.last_page)
            || !anchor.terminal_owner
            || !follow.leading_owner
            || anchor.turn != follow.turn
            || anchor.page_box_bits != follow.page_box_bits
            || anchor.column_bits != follow.column_bits
            || anchor.cell_x_bits != follow.cell_x_bits
            || !anchor.leading_styled
            || self.header.len() != 0
            || next.header.len() != 0
            || self.grid.is_empty()
            || next.grid.is_empty()
        {
            return false;
        }
        let skip = if follow.leading_styled {
            let (Some(a), Some(b)) = (
                Self::dense_row_signature(&self.grid[0]),
                Self::dense_row_signature(&next.grid[0]),
            ) else {
                return false;
            };
            if a != b {
                return false;
            }
            1
        } else {
            0
        };
        let (Some(mut ownership), Some(follow_ownership)) =
            (self.ownership.clone(), next.ownership.as_ref())
        else {
            return false;
        };
        if next.grid.len() <= skip || follow_ownership.row_claims.len() != next.grid.len() {
            return false;
        }
        let row_offset = self.grid.len();
        for (ri, row) in next.grid.iter().skip(skip).enumerate() {
            let mut row = row.clone();
            for cell in &mut row {
                cell.row = row_offset + ri;
                cell.role = TableCellRole::Data;
            }
            self.grid.push(row);
        }
        if skip == 1 {
            ownership.row_claims[0].extend(follow_ownership.row_claims[0].iter().cloned());
        }
        ownership
            .row_claims
            .extend(follow_ownership.row_claims.iter().skip(skip).cloned());
        ownership
            .union_claims
            .extend(follow_ownership.union_claims.iter().cloned());
        let Some(ContinuationProof::Proven(proof)) = self.continuation.as_mut() else {
            unreachable!();
        };
        proof.last_page = follow.last_page;
        proof.terminal_owner = follow.terminal_owner;
        self.ownership = Some(ownership);
        true
    }

    /// Mark the exact ruled leading tier as part of the legacy physical grid projection.
    /// No shape heuristic reaches this bit; the Phase 6B proof is its only production caller.
    pub(crate) fn project_header_into_legacy_grid(&mut self) {
        self.legacy_prepend_header = true;
        self.proven_leading_tier = true;
    }

    /// Restore the durable provenance marker when reconstructing a rendered table model.
    /// Legacy raw projection is irrelevant on this path and intentionally remains disabled.
    pub(crate) fn restore_proven_leading_tier(&mut self) {
        self.proven_leading_tier = true;
    }

    pub(crate) fn has_proven_leading_tier(&self) -> bool {
        self.proven_leading_tier
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

    /// Consume the legacy raw extraction projection. Ordinary detached headers remain absent;
    /// a proven ruled leading tier expands to its anchor followed by blank covered slots.
    pub(crate) fn into_grid_parts(self) -> Vec<Vec<String>> {
        let mut rows = Vec::with_capacity(
            self.grid.len() + usize::from(self.legacy_prepend_header && !self.header.is_empty()),
        );
        if self.legacy_prepend_header {
            rows.extend(self.header.into_iter().map(|header| {
                let width = header.iter().map(|cell| cell.render_colspan.max(1)).sum();
                let mut row = Vec::with_capacity(width);
                for cell in header {
                    row.push(cell.text);
                    row.extend(std::iter::repeat_n(
                        String::new(),
                        cell.render_colspan.max(1) - 1,
                    ));
                }
                row
            }));
        }
        rows.extend(self.grid
            .into_iter()
            .map(|row| row.into_iter().map(|cell| cell.text).collect()));
        rows
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

    /// Exact source intervals attributed to one detector row, before global unioning.
    pub(crate) fn row(&self, index: usize) -> Option<&[SourceSlice]> {
        self.row_ranges
            .get(index)
            .map(|range| &self.row_slices[range.clone()])
    }

    /// Whether this candidate owns every character in one painted source interval.
    ///
    /// Claims store a normalized union, so a ruled cell's sub-slice can be corroborated by an
    /// aligned candidate that owns the original whole span.
    pub(crate) fn covers(&self, source: SourceSlice) -> bool {
        self.slices.iter().any(|&owned| {
            owned.source() == source.source()
                && owned.char_start() <= source.char_start()
                && owned.char_end() >= source.char_end()
        })
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

    pub(crate) fn merge_proven_fragment(&mut self, next: &PositionedTableAnalysis) -> bool {
        self.table.merge_proven_fragment(&next.table)
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

/// Complete the private ruled proof only after the page's accepted owner set is final. A
/// declaration or an inference survivor cannot borrow a discarded frame's geometry.
fn accepted_page_ownership(
    page: u32,
    table: &PositionedTableAnalysis,
) -> Option<ProvenOwnership> {
    let row_claims = (0..table.claim.row_count())
        .map(|row| {
            table.claim.row(row).map(|slices| {
                slices
                    .iter()
                    .copied()
                    .map(|slice| PageSourceSlice { page, slice })
                    .collect()
            })
        })
        .collect::<Option<Vec<Vec<PageSourceSlice>>>>()?;
    Some(ProvenOwnership {
        anchor_page: page,
        key: table.key,
        row_claims,
        union_claims: table
            .claim
            .slices
            .iter()
            .copied()
            .map(|slice| PageSourceSlice { page, slice })
            .collect(),
    })
}

pub(crate) fn finalize_continuation_proofs(
    page: u32,
    spans: &[Span],
    tables: &mut [PositionedTableAnalysis],
    turn: PageTurn,
    inputs_are_display_space: bool,
) {
    let diagnostics_enabled = std::env::var_os("DPDF_TABLE_OWNERS").is_some();
    let eligible = |table: &PositionedTableAnalysis| {
        matches!(
            table.table.continuation.as_ref(),
            Some(ContinuationProof::Draft(draft))
                if table.key.producer == CandidateProducer::Frame
                    && table.table.evidence.contains(&TableEvidence::Ruled)
                    && table.table.header.is_empty()
                    && draft.cell_boxes.len() == table.table.grid.len()
                    && table.claim.row_count() == table.table.grid.len()
                    && !table.claim.slices.is_empty()
        )
    };
    if !tables.iter().any(eligible) {
        for table in tables {
            table.table.continuation = None;
            table.table.ownership = diagnostics_enabled
                .then(|| accepted_page_ownership(page, table))
                .flatten();
        }
        return;
    }
    let claimed = |source: SourceSlice| tables.iter().any(|table| table.claim.covers(source));
    let map_rect = |rect: Rect| {
        if inputs_are_display_space {
            rect
        } else {
            let (x0, x1, y0, y1) = turn.rect(rect.x0, rect.x1, rect.y0, rect.y1);
            Rect::new(x0, y0, x1, y1)
        }
    };
    let unclaimed: Option<Vec<Rect>> = spans
        .iter()
        .filter(|span| !span.text.trim().is_empty() && !claimed(span.source))
        .map(|span| {
            let width = if span.width > 0.1 {
                span.width
            } else {
                span.text.trim().chars().count() as f32 * span.size * 0.5
            };
            let rect = map_rect(Rect::new(
                span.x,
                span.y - span.size * 0.25,
                span.x + width,
                span.y + span.size * 0.75,
            ));
            [rect.x0, rect.y0, rect.x1, rect.y1]
                .into_iter()
                .all(f32::is_finite)
                .then_some(rect)
        })
        .collect();
    let (turn_code, page_box_bits) = turn.proof_geometry();
    for table in tables {
        table.table.ownership = diagnostics_enabled
            .then(|| accepted_page_ownership(page, table))
            .flatten();
        let draft = match table.table.continuation.take() {
            Some(ContinuationProof::Draft(draft)) => draft,
            _ => continue,
        };
        if table.key.producer != CandidateProducer::Frame
            || !table.table.evidence.contains(&TableEvidence::Ruled)
            || table.table.header.len() != 0
            || draft.cell_boxes.len() != table.table.grid.len()
            || table.claim.row_count() != table.table.grid.len()
            || table.claim.slices.is_empty()
        {
            continue;
        }
        let bbox = map_rect(table.bbox);
        if ![bbox.x0, bbox.y0, bbox.x1, bbox.y1]
            .into_iter()
            .all(f32::is_finite)
        {
            continue;
        }
        let mut canonical: Option<Vec<(u32, u32)>> = None;
        let mut stable = true;
        for row in draft.cell_boxes {
            let edges: Option<Vec<(u32, u32)>> = row
                .into_iter()
                .map(|cell| {
                    let cell = map_rect(cell);
                    [cell.x0, cell.y0, cell.x1, cell.y1]
                        .into_iter()
                        .all(f32::is_finite)
                        .then_some((cell.x0.to_bits(), cell.x1.to_bits()))
                })
                .collect();
            let Some(edges) = edges else {
                stable = false;
                break;
            };
            if canonical.as_ref().is_some_and(|first| first != &edges) {
                stable = false;
                break;
            }
            canonical.get_or_insert(edges);
        }
        let Some(cell_x_bits) = canonical.filter(|edges| stable && edges.len() >= 2) else {
            continue;
        };
        let mut column_bits = Vec::with_capacity(cell_x_bits.len() + 1);
        column_bits.push(cell_x_bits[0].0);
        column_bits.extend(cell_x_bits.iter().map(|edge| edge.1));
        if cell_x_bits.windows(2).any(|pair| pair[0].1 != pair[1].0) {
            continue;
        }
        let Some(unclaimed) = unclaimed.as_ref() else {
            continue;
        };
        if table.table.ownership.is_none() {
            table.table.ownership = accepted_page_ownership(page, table);
        }
        if table.table.ownership.is_none() {
            continue;
        }
        table.table.continuation = Some(ContinuationProof::Proven(ProvenContinuation {
            last_page: page,
            turn: turn_code,
            page_box_bits,
            column_bits,
            cell_x_bits,
            leading_styled: draft.leading_styled,
            terminal_owner: !unclaimed.iter().any(|span| span.y1 < bbox.y0),
            leading_owner: !unclaimed.iter().any(|span| span.y0 > bbox.y1),
        }));
    }
}

/// Group at most one exact terminal→leading pair per consecutive page boundary. Removing the
/// later fragment only after the shared merge succeeds preserves emit-once ownership.
pub(crate) fn group_positioned_continuations(
    pages: &mut [(u32, Vec<PositionedTableAnalysis>)],
) {
    for current in 1..pages.len() {
        if pages[current - 1].0.checked_add(1) != Some(pages[current].0) {
            continue;
        }
        let mut matches = Vec::new();
        for (next_i, next) in pages[current].1.iter().enumerate() {
            for anchor_page in 0..current {
                for (anchor_i, anchor) in pages[anchor_page].1.iter().enumerate() {
                    let mut merged = anchor.clone();
                    if merged.merge_proven_fragment(next) {
                        matches.push((anchor_page, anchor_i, next_i, merged));
                    }
                }
            }
        }
        if let Some((anchor_page, anchor_i, next_i, merged)) =
            (matches.len() == 1).then(|| matches.pop().unwrap())
        {
            pages[anchor_page].1[anchor_i] = merged;
            pages[current].1.remove(next_i);
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
    fn detached_headers_stay_out_of_legacy_grid_unless_the_producer_marks_them() {
        let make = || {
            TableAnalysis::from_parts(
                vec![vec![("Group A".into(), 2), ("Group B".into(), 2)]],
                vec![vec!["A".into(), "B".into(), "C".into(), "D".into()]],
                1,
                None,
                vec![TableEvidence::Aligned],
            )
        };
        assert_eq!(
            make().into_grid_parts(),
            vec![vec!["A", "B", "C", "D"]],
            "ordinary detached headers retain the legacy projection"
        );

        let mut marked = make();
        marked.project_header_into_legacy_grid();
        assert_eq!(
            marked.into_grid_parts(),
            vec![
                vec!["Group A", "", "Group B", ""],
                vec!["A", "B", "C", "D"],
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
        assert!(whole_claim.covers(whole.sub_slice(2, 5)));
        assert!(split_claim.covers(whole));
        assert!(!whole_claim.covers(SourceSlice::test_occurrence(13, 1)));
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

    fn proven_table(
        page: u32,
        styled: bool,
        terminal: bool,
        leading: bool,
        rows: &[&[&str]],
    ) -> TableAnalysis {
        let mut table = TableAnalysis::from_parts(
            Vec::new(),
            rows.iter()
                .map(|row| row.iter().map(|text| (*text).to_string()).collect())
                .collect(),
            1,
            None,
            vec![TableEvidence::Ruled],
        );
        table.continuation = Some(ContinuationProof::Proven(ProvenContinuation {
            last_page: page,
            turn: 0,
            page_box_bits: [0.0f32.to_bits(), 0.0f32.to_bits(), 100.0f32.to_bits(), 100.0f32.to_bits()],
            column_bits: vec![0.0f32.to_bits(), 10.0f32.to_bits(), 20.0f32.to_bits()],
            cell_x_bits: vec![(0.0f32.to_bits(), 10.0f32.to_bits()), (10.0f32.to_bits(), 20.0f32.to_bits())],
            leading_styled: styled,
            terminal_owner: terminal,
            leading_owner: leading,
        }));
        let row_claims: Vec<Vec<PageSourceSlice>> = rows.iter().enumerate().map(|(row, _)| vec![PageSourceSlice {
                page,
                slice: SourceSlice::test_occurrence(row as u32, 1),
            }]).collect();
        table.ownership = Some(ProvenOwnership {
            anchor_page: page,
            key: CandidateKey::new(CandidateProducer::Frame, 0),
            union_claims: row_claims.iter().flatten().cloned().collect(),
            row_claims,
        });
        table
    }

    #[test]
    fn continuation_style_controls_exact_header_suppression_and_claim_union() {
        let mut anchor = proven_table(1, true, true, false, &[&["H1", "H2"], &["a", "b"]]);
        let repeated = proven_table(2, true, true, true, &[&["H1", "H2"], &["c", "d"]]);
        assert!(anchor.merge_proven_fragment(&repeated));
        assert_eq!(
            anchor.grid_parts(),
            vec![
                vec![String::from("H1"), String::from("H2")],
                vec![String::from("a"), String::from("b")],
                vec![String::from("c"), String::from("d")],
            ]
        );
        let ownership = anchor.ownership.as_ref().unwrap();
        assert_eq!(ownership.row_claims.len(), 3);
        assert_eq!(ownership.row_claims[0].iter().map(|claim| claim.page).collect::<Vec<_>>(), vec![1, 2]);

        let mut data_anchor = proven_table(1, true, true, false, &[&["H1", "H2"], &["a", "b"]]);
        let text_equal_data = proven_table(2, false, true, true, &[&["H1", "H2"], &["c", "d"]]);
        assert!(data_anchor.merge_proven_fragment(&text_equal_data));
        assert_eq!(data_anchor.grid.len(), 4, "unstyled text-equal row is data, never a repeated header");
        assert!(data_anchor.grid[2].iter().all(|cell| cell.role == TableCellRole::Data));
        let claims = &data_anchor.ownership.as_ref().unwrap().row_claims;
        assert_eq!(claims.iter().map(Vec::len).sum::<usize>(), 4, "no source is lost or duplicated");
    }

    #[test]
    fn continuation_refuses_independent_headers_barriers_aligned_and_unstable_geometry() {
        let anchor = proven_table(1, true, true, false, &[&["H1", "H2"], &["a", "b"]]);
        let attacks = [
            proven_table(2, true, true, true, &[&["X1", "X2"], &["c", "d"]]),
            proven_table(2, true, true, false, &[&["H1", "H2"], &["c", "d"]]),
            proven_table(3, true, true, true, &[&["H1", "H2"], &["c", "d"]]),
        ];
        for attack in attacks {
            let mut attempt = anchor.clone();
            assert!(!attempt.merge_proven_fragment(&attack));
        }
        let mut rotated = proven_table(2, true, true, true, &[&["H1", "H2"], &["c", "d"]]);
        if let Some(ContinuationProof::Proven(proof)) = rotated.continuation.as_mut() {
            proof.turn = 90;
        }
        assert!(!anchor.clone().merge_proven_fragment(&rotated));
        let aligned = TableAnalysis::from_parts(
            Vec::new(),
            vec![vec!["H1".into(), "H2".into()], vec!["c".into(), "d".into()]],
            1,
            None,
            vec![TableEvidence::Aligned],
        );
        assert!(!anchor.clone().merge_proven_fragment(&aligned), "aligned prose has no ruled proof");
    }

    #[test]
    fn exact_unclaimed_boundary_and_nonfinite_geometry_withhold_proof() {
        let make = |bad_geometry: bool| {
            let mut table = TableAnalysis::from_parts(
                Vec::new(),
                vec![vec!["H1".into(), "H2".into()], vec!["a".into(), "b".into()]],
                1,
                None,
                vec![TableEvidence::Ruled],
            );
            let x0 = if bad_geometry { f32::NAN } else { 10.0 };
            table.set_ruled_continuation_draft(
                vec![
                    vec![Rect::new(x0, 30.0, 20.0, 40.0), Rect::new(20.0, 30.0, 30.0, 40.0)],
                    vec![Rect::new(x0, 20.0, 20.0, 30.0), Rect::new(20.0, 20.0, 30.0, 30.0)],
                ],
                true,
            );
            PositionedTableAnalysis {
                bbox: Rect::new(10.0, 20.0, 30.0, 40.0),
                table,
                key: CandidateKey::new(CandidateProducer::Frame, 0),
                claim: TableClaim::from_rows(vec![
                    vec![SourceSlice::test_occurrence(0, 1)],
                    vec![SourceSlice::test_occurrence(1, 1)],
                ]),
            }
        };
        let barrier = Span {
            x: 10.0,
            y: 5.0,
            size: 8.0,
            width: 20.0,
            text: "caption barrier".into(),
            bold: false,
            italic: false,
            mono: false,
            angle: 0.0,
            font: 0,
            mcid: None,
            source: SourceSlice::test_occurrence(9, 15),
        };
        let turn = PageTurn::new(0, [0.0, 0.0, 100.0, 100.0]);
        let mut blocked = vec![make(false)];
        finalize_continuation_proofs(1, &[barrier], &mut blocked, turn, true);
        assert!(!blocked[0].table.proven_continuation().unwrap().terminal_owner);

        let mut nonfinite = vec![make(true)];
        finalize_continuation_proofs(1, &[], &mut nonfinite, turn, true);
        assert!(nonfinite[0].table.proven_continuation().is_none());
    }

    #[test]
    fn positioned_grouping_chains_across_more_than_two_pages() {
        let positioned = |table| PositionedTableAnalysis {
            bbox: Rect::new(0.0, 0.0, 20.0, 20.0),
            table,
            key: CandidateKey::new(CandidateProducer::Frame, 0),
            claim: TableClaim::default(),
        };
        let mut pages = vec![
            (1, vec![positioned(proven_table(1, true, true, false, &[&["H1", "H2"], &["a", "b"]]))]),
            (2, vec![positioned(proven_table(2, false, true, true, &[&["c", "d"]]))]),
            (3, vec![positioned(proven_table(3, false, true, true, &[&["e", "f"]]))]),
        ];
        group_positioned_continuations(&mut pages);
        assert_eq!(pages[0].1[0].table.grid.len(), 4);
        assert!(pages[1].1.is_empty() && pages[2].1.is_empty());
    }
}
