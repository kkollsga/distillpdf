//! Shared 2-D geometry: the PDF transformation matrix and the axis-aligned rectangle.
//!
//! Both types existed as copies before this module: a character-identical `mul` in three
//! walkers (`img`, `vector`, `text`), and rectangle union / intersection / area open-coded
//! as three-liners across `html`, `img`, `vector` and `layout`. Nothing here interprets
//! anything — these are pure mechanics, and centralizing them is what stops the next typo
//! from living in one copy while the other four stay correct.
//!
//! **Float identity is a contract of this module.** Every body below keeps the exact
//! expression order of the copy it replaced (`a*r.a + b*r.c`, `x0.min(other.x0)`,
//! `(x1 - x0) * (y1 - y0)`), because f32 arithmetic is not associative and a reassociated
//! sum would move rendered output by a ULP — which shows up as a real diff on the corpus.
//! When adopting a call site, keep the receiver on the side its open-coded form had it
//! (`a.min(b)` and `b.min(a)` differ for signed zero and NaN).

/// A PDF 2×3 transformation matrix `[a b c d e f]`, i.e.
/// ```text
/// | a b 0 |
/// | c d 0 |
/// | e f 1 |
/// ```
/// Row-vector convention (PDF 32000-1 §8.3.3): a point is a row vector on the left, so
/// `p × self` maps `(x, y)` and `self.mul(outer)` is "self, then outer" — the order a
/// Form XObject's `/Matrix` composes with the CTM in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Mat {
    pub(crate) a: f32,
    pub(crate) b: f32,
    pub(crate) c: f32,
    pub(crate) d: f32,
    pub(crate) e: f32,
    pub(crate) f: f32,
}

impl Mat {
    /// The identity — the CTM a page's content stream starts from.
    pub(crate) const ID: Mat = Mat { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    /// A pure translation.
    pub(crate) fn translate(tx: f32, ty: f32) -> Mat {
        Mat { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: tx, f: ty }
    }

    /// `self × r` — apply `self` first, then `r`. Not commutative.
    pub(crate) fn mul(self, r: Mat) -> Mat {
        Mat {
            a: self.a * r.a + self.b * r.c,
            b: self.a * r.b + self.b * r.d,
            c: self.c * r.a + self.d * r.c,
            d: self.c * r.b + self.d * r.d,
            e: self.e * r.a + self.f * r.c + r.e,
            f: self.e * r.b + self.f * r.d + r.f,
        }
    }

    /// Map a point through the matrix.
    pub(crate) fn apply(self, x: f32, y: f32) -> (f32, f32) {
        (x * self.a + y * self.c + self.e, x * self.b + y * self.d + self.f)
    }

    /// Average linear scale factor (`sqrt(|det|)`) — for converting line widths and other
    /// scalar lengths to device space. Zero for a degenerate (rank-deficient) matrix.
    pub(crate) fn scale(self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
}

/// The page's `/Rotate`, as the **page space → display space** map: where a point drawn at
/// page-space `(x, y)` lands once a viewer has turned the page.
///
/// Both spaces have **y up**, so every "top is larger y", "reading order is descending y"
/// rule downstream reads the same on a turned page as on an upright one — only the axes have
/// swapped. `/Rotate` turns the page CLOCKWISE, so for `90` the page's bottom edge becomes
/// the display's left edge and its left edge becomes the display's top:
///
/// ```text
///   90 : (dx, dy) = (y - y0, x1 - x)      display box = h × w
///  180 : (dx, dy) = (x1 - x, y1 - y)      display box = w × h
///  270 : (dx, dy) = (y1 - y, x - x0)      display box = h × w
/// ```
///
/// A baseline that runs `+y` in page space (`angle = +90°`) therefore runs `+x` in display
/// space under `/Rotate 90` — which is exactly why a turned page's body text reads as
/// upright once mapped, and why [`PageTurn::angle`] subtracts the turn.
///
/// **`rot == 0` is the identity, by construction and not by arithmetic:** every method
/// returns its input unchanged, so an upright page's geometry is byte-identical rather than
/// merely equal. (The same reason [`crate::vector`]'s `to_local` spells out its four closed
/// forms: `(c.2 - x0) - (c.0 - x0)` is not `c.2 - c.0` in f32, and a mapped-corner difference
/// moves an upright figure by a rounding step.)
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageTurn {
    rot: i32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl PageTurn {
    /// `rot` is [`crate::pdfobj::page_rotation`]'s normalized 0/90/180/270; `page_box` is
    /// [`crate::pdfobj::page_box`] as authored (it may be inverted, so the corners are
    /// sorted here — a turn about an inverted box would mirror the page).
    pub(crate) fn new(rot: i32, page_box: [f32; 4]) -> PageTurn {
        PageTurn {
            rot,
            x0: page_box[0].min(page_box[2]),
            y0: page_box[1].min(page_box[3]),
            x1: page_box[0].max(page_box[2]),
            y1: page_box[1].max(page_box[3]),
        }
    }

    /// `true` when this turn maps every input to itself — an upright page.
    pub(crate) fn is_identity(self) -> bool {
        self.rot == 0
    }

    /// A page-space point in display space.
    pub(crate) fn pt(self, x: f32, y: f32) -> (f32, f32) {
        match self.rot {
            90 => (y - self.y0, self.x1 - x),
            180 => (self.x1 - x, self.y1 - y),
            270 => (self.y1 - y, x - self.x0),
            _ => (x, y),
        }
    }

    /// A page-space box `(x_left, x_right, y_bottom, y_top)` in display space, in the same
    /// left/right/bottom/top form (the quarter turns transpose the two axes).
    pub(crate) fn rect(self, xl: f32, xr: f32, yb: f32, yt: f32) -> (f32, f32, f32, f32) {
        match self.rot {
            90 => (yb - self.y0, yt - self.y0, self.x1 - xr, self.x1 - xl),
            180 => (self.x1 - xr, self.x1 - xl, self.y1 - yt, self.y1 - yb),
            270 => (self.y1 - yt, self.y1 - yb, xl - self.x0, xr - self.x0),
            _ => (xl, xr, yb, yt),
        }
    }

    /// A page-space baseline angle (radians, PDF CCW-positive) in display orientation,
    /// **wrapped into `(-π, π]`**.
    ///
    /// The wrap is not cosmetic: every consumer asks `angle.abs() < ε` for "upright", and a
    /// `/Rotate 270` page's body text is authored at `-90°`, which turns into `-90 - 270 =
    /// -360°` — the same direction as upright and nothing like it under that test. Unwrapped,
    /// exactly the quarter turn this fix exists for kept emitting an empty page.
    pub(crate) fn angle(self, angle: f32) -> f32 {
        if self.rot == 0 {
            return angle;
        }
        let tau = std::f32::consts::TAU;
        let a = (angle - (self.rot as f32).to_radians()).rem_euclid(tau);
        if a > std::f32::consts::PI {
            a - tau
        } else {
            a
        }
    }
}

/// An axis-aligned rectangle `[x0, y0, x1, y1]`, in whatever space the caller is working in
/// (PDF user space with y up, or a figure-local space with y down — the operations are
/// space-agnostic).
///
/// **Invariants callers may rely on and must not re-check:**
/// - Nothing here normalizes: an inverted or degenerate rectangle stays exactly as built,
///   because a PDF may legally author one and the caller decides what that means.
///   [`Rect::is_valid`] is the explicit check.
/// - [`Rect::EMPTY`] is the identity for [`Rect::union`] and [`Rect::include`] — a fold that
///   starts there and sees no point comes back `EMPTY` (and `is_valid() == false`), never a
///   fabricated zero-size box at the origin.
/// - [`Rect::overlap_w`] / [`Rect::overlap_h`] / [`Rect::overlap_area`] clamp at zero, so a
///   disjoint pair overlaps by `0.0`, never by a negative amount.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x0: f32,
    pub(crate) y0: f32,
    pub(crate) x1: f32,
    pub(crate) y1: f32,
}

impl Rect {
    /// The inverted-infinite seed for a union/point fold: the identity of [`Rect::union`].
    pub(crate) const EMPTY: Rect = Rect {
        x0: f32::INFINITY,
        y0: f32::INFINITY,
        x1: f32::NEG_INFINITY,
        y1: f32::NEG_INFINITY,
    };

    pub(crate) fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    /// `true` when the rectangle encloses something — i.e. it is not the un-grown
    /// [`Rect::EMPTY`] seed and not inverted in x.
    pub(crate) fn is_valid(self) -> bool {
        self.x1 >= self.x0
    }

    pub(crate) fn width(self) -> f32 {
        self.x1 - self.x0
    }

    pub(crate) fn height(self) -> f32 {
        self.y1 - self.y0
    }

    /// Signed area — negative for an inverted rectangle, which the caller may care about.
    pub(crate) fn area(self) -> f32 {
        (self.x1 - self.x0) * (self.y1 - self.y0)
    }

    /// The smallest rectangle enclosing both.
    pub(crate) fn union(self, o: Rect) -> Rect {
        Rect {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }

    /// Grow to enclose a point. `Rect::EMPTY.include(p)` is the degenerate rect at `p`.
    pub(crate) fn include(&mut self, x: f32, y: f32) {
        self.x0 = self.x0.min(x);
        self.y0 = self.y0.min(y);
        self.x1 = self.x1.max(x);
        self.y1 = self.y1.max(y);
    }

    /// The overlapping region. **Not** clamped: a disjoint pair yields an inverted rect, so
    /// callers that need "did they overlap at all" ask [`Rect::is_valid`] or use
    /// [`Rect::overlap_area`].
    pub(crate) fn intersect(self, o: Rect) -> Rect {
        Rect {
            x0: self.x0.max(o.x0),
            y0: self.y0.max(o.y0),
            x1: self.x1.min(o.x1),
            y1: self.y1.min(o.y1),
        }
    }

    /// Width of the overlap, clamped at zero.
    pub(crate) fn overlap_w(self, o: Rect) -> f32 {
        (self.x1.min(o.x1) - self.x0.max(o.x0)).max(0.0)
    }

    /// Height of the overlap, clamped at zero.
    pub(crate) fn overlap_h(self, o: Rect) -> f32 {
        (self.y1.min(o.y1) - self.y0.max(o.y0)).max(0.0)
    }

    /// Area of the overlap; `0.0` when they are disjoint in either axis.
    pub(crate) fn overlap_area(self, o: Rect) -> f32 {
        self.overlap_w(o) * self.overlap_h(o)
    }

    /// Is the point inside, edges inclusive?
    pub(crate) fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

impl From<[f32; 4]> for Rect {
    /// From the `[x0, y0, x1, y1]` array form used for annotation rects and element bboxes.
    fn from(b: [f32; 4]) -> Rect {
        Rect { x0: b[0], y0: b[1], x1: b[2], y1: b[3] }
    }
}

impl From<Rect> for [f32; 4] {
    fn from(r: Rect) -> [f32; 4] {
        [r.x0, r.y0, r.x1, r.y1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    /// The page box every turn test below uses: 400 wide, 600 tall, origin at 0.
    fn turn(rot: i32) -> PageTurn {
        PageTurn::new(rot, [0.0, 0.0, 400.0, 600.0])
    }

    #[test]
    fn an_upright_page_turn_returns_its_input_untouched() {
        // Not "equal to within a rounding step" — IDENTICAL. Every upright page in every
        // document goes through this, and a mapped-corner round trip would move output by a
        // ULP (the reason `vector::to_local` spells its four forms out).
        let t = turn(0);
        assert!(t.is_identity());
        assert_eq!(t.pt(137.25, 612.5), (137.25, 612.5));
        assert_eq!(t.rect(1.5, 2.5, 3.5, 4.5), (1.5, 2.5, 3.5, 4.5));
        assert_eq!(t.angle(0.75), 0.75);
    }

    #[test]
    fn a_quarter_turn_puts_the_page_where_the_reader_sees_it() {
        // `/Rotate` turns the page CLOCKWISE. At 90 the page's BOTTOM-left corner is what the
        // reader finds at the TOP-left, and the displayed page is 600 x 400 — the check that
        // tells 90 from 270, which a symmetric assertion could not.
        let (w, h) = (400.0, 600.0);
        for (rot, corner) in [(90, (0.0, w)), (180, (w, h)), (270, (h, 0.0))] {
            assert_eq!(turn(rot).pt(0.0, 0.0), corner, "/Rotate {rot}: page origin");
        }
        // Every corner of the page box lands inside the displayed box, and the four corners
        // stay four distinct corners (a turn is rigid, not a projection).
        for rot in [90, 180, 270] {
            let (dw, dh) = if rot % 180 == 0 { (w, h) } else { (h, w) };
            let mut seen = Vec::new();
            for (x, y) in [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)] {
                let p = turn(rot).pt(x, y);
                assert!(p.0 >= 0.0 && p.0 <= dw && p.1 >= 0.0 && p.1 <= dh, "/Rotate {rot}: {p:?} outside {dw}x{dh}");
                seen.push((p.0 as i32, p.1 as i32));
            }
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), 4, "/Rotate {rot} collapsed two corners");
        }
    }

    #[test]
    fn a_turned_box_keeps_its_extents_and_its_corner_order() {
        // A box comes back left<=right, bottom<=top (the form every consumer indexes), with
        // the two extents TRANSPOSED at a quarter turn rather than recomputed from mapped
        // corners.
        let (xl, xr, yb, yt) = (100.0, 300.0, 200.0, 500.0);
        for rot in [0, 90, 180, 270] {
            let (l, r, b, t) = turn(rot).rect(xl, xr, yb, yt);
            assert!(l <= r && b <= t, "/Rotate {rot}: inverted {:?}", (l, r, b, t));
            let (want_w, want_h) = if rot % 180 == 0 { (200.0, 300.0) } else { (300.0, 200.0) };
            approx(r - l, want_w);
            approx(t - b, want_h);
        }
        // The box's corners agree with the point map — one geometry, two entry points.
        let (l, r, b, t) = turn(90).rect(xl, xr, yb, yt);
        assert_eq!(turn(90).pt(xl, yb), (l, t));
        assert_eq!(turn(90).pt(xr, yt), (r, b));
    }

    #[test]
    fn a_turned_baseline_angle_is_wrapped_not_merely_subtracted() {
        // THE bug this test exists for: a `/Rotate 270` page's body text is authored at -90
        // deg, and `-90 - 270` is -360 — the same direction as upright, and nothing like it
        // under the `angle.abs() < eps` test every consumer makes. Unwrapped, the 270 page of
        // `rotated_body.pdf` rendered EMPTY while 90 and 180 were already fixed.
        let quarter = std::f32::consts::FRAC_PI_2;
        approx(turn(90).angle(quarter), 0.0);
        approx(turn(180).angle(std::f32::consts::PI), 0.0);
        approx(turn(270).angle(-quarter), 0.0);
        // …and text that is upright IN PAGE SPACE on a turned page is sideways to the reader,
        // which is exactly what the body pipeline must keep refusing.
        for rot in [90, 180, 270] {
            let a = turn(rot).angle(0.0);
            assert!(a.abs() > 0.01, "/Rotate {rot}: page-upright text must not read as display-upright");
            assert!(a > -std::f32::consts::PI - 1e-6 && a <= std::f32::consts::PI + 1e-6, "/Rotate {rot}: {a} outside (-pi, pi]");
        }
    }

    #[test]
    fn identity_is_neutral_on_both_sides() {
        let m = Mat { a: 2.0, b: 0.5, c: -1.0, d: 3.0, e: 10.0, f: -4.0 };
        assert_eq!(m.mul(Mat::ID), m);
        assert_eq!(Mat::ID.mul(m), m);
        assert_eq!(Mat::ID.apply(7.0, -2.0), (7.0, -2.0));
    }

    #[test]
    fn mul_composes_in_row_vector_order() {
        // `a.mul(b)` must mean "a first, then b" — the order a form's /Matrix composes with
        // the CTM in. Composing then applying equals applying twice, in that order.
        let a = Mat::translate(5.0, 7.0);
        let b = Mat { a: 2.0, b: 0.0, c: 0.0, d: 2.0, e: 0.0, f: 0.0 }; // scale x2
        let (x, y) = a.mul(b).apply(1.0, 1.0);
        approx(x, 12.0); // (1+5)*2
        approx(y, 16.0); // (1+7)*2
        // The other order really is different, so this cannot pass on a commutative bug.
        let (x2, y2) = b.mul(a).apply(1.0, 1.0);
        approx(x2, 7.0);
        approx(y2, 9.0);
    }

    #[test]
    fn mul_is_associative_and_apply_agrees_with_composition() {
        let a = Mat { a: 0.7, b: 0.3, c: -0.2, d: 1.4, e: 3.0, f: -1.0 };
        let b = Mat { a: 1.1, b: -0.4, c: 0.9, d: 0.6, e: -2.0, f: 5.0 };
        let c = Mat::translate(-3.5, 2.25);
        let (lx, ly) = a.mul(b).mul(c).apply(2.0, -3.0);
        let (rx, ry) = a.mul(b.mul(c)).apply(2.0, -3.0);
        approx(lx, rx);
        approx(ly, ry);
        let (sx, sy) = a.apply(2.0, -3.0);
        let (sx, sy) = b.apply(sx, sy);
        let (sx, sy) = c.apply(sx, sy);
        approx(lx, sx);
        approx(ly, sy);
    }

    #[test]
    fn scale_is_the_sqrt_of_the_absolute_determinant() {
        approx(Mat::ID.scale(), 1.0);
        approx(Mat { a: 3.0, b: 0.0, c: 0.0, d: 3.0, e: 0.0, f: 0.0 }.scale(), 3.0);
        // A y-flip (negative determinant) still scales by 1 — a line width never goes
        // imaginary because the page is mirrored.
        approx(Mat { a: 1.0, b: 0.0, c: 0.0, d: -1.0, e: 0.0, f: 0.0 }.scale(), 1.0);
        // A 90° rotation preserves length.
        approx(Mat { a: 0.0, b: 1.0, c: -1.0, d: 0.0, e: 0.0, f: 0.0 }.scale(), 1.0);
        // A rank-deficient matrix collapses to zero rather than producing NaN.
        approx(Mat { a: 2.0, b: 4.0, c: 1.0, d: 2.0, e: 0.0, f: 0.0 }.scale(), 0.0);
    }

    #[test]
    fn empty_is_the_identity_of_union_and_never_fabricates_a_box() {
        let r = Rect::new(1.0, 2.0, 3.0, 4.0);
        assert_eq!(Rect::EMPTY.union(r), r);
        assert_eq!(r.union(Rect::EMPTY), r);
        // A fold that saw nothing stays EMPTY and reports itself invalid — it must never
        // read as a zero-size box at the origin.
        assert!(!Rect::EMPTY.is_valid());
        assert!(r.is_valid());
    }

    #[test]
    fn union_and_include_grow_to_cover_everything() {
        let a = Rect::new(0.0, 0.0, 10.0, 5.0);
        let b = Rect::new(-4.0, 2.0, 6.0, 20.0);
        assert_eq!(a.union(b), Rect::new(-4.0, 0.0, 10.0, 20.0));
        assert_eq!(a.union(b), b.union(a));
        let mut f = Rect::EMPTY;
        for &(x, y) in &[(3.0, 9.0), (-1.0, 4.0), (7.0, -2.0)] {
            f.include(x, y);
        }
        assert_eq!(f, Rect::new(-1.0, -2.0, 7.0, 9.0));
        // One point makes a degenerate — but valid — rect at that point.
        let mut one = Rect::EMPTY;
        one.include(2.0, 3.0);
        assert_eq!(one, Rect::new(2.0, 3.0, 2.0, 3.0));
        assert!(one.is_valid());
        approx(one.area(), 0.0);
    }

    #[test]
    fn intersect_reports_a_disjoint_pair_as_invalid_rather_than_empty() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 20.0, 20.0);
        assert_eq!(a.intersect(b), Rect::new(5.0, 5.0, 10.0, 10.0));
        approx(a.overlap_area(b), 25.0);
        // Disjoint: `intersect` returns the inverted rect verbatim (the caller decides), but
        // the overlap measures clamp to zero instead of going negative.
        let c = Rect::new(30.0, 30.0, 40.0, 40.0);
        assert!(!a.intersect(c).is_valid());
        approx(a.overlap_area(c), 0.0);
        approx(a.overlap_w(c), 0.0);
        approx(a.overlap_h(c), 0.0);
        // Overlapping in x but not y is still zero area, not a negative product of two
        // negative spans — the trap an unclamped `ox * oy` falls into.
        let d = Rect::new(2.0, 30.0, 8.0, 40.0);
        approx(a.overlap_w(d), 6.0);
        approx(a.overlap_h(d), 0.0);
        approx(a.overlap_area(d), 0.0);
    }

    #[test]
    fn area_and_extents_are_signed_and_never_normalized() {
        approx(Rect::new(0.0, 0.0, 4.0, 3.0).area(), 12.0);
        approx(Rect::new(0.0, 0.0, 4.0, 3.0).width(), 4.0);
        approx(Rect::new(0.0, 0.0, 4.0, 3.0).height(), 3.0);
        // An inverted rect keeps its sign: PDFs author these, and silently normalizing
        // would hide the fact from a caller that wants to notice.
        let inv = Rect::new(4.0, 3.0, 0.0, 0.0);
        approx(inv.width(), -4.0);
        approx(inv.area(), 12.0);
        assert!(!inv.is_valid());
    }

    #[test]
    fn contains_is_edge_inclusive() {
        let r = Rect::new(1.0, 1.0, 3.0, 3.0);
        assert!(r.contains(2.0, 2.0));
        assert!(r.contains(1.0, 1.0) && r.contains(3.0, 3.0)); // both corners on the edge
        assert!(!r.contains(0.999, 2.0));
        assert!(!r.contains(2.0, 3.001));
        // A degenerate rect contains exactly its own point.
        let p = Rect::new(5.0, 5.0, 5.0, 5.0);
        assert!(p.contains(5.0, 5.0));
        assert!(!p.contains(5.0, 5.001));
    }

    #[test]
    fn the_array_form_round_trips() {
        let b = [1.5, -2.0, 30.0, 40.25];
        assert_eq!(<[f32; 4]>::from(Rect::from(b)), b);
    }
}
