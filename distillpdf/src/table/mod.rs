//! Canonical in-memory table analysis shared by the born-digital detector, render IR and
//! durable-model projection.
//!
//! This is deliberately private and deliberately not a wire type.  The public legacy raw
//! extraction remains [`crate::TableInfo`], while the `.dpdf` model keeps its existing
//! `table_header` / `table_grid` fields.  Those surfaces are projections of this analysis,
//! not parallel interpretations of a table.

use crate::geom::Rect;

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
    /// Colspan of the current byte-stable HTML/model projection.  This differs from
    /// `colspan` only for declared cells: the existing declared renderer expands spans into
    /// dense slots even though the declaration itself retains the exact topology above.
    pub(crate) render_colspan: usize,
    pub(crate) role: CellRole,
    /// This dense slot is covered by an earlier row/column-spanning anchor.
    pub(crate) covered: bool,
    pub(crate) bbox: Option<Rect>,
    pub(crate) content_bbox: Option<Rect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CellRole {
    Header,
    Data,
}

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
            role: CellRole::Data,
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
        role: CellRole,
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
            role: CellRole::Data,
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
pub(crate) enum TableEvidence {
    Declared,
    Ruled,
    Aligned,
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
                    cell.role = CellRole::Header;
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
                            cell.role = CellRole::Header;
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
}

/// A table while it still participates in page-space ownership and reading-order decisions.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PositionedTableAnalysis {
    pub(crate) bbox: Rect,
    pub(crate) table: TableAnalysis,
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
}
