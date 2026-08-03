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
    extent: impl Fn(&Span) -> (f32, f32),
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
        let (x0, x1) = extent(s);
        if x1 < axes.xs[0] - tol || x0 > axes.xs[ncols] + tol {
            continue;
        }
        // y is ascending in page space; the first reading-order row is the last band.
        let r = nrows - 1 - band_index(axes.ys, cy);
        let pieces = split(s, axes.xs);
        seen += 1;
        cut += usize::from(pieces.len() > 1);
        for piece in pieces {
            let (px0, px1) = extent(&piece);
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

/// Reconcile a stronger producer's candidates against an existing table stream.
///
/// A preferred candidate replaces every existing answer it owns only when it covers those
/// answers sufficiently. If it owns an answer but overreaches it, the existing answer wins
/// and the preferred candidate is discarded. The caller supplies the evidence predicates;
/// this core owns the once-only, stable-order reconciliation.
pub(crate) fn reconcile_preferred<T>(
    existing: &mut Vec<T>,
    preferred: Vec<T>,
    region_of: impl Fn(&T) -> Rect,
    owns: impl Fn(&Rect, &T) -> bool,
    covers: impl Fn(&Rect, &T) -> bool,
) {
    if preferred.is_empty() {
        return;
    }
    let regions: Vec<Rect> = preferred.iter().map(region_of).collect();
    let overreaches: Vec<bool> = regions
        .iter()
        .map(|r| existing.iter().any(|t| owns(r, t) && !covers(r, t)))
        .collect();
    existing.retain(|t| {
        !regions
            .iter()
            .zip(&overreaches)
            .any(|(r, &over)| !over && owns(r, t))
    });
    existing.extend(
        preferred
            .into_iter()
            .zip(overreaches)
            .filter(|(_, over)| !over)
            .map(|(t, _)| t),
    );
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

    #[test]
    fn preferred_candidates_replace_owned_answers_but_not_when_they_overreach() {
        #[derive(Debug, PartialEq)]
        struct Item {
            id: &'static str,
            rect: Rect,
        }
        let item = |id, y0, y1| Item {
            id,
            rect: Rect::new(0.0, y0, 10.0, y1),
        };
        let owns = |r: &Rect, t: &Item| r.overlap_area(t.rect) > 0.0;

        let mut existing = vec![item("old", 0.0, 10.0), item("untouched", 20.0, 30.0)];
        reconcile_preferred(
            &mut existing,
            vec![item("preferred", 0.0, 10.0)],
            |t| t.rect,
            owns,
            |r, t| r.overlap_h(t.rect) >= t.rect.height(),
        );
        assert_eq!(
            existing.iter().map(|t| t.id).collect::<Vec<_>>(),
            ["untouched", "preferred"]
        );

        let mut existing = vec![item("longer", 0.0, 20.0)];
        reconcile_preferred(
            &mut existing,
            vec![item("short", 0.0, 5.0)],
            |t| t.rect,
            owns,
            |r, t| r.overlap_h(t.rect) >= t.rect.height() * 0.5,
        );
        assert_eq!(
            existing.iter().map(|t| t.id).collect::<Vec<_>>(),
            ["longer"]
        );
    }
}
