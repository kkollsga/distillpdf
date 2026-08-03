//! **L1, second evidence source** — a page's ruling read as closed-cell table frames.
//!
//! Text clustering answers "do these words line up?". That question is blind by construction
//! to two things a ruled table does routinely: it cannot see a **blank cell** (there are no
//! words to align) and it reads an in-grid **band title** as a one-cell row, which terminates
//! the run and splits one table into several. The ruling answers a different question — "where
//! did the producer say the cell boundaries are?" — and it answers it whether or not anybody
//! typed anything into the cells.
//!
//! This module is **type-agnostic**, per the architecture directive: it knows about closed
//! cells and nothing about full-grid / booktabs / borderless. A frame it emits is a *candidate
//! region with a column and row model*; deciding what to do with one is [`crate::extract`]'s
//! L2/L3 business.
//!
//! Everything here is in page space (y up), the space [`crate::vector::PageRules`] arrives in.
//!
//! **What consumes what.** [`h_bands`] feeds the alignment detector, which reads merged row
//! bands to decide whether a two-row run is a booktabs table. [`frames`] — the closed-cell
//! derivation below — feeds `extract`'s L2/L3 dispatch, whose `l3_ruled` handler binds text
//! into those cells by GEOMETRIC CONTAINMENT: a run is filtered to its row band and then cut at
//! every column boundary that falls inside it, so a run straddling a rule is split between the
//! two cells rather than placed wholly on the side its midpoint fell. That binder lives in
//! `extract` and is shared; this module stays pure geometry and knows nothing about text.
//!
//! Every tolerance below was chosen against THIS corpus's ruled documents and is documented
//! with what it was measured on. Nothing here is ported: no external table implementation was
//! read, and no constant is adopted from one.

use crate::geom::Rect;
use std::collections::BTreeMap;
use crate::vector::PageRules;

/// Rules whose position differs by less than this are the same grid line — producers draw a
/// cell's right edge and its neighbour's left edge as two paths a fraction of a point apart.
///
/// Swept on our corpus's ruled documents: the disagreements that must collapse are ≤0.6 pt
/// (World Bank verticals at 181.3/181.8, IRS at 482.1/482.4), while the closest DISTINCT grid
/// lines we carry are the 1040's line-number box at 482.4 and its amount box at 504.0 — 21 pt
/// apart. 1.6 sits in the middle of a wide empty band, so the value is not delicate.
const SNAP: f32 = 1.6;
/// A gap this small inside one grid line is a rounding seam between two segments of it, not a
/// break — a vertical drawn per row band arrives as N touching pieces.
///
/// Same sweep: the seams measured are ≤0.5 pt (a row rule ending at 504.2 where the next
/// begins at 503.8) and the narrowest real gap inside a rule — the World Bank tables' unruled
/// column gutters — is over 60 pt.
const JOIN: f32 = 2.5;
/// How much shorter than the cell edge a rule may fall and still be said to bound it.
///
/// Measured on our own ruled documents: a World Bank status table draws a row rule from
/// x=181.8 while the vertical it must reach starts at x=181.3, and a column rule spanning
/// y=177.1..227.5 has to bound a band whose top rule sits at y=176.6. Half-point disagreements
/// like these are the norm, and requiring exact containment finds almost no closed cell at all.
const COVER: f32 = 2.5;
/// Grid lines considered per axis. Beyond this the page is a map or a chart, not a table, and
/// the closed-cell scan would be quadratic in noise. The longest lines are kept, so a real
/// table's frame survives a page that also carries ruling clutter.
const MAX_LINES: usize = 96;
/// A frame narrower/shorter than this is a rule pair or a check-box, not a table.
const MIN_FRAME_W: f32 = 36.0;
const MIN_FRAME_H: f32 = 12.0;

/// One closed ruling frame: the grid lines that bound its cells.
///
/// `xs` and `ys` are both ascending, so with y up `ys[0]` is the frame's BOTTOM edge and the
/// first *reading-order* row is the band `ys[len-2]..ys[len-1]`. `xs.len() - 1` columns by
/// `ys.len() - 1` rows, both ≥ 2 by construction.
pub(crate) struct Frame {
    pub xs: Vec<f32>,
    pub ys: Vec<f32>,
    pub bbox: Rect,
}

impl Frame {
    /// Borrow this producer's result through the shared geometric grid contract.
    pub(crate) fn axes(&self) -> crate::grid::GridAxes<'_> {
        crate::grid::GridAxes::new(&self.xs, &self.ys, self.bbox)
            .expect("lattice frames publish finite, strictly ascending axes")
    }
}

/// One grid line: its position on the perpendicular axis, and the disjoint intervals it
/// actually covers along its own axis.
struct Line {
    p: f32,
    iv: Vec<(f32, f32)>,
}

impl Line {
    /// Does this line bound the edge `lo..hi`? Tolerant at both ends by [`COVER`].
        fn covers(&self, lo: f32, hi: f32) -> bool {
        self.iv.iter().any(|&(a, b)| a <= lo + COVER && b >= hi - COVER)
    }
    fn extent(&self) -> f32 {
        self.iv.iter().map(|&(a, b)| b - a).sum()
    }
}

/// Collapse raw rules into grid lines: snap near-equal positions together, then merge each
/// line's intervals (closing [`JOIN`]-sized seams). Deterministic — a total order on floats
/// via `total_cmp`, no hashing.
fn grid_lines(mut raw: Vec<(f32, f32, f32)>) -> Vec<Line> {
    // `raw` is `(lo, hi, p)`.
    raw.retain(|&(lo, hi, p)| hi > lo && lo.is_finite() && hi.is_finite() && p.is_finite());
    raw.sort_by(|a, b| a.2.total_cmp(&b.2).then(a.0.total_cmp(&b.0)));
    let mut out: Vec<Line> = Vec::new();
    for (lo, hi, p) in raw {
        match out.last_mut() {
            Some(l) if (p - l.p).abs() <= SNAP => l.iv.push((lo, hi)),
            _ => out.push(Line { p, iv: vec![(lo, hi)] }),
        }
    }
    for l in &mut out {
        l.iv.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for (lo, hi) in l.iv.drain(..) {
            match merged.last_mut() {
                Some(m) if lo <= m.1 + JOIN => m.1 = m.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        l.iv = merged;
    }
    if out.len() > MAX_LINES {
        // Keep the LONGEST lines: a real frame's edges run the width (or height) of the table
        // while cartographic clutter is short. Order is restored right after, so the choice of
        // which lines survive never changes the order the survivors are scanned in.
        out.sort_by(|a, b| b.extent().total_cmp(&a.extent()));
        out.truncate(MAX_LINES);
        out.sort_by(|a, b| a.p.total_cmp(&b.p));
    }
    out
}

/// The closed-cell frames a page's ruling defines.
///
/// A cell is CLOSED when all four of its edges are actually painted. Its left and right edges
/// are adjacent vertical lines; its top and bottom are **the next horizontal lines that reach
/// across it** — which is not the same thing as the next horizontal lines on the page, and the
/// difference is load-bearing. Producers rule tables partially all the time: a World Bank table
/// draws a sub-rule under its first three columns only, and reading that rule as a row boundary
/// for the other five leaves every cell in them unclosed and the table in shards.
///
/// Closed cells belong to the same frame when they touch vertically in one column or overlap
/// vertically in adjacent columns; a frame is emitted for each connected group at least 2
/// columns by 2 rows, spanning the rectangular hull of its cells and carrying every row line
/// any of them used. The hull is deliberate: a merged cell leaves a hole in the component, and
/// the table it belongs to still has that column and that row.
pub(crate) fn frames(rules: &PageRules) -> Vec<Frame> {
    let vs = grid_lines(rules.v.iter().map(|&(x, y0, y1)| (y0, y1, x)).collect());
    let hs = grid_lines(rules.h.iter().map(|&(x0, x1, y)| (x0, x1, y)).collect());
    let (nx, ny) = (vs.len(), hs.len());
    if nx < 2 || ny < 2 {
        return Vec::new();
    }
    let cw = nx - 1;
    // Per column band, the closed cells as (top line, bottom line) index pairs, top-down.
    let mut cells: Vec<Vec<(usize, usize)>> = vec![Vec::new(); cw];
    for i in 0..cw {
        let (x0, x1) = (vs[i].p, vs[i + 1].p);
        if x1 - x0 < 1.0 {
            continue;
        }
        // The horizontal lines that actually bound THIS column band.
        let bounds: Vec<usize> = (0..ny).filter(|&j| hs[j].covers(x0, x1)).collect();
        for w in bounds.windows(2) {
            let (j, j2) = (w[0], w[1]);
            let (y0, y1) = (hs[j].p, hs[j2].p);
            if y1 - y0 >= 1.0 && vs[i].covers(y0, y1) && vs[i + 1].covers(y0, y1) {
                cells[i].push((j, j2));
            }
        }
    }
    // Connected components: same column and touching, or adjacent columns and vertically
    // overlapping. Scanned in a fixed order, so the output never depends on iteration luck.
    let flat: Vec<(usize, usize, usize)> =
        cells.iter().enumerate().flat_map(|(i, cs)| cs.iter().map(move |&(j, j2)| (i, j, j2))).collect();
    let mut comp: Vec<usize> = (0..flat.len()).collect();
    fn find(comp: &mut [usize], mut k: usize) -> usize {
        while comp[k] != k {
            comp[k] = comp[comp[k]];
            k = comp[k];
        }
        k
    }
    // Only same-column and next-column pairs can join, so the scan walks the column bands
    // rather than every pair of cells on the page — the difference between linear and
    // quadratic on a page whose ruling is dense.
    let mut start: Vec<usize> = Vec::with_capacity(cw + 1);
    let mut n = 0usize;
    for cs in &cells {
        start.push(n);
        n += cs.len();
    }
    start.push(n);
    let union = |comp: &mut [usize], a: usize, b: usize| {
        let (ra, rb) = (find(comp, a), find(comp, b));
        if ra != rb {
            comp[rb] = ra;
        }
    };
    for i in 0..cw {
        for a in start[i]..start[i + 1] {
            let (_, ja, ja2) = flat[a];
            for (b, &(_, jb, jb2)) in flat.iter().enumerate().take(start[i + 1]).skip(a + 1) {
                if ja2 == jb || jb2 == ja {
                    union(&mut comp, a, b);
                }
            }
            if i + 1 < cw {
                for (b, &(_, jb, jb2)) in flat.iter().enumerate().take(start[i + 2]).skip(start[i + 1]) {
                    if ja < jb2 && jb < ja2 {
                        union(&mut comp, a, b); // overlapping row extents in neighbouring columns
                    }
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for k in 0..flat.len() {
        let r = find(&mut comp, k);
        groups.entry(r).or_default().push(k);
    }
    let mut out: Vec<Frame> = Vec::new();
    for (_, members) in groups {
        if members.len() < 4 {
            continue; // fewer than four cells cannot be a 2×2 lattice
        }
        let (mut imin, mut imax) = (usize::MAX, 0usize);
        let mut rows: Vec<usize> = Vec::new();
        for &k in &members {
            let (i, j, j2) = flat[k];
            imin = imin.min(i);
            imax = imax.max(i);
            rows.push(j);
            rows.push(j2);
        }
        rows.sort_unstable();
        rows.dedup();
        if rows.len() < 3 {
            continue; // a single row of cells is a strip, not a lattice
        }
        if imin == imax {
            continue; // a single file of boxes with no neighbouring column is not a table
        }
        let xs: Vec<f32> = (imin..=imax + 1).map(|i| vs[i].p).collect();
        let ys: Vec<f32> = rows.iter().map(|&j| hs[j].p).collect();
        let bbox = Rect::new(xs[0], ys[0], xs[xs.len() - 1], ys[ys.len() - 1]);
        if bbox.width() < MIN_FRAME_W || bbox.height() < MIN_FRAME_H {
            continue;
        }
        out.push(Frame { xs, ys, bbox });
    }
    join_shards(&mut out);
    // Reading order (top-down), so a page's frames arrive in the order a reader meets them.
    out.sort_by(|a, b| b.bbox.y1.total_cmp(&a.bbox.y1).then(a.bbox.x0.total_cmp(&b.bbox.x0)));
    out
}

/// Positions merged and deduplicated at [`SNAP`], ascending.
fn union_axis(a: &[f32], b: &[f32]) -> Vec<f32> {
    let mut all: Vec<f32> = a.iter().chain(b).copied().collect();
    all.sort_by(|p, q| p.total_cmp(q));
    let mut out: Vec<f32> = Vec::new();
    for p in all {
        if out.last().is_none_or(|&q| p - q > SNAP) {
            out.push(p);
        }
    }
    out
}

/// One table, several components — rejoined.
///
/// A ruled table fragments into closed-cell components wherever a cell is not closed on all
/// four sides, which happens constantly and for entirely ordinary reasons: a full-width band
/// title with no interior verticals, a header rule drawn only under the columns it labels, a
/// merged summary row. The reader still sees one table. Two shards belong to the same one when
/// they either TOUCH (separated by nothing but the rule they share) or are stacked on the same
/// column model within a row's reach — and the joined frame's grid lines are the union of
/// theirs, so the unruled band between them becomes exactly the row it looks like.
///
/// This is the ruled twin of the alignment path's stranded-header machinery. On World Bank
/// tables it is the difference between one 13×8 table and the five 2×3 shards its band rows
/// leave behind.
fn join_shards(frames: &mut Vec<Frame>) {
    /// Shards of one lattice are separated by the rule they share; a hair of slack absorbs
    /// the rule's own thickness.
    const TOUCH: f32 = 3.0;
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for a in 0..frames.len() {
            for b in (a + 1)..frames.len() {
                let (fa, fb) = (&frames[a], &frames[b]);
                // Signed gaps: negative where the boxes overlap on that axis.
                let gap_x = (fa.bbox.x0 - fb.bbox.x1).max(fb.bbox.x0 - fa.bbox.x1);
                let gap_y = (fa.bbox.y0 - fb.bbox.y1).max(fb.bbox.y0 - fa.bbox.y1);
                let touches = gap_x <= TOUCH && gap_y <= TOUCH;
                let row_h = |f: &Frame| f.bbox.height() / (f.ys.len() - 1).max(1) as f32;
                let stacked = fa.bbox.overlap_w(fb.bbox) >= fa.bbox.width().min(fb.bbox.width()).max(1.0) * 0.6
                    && gap_y <= row_h(fa).max(row_h(fb)) * 2.0;
                if !(touches || stacked) {
                    continue;
                }
                let joined = Frame {
                    xs: union_axis(&fa.xs, &fb.xs),
                    ys: union_axis(&fa.ys, &fb.ys),
                    bbox: fa.bbox.union(fb.bbox),
                };
                frames[a] = joined;
                frames.remove(b);
                merged = true;
                break 'outer;
            }
        }
    }
}

/// The page's horizontal ruling as **merged bands** — `(x0, x1, y)` per contiguous run of rule,
/// with collinear pieces joined and near-equal y's snapped together.
///
/// A booktabs table publishes no cell boundaries, only row BANDS: a rule above the header and a
/// rule under the last row. That evidence is worth nothing to the lattice (nothing closes) and
/// everything to the alignment path, which otherwise refuses a two-row run — see
/// [`crate::extract::rule_banded`].
pub(crate) fn h_bands(rules: &PageRules) -> Vec<(f32, f32, f32)> {
    grid_lines(rules.h.iter().map(|&(x0, x1, y)| (x0, x1, y)).collect())
        .into_iter()
        .flat_map(|l| l.iv.into_iter().map(move |(a, b)| (a, b, l.p)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_bands_join_a_rule_drawn_in_pieces() {
        // A World Bank status table draws its bottom rule as three abutting segments. Read as
        // three rules, none of them spans the table; read as one band, it is the boundary.
        let mut r = PageRules::default();
        for (a, b) in [(26.8, 257.4), (257.4, 340.6), (340.6, 589.2)] {
            r.h.push((a, b, 126.4));
        }
        let bands = h_bands(&r);
        assert_eq!(bands.len(), 1, "one band, got {bands:?}");
        assert!(bands[0].0 < 27.0 && bands[0].1 > 589.0, "{bands:?}");
    }

    /// A ruled `cols`×`rows` grid drawn the way a form does it: every line full-length.
    fn grid(x0: f32, y0: f32, cw: f32, rh: f32, cols: usize, rows: usize) -> PageRules {
        let mut r = PageRules::default();
        let x1 = x0 + cw * cols as f32;
        let y1 = y0 + rh * rows as f32;
        for i in 0..=cols {
            let x = x0 + cw * i as f32;
            r.v.push((x, y0, y1));
        }
        for j in 0..=rows {
            let y = y0 + rh * j as f32;
            r.h.push((x0, x1, y));
        }
        r
    }

    #[test]
    fn a_ruled_grid_becomes_one_frame_with_its_own_row_and_column_model() {
        let f = frames(&grid(50.0, 100.0, 60.0, 20.0, 4, 5));
        assert_eq!(f.len(), 1, "one frame, got {}", f.len());
        assert_eq!(f[0].xs.len(), 5, "5 column boundaries: {:?}", f[0].xs);
        assert_eq!(f[0].ys.len(), 6, "6 row boundaries: {:?}", f[0].ys);
    }

    #[test]
    fn a_lone_box_is_not_a_lattice() {
        // One stroked rectangle is a callout panel, a check box, a figure border — never a
        // table. It closes exactly one cell, and one cell is not a 2×2 lattice.
        assert!(frames(&grid(50.0, 100.0, 200.0, 80.0, 1, 1)).is_empty());
        // Nor is a single row of cells, or a single column of them.
        assert!(frames(&grid(50.0, 100.0, 60.0, 40.0, 4, 1)).is_empty());
        assert!(frames(&grid(50.0, 100.0, 200.0, 20.0, 1, 4)).is_empty());
    }

    #[test]
    fn two_separated_grids_stay_two_frames() {
        let mut r = grid(50.0, 500.0, 60.0, 20.0, 3, 3);
        let b = grid(50.0, 100.0, 60.0, 20.0, 3, 3);
        r.h.extend(b.h);
        r.v.extend(b.v);
        let f = frames(&r);
        assert_eq!(f.len(), 2, "two frames, got {}", f.len());
        assert!(f[0].bbox.y1 > f[1].bbox.y1, "frames come back in reading order");
    }

    #[test]
    fn a_merged_cell_leaves_the_frame_rectangular() {
        // Drop one interior vertical segment (the producer merged two cells in row 1). The
        // component is no longer a rectangle, but the TABLE still has that column, so the
        // hull — not the component — is what the frame reports.
        let mut r = grid(50.0, 100.0, 60.0, 20.0, 3, 3);
        r.v.retain(|&(x, y0, _)| !((x - 110.0).abs() < 0.1 && (y0 - 120.0).abs() < 0.1));
        // Re-add the vertical everywhere except the merged band.
        r.v.push((110.0, 100.0, 120.0));
        r.v.push((110.0, 140.0, 160.0));
        let f = frames(&r);
        assert_eq!(f.len(), 1, "one frame, got {}", f.len());
        assert_eq!(f[0].xs.len(), 4, "the merged column is still a column: {:?}", f[0].xs);
    }

    #[test]
    fn a_line_drawn_in_touching_pieces_is_one_line() {
        // A vertical drawn per row band arrives as N touching segments; a frame must see one
        // boundary, not N unrelated ones.
        let mut r = PageRules::default();
        for i in 0..=2 {
            let x = 50.0 + 60.0 * i as f32;
            for k in 0..4 {
                r.v.push((x, 100.0 + 20.0 * k as f32, 120.0 + 20.0 * k as f32));
            }
        }
        for j in 0..=4 {
            r.h.push((50.0, 170.0, 100.0 + 20.0 * j as f32));
        }
        let f = frames(&r);
        assert_eq!(f.len(), 1, "one frame, got {}", f.len());
        assert_eq!((f[0].xs.len(), f[0].ys.len()), (3, 5), "{:?} {:?}", f[0].xs, f[0].ys);
    }

    #[test]
    fn ruling_clutter_cannot_make_the_scan_quadratic() {
        // A cartographic page paints thousands of short segments. The line budget keeps the
        // scan bounded, and the longest-first keep means a real frame on such a page survives.
        let mut r = grid(50.0, 100.0, 60.0, 20.0, 4, 5);
        for k in 0..5000 {
            let t = k as f32 * 0.37;
            r.h.push((300.0 + t, 312.0 + t, 400.0 + t));
            r.v.push((300.0 + t, 400.0 + t, 412.0 + t));
        }
        let f = frames(&r);
        assert!(f.iter().any(|g| g.xs.len() == 5 && g.ys.len() == 6), "the real frame survived the clutter");
    }
}
