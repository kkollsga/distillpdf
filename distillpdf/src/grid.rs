//! Shared geometric grid contract and containment binder.
//!
//! Producers answer only where the row and column boundaries are.  This module owns the
//! common downstream operation: assign upright text to those cells by containment, splitting
//! a text run at interior column boundaries before assigning it.  The axes are always in PDF
//! page space (ascending x and y); output cells are row-major in reading order (top-down).
//!
//! Declared L0 tables deliberately do not use this contract: their structure tree already is
//! an exact grid, and converting it back into inferred geometry would discard information.

use crate::geom::Rect;
use crate::text::Span;

/// Borrowed axes published by a geometric grid producer.
pub(crate) struct GridAxes<'a> {
    pub(crate) xs: &'a [f32],
    pub(crate) ys: &'a [f32],
    pub(crate) bbox: Rect,
}

impl<'a> GridAxes<'a> {
    pub(crate) fn new(xs: &'a [f32], ys: &'a [f32], bbox: Rect) -> Option<Self> {
        let ordered = |v: &[f32]| {
            v.len() >= 2 && v.iter().all(|x| x.is_finite()) && v.windows(2).all(|w| w[0] < w[1])
        };
        (ordered(xs) && ordered(ys)).then_some(Self { xs, ys, bbox })
    }

    pub(crate) fn ncols(&self) -> usize {
        self.xs.len() - 1
    }

    pub(crate) fn nrows(&self) -> usize {
        self.ys.len() - 1
    }
}

/// Text pieces assigned to a geometric grid, plus the admission evidence the ruled path uses.
pub(crate) struct BoundCells {
    pub(crate) ncols: usize,
    pub(crate) nrows: usize,
    pub(crate) cells: Vec<Vec<Span>>,
    pub(crate) seen: usize,
    pub(crate) cut: usize,
}

/// Index of the band `bounds[k]..bounds[k+1]` containing `v`, clamped to the ends.
fn band_index(bounds: &[f32], v: f32) -> usize {
    match bounds.iter().position(|&b| v < b) {
        Some(0) => 0,
        Some(k) => k - 1,
        None => bounds.len().saturating_sub(2),
    }
}

/// Bind upright spans to a producer's cells by containment.
///
/// `split` cuts one span at all interior x boundaries.  It is supplied by the text layer so
/// this geometric core does not own font-advance semantics.  The 1pt boundary tolerance and
/// quarter-em baseline lift are preserved from the measured ruled path.
pub(crate) fn bind_contained(
    axes: &GridAxes<'_>,
    spans: &[Span],
    split: impl Fn(&Span, &[f32]) -> Vec<Span>,
) -> BoundCells {
    let (ncols, nrows) = (axes.ncols(), axes.nrows());
    let mut cells: Vec<Vec<Span>> = vec![Vec::new(); ncols * nrows];
    let (mut seen, mut cut) = (0usize, 0usize);
    let tol = 1.0f32;
    for s in spans {
        if s.angle.abs() >= 0.01 || s.text.trim().is_empty() {
            continue;
        }
        // Text sits on its baseline, a little above the rule below it.  Probe the middle of
        // the glyph body so a baseline grazing a boundary remains in its own row.
        let cy = s.y + s.size * 0.25;
        if cy < axes.ys[0] - tol || cy > axes.ys[nrows] + tol {
            continue;
        }
        let x0 = s.x;
        let x1 = s.x + s.width.max(0.0);
        if x1 < axes.xs[0] - tol || x0 > axes.xs[ncols] + tol {
            continue;
        }
        // y is ascending in page space; the first reading-order row is the last band.
        let r = nrows - 1 - band_index(axes.ys, cy);
        let pieces = split(s, axes.xs);
        seen += 1;
        cut += usize::from(pieces.len() > 1);
        for piece in pieces {
            let px0 = piece.x;
            let px1 = piece.x + piece.width.max(0.0);
            let c = band_index(axes.xs, (px0 + px1) * 0.5);
            cells[r * ncols + c].push(piece);
        }
    }
    BoundCells {
        ncols,
        nrows,
        cells,
        seen,
        cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axes_must_be_finite_and_strictly_ascending() {
        let bbox = Rect::new(0.0, 0.0, 20.0, 20.0);
        assert!(GridAxes::new(&[0.0, 10.0, 20.0], &[0.0, 10.0, 20.0], bbox).is_some());
        assert!(GridAxes::new(&[0.0, 0.0], &[0.0, 10.0], bbox).is_none());
        assert!(GridAxes::new(&[0.0, f32::NAN], &[0.0, 10.0], bbox).is_none());
    }

    #[test]
    fn page_space_y_bands_map_to_top_down_rows() {
        let ys = [0.0, 10.0, 20.0];
        assert_eq!(2 - 1 - band_index(&ys, 15.0), 0);
        assert_eq!(2 - 1 - band_index(&ys, 5.0), 1);
    }
}
