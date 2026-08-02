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
