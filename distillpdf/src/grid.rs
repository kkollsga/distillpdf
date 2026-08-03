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

/// Index of the column band containing `x`, or the nearest band when `x` falls in a gap.
///
/// Inferred grids publish occupied x-ranges rather than boundary axes. Keeping their exact
/// containment-and-nearest rule here gives every inferred producer one allocation contract.
pub(crate) fn column_band_index(bands: &[(f32, f32)], x: f32) -> Option<usize> {
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
            d(lo, hi)
                .partial_cmp(&d(lo2, hi2))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
}

/// Assign each row's items to inferred column bands by horizontal centre.
///
/// Item order inside a cell is stable. Text joining remains the text layer's responsibility;
/// this geometric core returns only the allocation produced by the shared band contract.
pub(crate) fn bind_rows_by_center<'a, T>(
    bands: &[(f32, f32)],
    rows: &[&'a [T]],
    center: impl Fn(&T) -> f32,
) -> Vec<Vec<Vec<&'a T>>> {
    rows.iter()
        .map(|row| {
            let mut cells = vec![Vec::new(); bands.len()];
            for item in *row {
                if let Some(k) = column_band_index(bands, center(item)) {
                    cells[k].push(item);
                }
            }
            cells
        })
        .collect()
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

fn overlaps_half(a: Rect, b: Rect) -> bool {
    let lo = a.y0.max(b.y0);
    let hi = a.y1.min(b.y1);
    let span = a.height().min(b.height()).max(1.0);
    (hi - lo) >= span * 0.5
}

/// Rejoin two table streams that a page gutter split, preserving unmatched items in order.
///
/// `redetect` receives the combined vertical band. If it cannot produce an answer, both
/// original halves survive. The `enabled` bit is independent evidence that a full-width table
/// can exist; without it, side-by-side tables are never merged merely because their rows align.
pub(crate) fn rejoin_split_pairs<T: Clone>(
    left: &[T],
    right: Vec<T>,
    enabled: bool,
    region_of: impl Fn(&T) -> Rect,
    mut redetect: impl FnMut(f32, f32) -> Vec<T>,
) -> Vec<T> {
    let mut out = Vec::new();
    let mut used_right = vec![false; right.len()];
    for l in left {
        let lr = region_of(l);
        match right
            .iter()
            .enumerate()
            .find(|(j, r)| enabled && !used_right[*j] && overlaps_half(lr, region_of(r)))
        {
            Some((j, r)) => {
                used_right[j] = true;
                let rr = region_of(r);
                let merged = redetect(lr.y0.min(rr.y0), lr.y1.max(rr.y1));
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
    for (j, r) in right.into_iter().enumerate() {
        if !used_right[j] {
            out.push(r);
        }
    }
    out
}

/// Rejoin table fragments that a multi-column page split across two or more lanes.
///
/// Starting from each unused fragment, take at most one vertically-overlapping fragment from
/// every lane to its right. Multi-fragment chains are redetected across their combined vertical
/// band; when redetection has no answer, the original fragments survive in lane order.
pub(crate) fn rejoin_lane_chains<T: Clone>(
    per_lane: &[Vec<T>],
    region_of: impl Fn(&T) -> Rect,
    mut redetect: impl FnMut(f32, f32) -> Vec<T>,
) -> Vec<T> {
    let mut used: Vec<Vec<bool>> = per_lane.iter().map(|v| vec![false; v.len()]).collect();
    let mut out = Vec::new();
    for (k, lane) in per_lane.iter().enumerate() {
        for (i, item) in lane.iter().enumerate() {
            if used[k][i] {
                continue;
            }
            used[k][i] = true;
            let region = region_of(item);
            let (mut y0, mut y1) = (region.y0, region.y1);
            let mut chain = vec![item];
            for (k2, next_lane) in per_lane.iter().enumerate().skip(k + 1) {
                if let Some(j) = (0..next_lane.len()).find(|&j| {
                    if used[k2][j] {
                        return false;
                    }
                    let next = region_of(&next_lane[j]);
                    let lo = y0.max(next.y0);
                    let hi = y1.min(next.y1);
                    let span = (y1 - y0).min(next.height()).max(1.0);
                    (hi - lo) >= span * 0.5
                }) {
                    used[k2][j] = true;
                    let next = region_of(&next_lane[j]);
                    y0 = y0.min(next.y0);
                    y1 = y1.max(next.y1);
                    chain.push(&next_lane[j]);
                }
            }
            if chain.len() == 1 {
                out.push(item.clone());
                continue;
            }
            let merged = redetect(y0, y1);
            if merged.is_empty() {
                out.extend(chain.into_iter().cloned());
            } else {
                out.extend(merged);
            }
        }
    }
    out
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
    fn inferred_rows_bind_by_center_and_use_the_nearest_gap_band() {
        let bands = [(0.0, 4.0), (6.0, 10.0)];
        let row = [1.0f32, 5.5, 8.0];
        let bound = bind_rows_by_center(&bands, &[&row], |x| *x);
        assert_eq!(bound[0][0], vec![&1.0]);
        assert_eq!(bound[0][1], vec![&5.5, &8.0]);
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

    #[test]
    fn a_split_pair_rejoins_only_with_independent_full_width_evidence() {
        #[derive(Clone, Debug, PartialEq)]
        struct Item {
            id: &'static str,
            rect: Rect,
        }
        let item = |id, x0, x1| Item {
            id,
            rect: Rect::new(x0, 0.0, x1, 10.0),
        };
        let left = vec![item("left", 0.0, 10.0)];
        let right = vec![item("right", 10.0, 20.0)];
        let kept = rejoin_split_pairs(
            &left,
            right.clone(),
            false,
            |t| t.rect,
            |_, _| vec![item("merged", 0.0, 20.0)],
        );
        assert_eq!(
            kept.iter().map(|t| t.id).collect::<Vec<_>>(),
            ["left", "right"]
        );

        let joined = rejoin_split_pairs(
            &left,
            right,
            true,
            |t| t.rect,
            |_, _| vec![item("merged", 0.0, 20.0)],
        );
        assert_eq!(joined.iter().map(|t| t.id).collect::<Vec<_>>(), ["merged"]);
    }

    #[test]
    fn lane_fragments_rejoin_across_the_combined_vertical_band() {
        #[derive(Clone, Debug, PartialEq)]
        struct Item {
            id: &'static str,
            rect: Rect,
        }
        let item = |id, y0, y1| Item {
            id,
            rect: Rect::new(0.0, y0, 10.0, y1),
        };
        let lanes = vec![
            vec![item("left", 0.0, 10.0)],
            vec![item("middle", 1.0, 11.0)],
            vec![item("right", 2.0, 12.0)],
        ];
        let joined = rejoin_lane_chains(
            &lanes,
            |t| t.rect,
            |y0, y1| {
                assert_eq!((y0, y1), (0.0, 12.0));
                vec![item("merged", y0, y1)]
            },
        );
        assert_eq!(joined.iter().map(|t| t.id).collect::<Vec<_>>(), ["merged"]);
    }
}
