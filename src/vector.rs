//! Vector-graphics → inline SVG. Many PDF figures (architecture diagrams, DAGs,
//! line plots) are drawn directly in the content stream with path operators, not
//! as raster XObjects, so [`crate::img`] never sees them. This module walks those
//! path / paint / colour operators, applies the CTM, and transcodes each
//! *substantial cluster* of vector ink into a self-contained `<svg>` (PDF's y-up
//! axis flipped to SVG's y-down within the figure's bbox).
//!
//! Conservative on purpose — only real figures are emitted; thin rules,
//! underlines, table borders and stray marks are filtered by size + ink amount.
//! Shadings / patterns / soft masks are out of scope here (skipped); text inside
//! a figure stays in the normal text flow (it is extracted as spans elsewhere).

use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct M {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}
impl M {
    const ID: M = M { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };
    fn mul(self, r: M) -> M {
        M {
            a: self.a * r.a + self.b * r.c,
            b: self.a * r.b + self.b * r.d,
            c: self.c * r.a + self.d * r.c,
            d: self.c * r.b + self.d * r.d,
            e: self.e * r.a + self.f * r.c + r.e,
            f: self.e * r.b + self.f * r.d + r.f,
        }
    }
    fn apply(self, x: f32, y: f32) -> (f32, f32) {
        (x * self.a + y * self.c + self.e, x * self.b + y * self.d + self.f)
    }
    /// Average linear scale factor (for converting line widths to device space).
    fn scale(self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
}

fn num(o: &Object) -> f32 {
    match o {
        Object::Integer(i) => *i as f32,
        Object::Real(r) => *r,
        _ => 0.0,
    }
}

fn gray(g: f32) -> [u8; 3] {
    let v = (g.clamp(0.0, 1.0) * 255.0).round() as u8;
    [v, v, v]
}
fn rgb(r: f32, g: f32, b: f32) -> [u8; 3] {
    [(r.clamp(0.0, 1.0) * 255.0).round() as u8, (g.clamp(0.0, 1.0) * 255.0).round() as u8, (b.clamp(0.0, 1.0) * 255.0).round() as u8]
}
fn cmyk(c: f32, m: f32, y: f32, k: f32) -> [u8; 3] {
    rgb((1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k))
}

/// One path segment, points already in PDF page space (CTM applied).
#[derive(Clone)]
enum Seg {
    M(f32, f32),
    L(f32, f32),
    C(f32, f32, f32, f32, f32, f32),
    Z,
}

/// A painted path with its colours, opacities and page-space bounding box.
struct Painted {
    segs: Vec<Seg>,
    fill: Option<[u8; 3]>,
    stroke: Option<([u8; 3], f32)>,
    fill_op: f32,
    stroke_op: f32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    seq: usize, // paint order in the content stream — preserved for correct z-order
    // Active clip rect (page space) when this path was painted, if it actually crops it.
    // Rendered as an SVG <clipPath> so the visible ink matches the PDF (no overshoot).
    clip: Option<(f32, f32, f32, f32)>,
}

/// Graphics state carried through the walk and the q/Q stack.
#[derive(Clone, Copy)]
struct GState {
    ctm: M,
    fill: [u8; 3],
    stroke: [u8; 3],
    lw: f32,
    fill_a: f32,   // ExtGState `ca` — fill alpha
    stroke_a: f32, // ExtGState `CA` — stroke alpha
    // Active clipping rectangle in PAGE space (x0, y0, x1, y1), the intersection of every
    // `W`/`W*` clip seen so far on the q/Q stack. `None` = unclipped (page bounds). A plot
    // clips its reference curves to the axes box; honouring it crops the curve overshoot.
    clip: Option<(f32, f32, f32, f32)>,
}
impl GState {
    fn new(ctm: M, fill: [u8; 3], stroke: [u8; 3], lw: f32, fill_a: f32, stroke_a: f32) -> GState {
        GState { ctm, fill, stroke, lw, fill_a, stroke_a, clip: None }
    }
}

const ALPHA_HIDDEN: f32 = 0.04; // below this, a paint is effectively invisible — drop it

/// A form-internal text label (page space, y up) destined for a figure's SVG.
pub struct LabelSpan {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub width: f32,
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub angle: f32, // baseline rotation (radians, PDF CCW); 0 = upright
}

/// A label already mapped into a figure's local SVG coords (y down).
struct Label {
    lx: f32,
    ly: f32, // text baseline
    size: f32,
    w: f32,
    text: String,
    bold: bool,
    italic: bool,
    angle: f32,
}

/// A vector figure placed on the page: bbox (PDF space, y up) plus the geometry
/// needed to render its self-contained `<svg>`. Rendering is deferred (via
/// [`PlacedSvg::svg`]) so the viewBox can grow to include text labels attached
/// after construction — edge labels above/beside the vector ink must not clip.
pub struct PlacedSvg {
    pub y_top: f32,
    pub y_bottom: f32,
    pub x_left: f32,
    pub x_right: f32,
    paths: String, // <path> elements, local coords: origin (x_left, y_top), y down
    w: f32,        // vector-ink content extent
    h: f32,
    page_w: f32, // page width — figure renders at its page-width share
    labels: Vec<Label>,
    // Local bbox of the figure's opaque background rect (a plot's plot-area), if any. When
    // present the viewBox is bounded to it (plus labels) so path ink overshooting the plot
    // — reference curves the PDF clips to the axes box — is cropped by the SVG viewport
    // instead of trailing far past the figure.
    plot: Option<(f32, f32, f32, f32)>,
}

// A label whose centre is within this margin (pt) of the vector-ink bbox is taken
// to belong to the figure (form text sits just outside the boxes it annotates).
const LABEL_MARGIN: f32 = 24.0;

impl PlacedSvg {
    /// Attach form-internal text spans that belong to this figure, mapping each
    /// into local SVG coords. A span is claimed when its centre lies within the
    /// bbox expanded by [`LABEL_MARGIN`].
    fn attach(&mut self, spans: &[LabelSpan]) {
        for s in spans {
            let cx = s.x + s.width * 0.5;
            let cy = s.y + s.size * 0.5;
            if cx >= self.x_left - LABEL_MARGIN
                && cx <= self.x_right + LABEL_MARGIN
                && cy >= self.y_bottom - LABEL_MARGIN
                && cy <= self.y_top + LABEL_MARGIN
            {
                self.labels.push(Label {
                    lx: s.x - self.x_left,
                    ly: self.y_top - s.y,
                    size: s.size,
                    w: s.width,
                    text: s.text.clone(),
                    bold: s.bold,
                    italic: s.italic,
                    angle: s.angle,
                });
            }
        }
    }

    /// Render the self-contained `<svg>`. The viewBox spans the union of the
    /// vector ink and every attached label, so nothing clips; the displayed width
    /// is the figure's share of the page width, centred (matching the page).
    pub fn svg(&self) -> String {
        // viewBox: union of vector content [0,w]x[0,h] and label extents. A glyph
        // run occupies [lx, lx+w] horizontally and (allowing ascenders above the
        // baseline and descenders below it) [ly-size, ly+0.25*size] vertically.
        // Base the viewBox on the plot area when one was detected (so reference curves the
        // PDF clips to the axes box don't trail far past the figure); else the full ink.
        let (mut min_x, mut min_y, mut max_x, mut max_y) = self.plot.unwrap_or((0.0, 0.0, self.w, self.h));
        for l in &self.labels {
            // Text box in local coords (baseline at ly): [lx, lx+w] × [ly-size, ly+0.25size].
            // For a rotated label, rotate the four corners about the anchor (lx,ly) so the
            // viewBox grows to the text's true (vertical) extent and nothing clips.
            let svg_rad = -l.angle; // SVG y-down negates the PDF (y-up, CCW) angle
            let (sin, cos) = (svg_rad.sin(), svg_rad.cos());
            for (px, py) in [(l.lx, l.ly - l.size), (l.lx + l.w, l.ly - l.size), (l.lx + l.w, l.ly + l.size * 0.25), (l.lx, l.ly + l.size * 0.25)] {
                let (dx, dy) = (px - l.lx, py - l.ly);
                let (rx, ry) = (l.lx + dx * cos - dy * sin, l.ly + dx * sin + dy * cos);
                min_x = min_x.min(rx);
                min_y = min_y.min(ry);
                max_x = max_x.max(rx);
                max_y = max_y.max(ry);
            }
        }
        // Pad the box so strokes on the boundary (drawn half a line-width outside
        // their path) and any glyph overshoot are not clipped at the edges.
        const PAD: f32 = 4.0;
        min_x -= PAD;
        min_y -= PAD;
        max_x += PAD;
        max_y += PAD;
        let (vbw, vbh) = (max_x - min_x, max_y - min_y);
        let mut texts = String::new();
        for l in &self.labels {
            let weight = if l.bold { " font-weight=\"bold\"" } else { "" };
            let style = if l.italic { " font-style=\"italic\"" } else { "" };
            // Rotated label (e.g. a 90° y-axis title): rotate about its anchor. SVG's
            // y-down frame makes a positive rotation clockwise, so negate the PDF angle.
            let transform = if l.angle.abs() > 0.01 {
                format!(" transform=\"rotate({} {} {})\"", fmt(-l.angle * 180.0 / std::f32::consts::PI), fmt(l.lx), fmt(l.ly))
            } else {
                String::new()
            };
            texts.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\"{weight}{style}{transform}>{}</text>",
                fmt(l.lx),
                fmt(l.ly),
                fmt(l.size),
                esc(&l.text)
            ));
        }
        // Render at 1.5× the figure's share of the page width, capped at the body width
        // (100%). On the PDF page a figure shares space with margins/columns, so its raw
        // page fraction reads small in a single-column web layout; the 1.5× upscale makes
        // plots/diagrams comfortably legible while the 100% clamp keeps it within the
        // body. (A figure already ≥⅔ of the page width simply fills the body width.)
        let pct = if self.page_w > 1.0 { (vbw / self.page_w * 150.0).clamp(10.0, 100.0) } else { 100.0 };
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\" \
             style=\"display:block;width:{}%;height:auto;margin:0 auto\" \
             font-family=\"sans-serif\" fill=\"#000\">{}{}</svg>",
            fmt(min_x),
            fmt(min_y),
            fmt(vbw),
            fmt(vbh),
            fmt(pct),
            self.paths,
            texts
        )
    }

    /// Render the `<text>` labels of this figure as SVG, in its local coords.
    fn label_texts(&self) -> String {
        let mut texts = String::new();
        for l in &self.labels {
            let weight = if l.bold { " font-weight=\"bold\"" } else { "" };
            let style = if l.italic { " font-style=\"italic\"" } else { "" };
            let transform = if l.angle.abs() > 0.01 {
                format!(" transform=\"rotate({} {} {})\"", fmt(-l.angle * 180.0 / std::f32::consts::PI), fmt(l.lx), fmt(l.ly))
            } else {
                String::new()
            };
            texts.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-size=\"{}\"{weight}{style}{transform}>{}</text>",
                fmt(l.lx),
                fmt(l.ly),
                fmt(l.size),
                esc(&l.text)
            ));
        }
        texts
    }

    /// Render an OVERLAY `<svg>` for compositing over a raster image: the viewBox is the
    /// vector INK box (not expanded to include labels), so labels that fall outside it —
    /// e.g. body prose the figure picked up below the map — are clipped by the SVG
    /// viewport. `style` (caller-supplied) positions it over the image; `preserveAspect
    /// Ratio="none"` makes the ink fill the positioned box exactly, so the polygons line
    /// up with the raster (both are in page coordinates).
    pub fn overlay_svg(&self, style: &str) -> String {
        const PAD: f32 = 1.0;
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\" \
             preserveAspectRatio=\"none\" style=\"{}\" font-family=\"sans-serif\" fill=\"#000\">{}{}</svg>",
            fmt(-PAD),
            fmt(-PAD),
            fmt(self.w + 2.0 * PAD),
            fmt(self.h + 2.0 * PAD),
            style,
            self.paths,
            self.label_texts()
        )
    }

    /// Render ONE self-contained `<svg>` that composites one or more raster images WITH
    /// this figure's vector ink and labels — all in the figure's local user space, so they
    /// register pixel-for-pixel. The viewBox is the union of every raster rect, the vector
    /// ink, and every label, so nothing is clipped (axis labels in the margins included).
    /// Works in BOTH directions: a vector OVER a base raster (a location map: vector lines/
    /// labels over a base photo) and rasters INSIDE a larger vector frame (a plot whose
    /// data points are a raster within the axes/legend). The raster sits behind the ink;
    /// the vector's opaque plot-area background is dropped in `build_svg`, so the raster
    /// shows and the grid/curves/axes overlay it. Each entry is
    /// `(href, (x_left, x_right, y_bottom, y_top))`: the source and its PDF page rect (y up).
    pub fn composite_svg(&self, rasters: &[(&str, (f32, f32, f32, f32))]) -> String {
        // viewBox base: the plot area if detected (crops overshooting reference curves).
        // When no plot box was found, start from an empty box and grow it from the rasters
        // + labels only (NOT the full ink): that still bounds the figure to its real content
        // — the data raster, axes ticks and legend text — so a curve that trails below the
        // plot is clipped by the SVG viewport. We fall back to the full ink [0,w]×[0,h] only
        // if there is nothing to anchor on (no raster, no label).
        let have_plot = self.plot.is_some();
        let (mut min_x, mut min_y, mut max_x, mut max_y) = self
            .plot
            .unwrap_or((f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY));
        // Raster rects in local coords (origin (x_left, y_top), y DOWN).
        let mut images = String::new();
        for (href, (ix0, ix1, iy0, iy1)) in rasters {
            let img_lx = ix0 - self.x_left;
            let img_ly = self.y_top - iy1;
            let img_lw = (ix1 - ix0).max(0.1);
            let img_lh = (iy1 - iy0).max(0.1);
            min_x = min_x.min(img_lx);
            min_y = min_y.min(img_ly);
            max_x = max_x.max(img_lx + img_lw);
            max_y = max_y.max(img_ly + img_lh);
            images.push_str(&format!(
                "<image href=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\"/>",
                href,
                fmt(img_lx),
                fmt(img_ly),
                fmt(img_lw),
                fmt(img_lh)
            ));
        }
        // Grow to every (rotation-aware) label too.
        for l in &self.labels {
            let svg_rad = -l.angle;
            let (sin, cos) = (svg_rad.sin(), svg_rad.cos());
            for (px, py) in [(l.lx, l.ly - l.size), (l.lx + l.w, l.ly - l.size), (l.lx + l.w, l.ly + l.size * 0.25), (l.lx, l.ly + l.size * 0.25)] {
                let (dx, dy) = (px - l.lx, py - l.ly);
                let (rx, ry) = (l.lx + dx * cos - dy * sin, l.ly + dx * sin + dy * cos);
                min_x = min_x.min(rx);
                min_y = min_y.min(ry);
                max_x = max_x.max(rx);
                max_y = max_y.max(ry);
            }
        }
        // Nothing to anchor the viewBox on (no plot box, no raster, no label): fall back to
        // the full vector ink so we never emit a degenerate/infinite viewBox.
        if !have_plot && !min_x.is_finite() {
            min_x = 0.0;
            min_y = 0.0;
            max_x = self.w;
            max_y = self.h;
        }
        const PAD: f32 = 4.0;
        min_x -= PAD;
        min_y -= PAD;
        max_x += PAD;
        max_y += PAD;
        let (vbw, vbh) = (max_x - min_x, max_y - min_y);
        let pct = if self.page_w > 1.0 { (vbw / self.page_w * 150.0).clamp(10.0, 100.0) } else { 100.0 };
        // Rasters behind, then the vector ink, then the text labels on top.
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\" \
             style=\"display:block;width:{}%;height:auto;margin:0 auto\" \
             font-family=\"sans-serif\" fill=\"#000\">{}{}{}</svg>",
            fmt(min_x),
            fmt(min_y),
            fmt(vbw),
            fmt(vbh),
            fmt(pct),
            images,
            self.paths,
            self.label_texts()
        )
    }
}

// Figure filter: a real vector figure is a cluster of ink at least this big with
// at least this many painted paths (so single rules / underlines / a lone box
// don't qualify).
const MIN_W: f32 = 72.0;
const MIN_H: f32 = 54.0;
const MIN_PATHS: usize = 6;
const BAND_GAP: f32 = 24.0; // vertical gap that separates two figures
const MAX_OPS: usize = 60_000; // bail on pathologically dense pages

fn deref<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Object> {
    match o {
        Object::Reference(r) => doc.get_object(*r).ok(),
        other => Some(other),
    }
}

/// XObject name -> object id (images AND forms) from a resources dict.
fn xobjects_of(doc: &Document, resources: &Dictionary) -> HashMap<Vec<u8>, ObjectId> {
    let mut map = HashMap::new();
    if let Some(xd) = resources.get(b"XObject").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        for (name, val) in xd.iter() {
            if let Ok(id) = val.as_reference() {
                map.insert(name.clone(), id);
            }
        }
    }
    map
}

fn page_resources(doc: &Document, page_id: ObjectId) -> Option<Dictionary> {
    match doc.get_page_resources(page_id) {
        Ok((Some(d), _)) => Some(d.clone()),
        Ok((None, ids)) => ids.first().and_then(|id| doc.get_dictionary(*id).ok()).cloned(),
        Err(_) => None,
    }
}

/// ExtGState name -> (fill alpha `ca`, stroke alpha `CA`) where defined.
fn extgstates_of(doc: &Document, resources: &Dictionary) -> HashMap<Vec<u8>, (Option<f32>, Option<f32>)> {
    let mut map = HashMap::new();
    if let Some(eg) = resources.get(b"ExtGState").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        for (name, val) in eg.iter() {
            if let Some(d) = deref(doc, val).and_then(|o| o.as_dict().ok()) {
                let ca = d.get(b"ca").ok().map(num);
                let big = d.get(b"CA").ok().map(num);
                map.insert(name.clone(), (ca, big));
            }
        }
    }
    map
}

/// Finish the current subpath: compute its bbox and push it as a painted path.
/// A path with neither fill nor stroke (e.g. a fully transparent `ca 0` fill, or
/// a clip-only path) carries no ink and is dropped — it must not inflate a
/// figure cluster or paint a "hidden" black field.
fn finish(cur: &mut Vec<Seg>, fill: Option<[u8; 3]>, stroke: Option<([u8; 3], f32)>, fill_op: f32, stroke_op: f32, clip: Option<(f32, f32, f32, f32)>, out: &mut Vec<Painted>) {
    if cur.is_empty() {
        return;
    }
    if fill.is_none() && stroke.is_none() {
        cur.clear();
        return;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for s in cur.iter() {
        let pts: &[(f32, f32)] = match s {
            Seg::M(x, y) | Seg::L(x, y) => &[(*x, *y)],
            Seg::C(a, b, c, d, e, f) => &[(*a, *b), (*c, *d), (*e, *f)],
            Seg::Z => &[],
        };
        for &(x, y) in pts {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    // Drop paths whose extent is implausibly large. A real figure element never exceeds
    // page size (~800 pt); a span of thousands+ means a coordinate was left in the wrong
    // space (page coords leaking into a figure-local frame, a mis-applied matrix), which
    // otherwise draws a line shooting off the figure or collapses its viewBox.
    const MAX_EXTENT: f32 = 2000.0;
    if x1 < x0 || (x1 - x0).max(y1 - y0) > MAX_EXTENT {
        cur.clear();
        return;
    }
    // Honour the active clip: a path drawn under a tighter clip than its own extent (a
    // plot's reference curve clipped to the axes box) only *shows* the clipped portion.
    // Crop the stored bbox to that intersection so the figure's extent and viewBox exclude
    // the overshoot; the full geometry stays in `segs` and is masked by an SVG <clipPath>
    // at render time. Keep `clip` only when it actually crops (so we don't emit no-op masks
    // for the ubiquitous full-page `re W n`).
    let mut crop = None;
    if let Some((cx0, cy0, cx1, cy1)) = clip {
        let crops = cx0 > x0 + 0.5 || cy0 > y0 + 0.5 || cx1 < x1 - 0.5 || cy1 < y1 - 0.5;
        if crops {
            let (nx0, ny0, nx1, ny1) = (x0.max(cx0), y0.max(cy0), x1.min(cx1), y1.min(cy1));
            if nx1 <= nx0 || ny1 <= ny0 {
                cur.clear(); // path lies entirely outside its clip — invisible
                return;
            }
            x0 = nx0;
            y0 = ny0;
            x1 = nx1;
            y1 = ny1;
            crop = clip;
        }
    }
    out.push(Painted { segs: std::mem::take(cur), fill, stroke, fill_op, stroke_op, x0, y0, x1, y1, seq: 0, clip: crop });
}

/// Page-space bounding box of a path under construction (a clip path is just a path
/// followed by `W`/`W*`); `None` if it has no points.
fn path_bbox(cur: &[Seg]) -> Option<(f32, f32, f32, f32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for s in cur {
        let pts: &[(f32, f32)] = match s {
            Seg::M(x, y) | Seg::L(x, y) => &[(*x, *y)],
            Seg::C(a, b, c, d, e, f) => &[(*a, *b), (*c, *d), (*e, *f)],
            Seg::Z => &[],
        };
        for &(x, y) in pts {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x1 >= x0 {
        Some((x0, y0, x1, y1))
    } else {
        None
    }
}

/// Vector figures on a page, top-to-bottom.
pub fn positioned_vectors(doc: &Document, page_id: ObjectId) -> Vec<PlacedSvg> {
    let resources = match page_resources(doc, page_id) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let content = match doc.get_and_decode_page_content(page_id) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    if content.operations.len() > MAX_OPS {
        return Vec::new();
    }
    let xmap = xobjects_of(doc, &resources);
    let egmap = extgstates_of(doc, &resources);
    let mut painted = Vec::new();
    walk(doc, &content.operations, &xmap, &egmap, GState::new(M::ID, [0; 3], [0; 3], 1.0, 1.0, 1.0), &mut painted, 0);
    // Stamp paint order before clustering reshuffles the vector for banding.
    for (i, p) in painted.iter_mut().enumerate() {
        p.seq = i;
    }
    let page_w = page_width(doc, page_id);
    let figures = cluster_figures(painted);
    figures.iter().map(|c| build_svg(c, page_w)).collect()
}

/// Page width from the MediaBox (used to size each figure as a share of the page).
fn page_width(doc: &Document, page_id: ObjectId) -> f32 {
    doc.get_object(page_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| {
            d.get(b"MediaBox")
                .ok()
                .or_else(|| d.get(b"CropBox").ok())
                .and_then(|o| deref(doc, o))
                .and_then(|o| o.as_array().ok())
        })
        .filter(|a| a.len() >= 4)
        .map(|a| (num(&a[2]) - num(&a[0])).abs())
        .filter(|w| *w > 1.0)
        .unwrap_or(612.0)
}

/// Distribute form-internal text labels among the figures on a page (each label
/// goes to the figure whose bbox, expanded by a margin, contains its centre).
pub fn attach_labels(figs: &mut [PlacedSvg], spans: &[LabelSpan]) {
    for f in figs.iter_mut() {
        f.attach(spans);
    }
}

/// Walk a content stream, collecting painted paths in page space. Recurses into
/// Form XObjects (most figures are a single form `Do`) applying the form `/Matrix`
/// — without this, vector figures drawn inside a form are invisible. Images are
/// left to [`crate::img`].
#[allow(clippy::too_many_arguments)]
fn walk(
    doc: &Document,
    ops: &[lopdf::content::Operation],
    xmap: &HashMap<Vec<u8>, ObjectId>,
    egmap: &HashMap<Vec<u8>, (Option<f32>, Option<f32>)>,
    base: GState,
    out: &mut Vec<Painted>,
    depth: u32,
) {
    if depth > 8 {
        return;
    }
    let mut g = base;
    let mut stack: Vec<GState> = Vec::new();
    let mut cur: Vec<Seg> = Vec::new();
    // `W`/`W*` mark the current path as a clip, but it takes effect only after the path's
    // painting operator. Defer it: set this flag on `W`/`W*`, fold it into `g.clip` when the
    // path is painted/ended.
    let mut pending_clip = false;
    // Effective fill/stroke for a paint op, after applying ExtGState alpha: a
    // ~zero alpha means the paint is invisible (so it is dropped, not blacked in).
    let eff_fill = |g: &GState| if g.fill_a >= ALPHA_HIDDEN { Some(g.fill) } else { None };
    let eff_stroke = |g: &GState| if g.stroke_a >= ALPHA_HIDDEN { Some((g.stroke, (g.lw * g.ctm.scale()).max(0.3))) } else { None };

    for op in ops {
        let o = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push(g),
            "Q" => {
                if let Some(s) = stack.pop() {
                    g = s;
                }
            }
            "cm" if o.len() >= 6 => {
                g.ctm = M { a: num(&o[0]), b: num(&o[1]), c: num(&o[2]), d: num(&o[3]), e: num(&o[4]), f: num(&o[5]) }.mul(g.ctm);
            }
            "gs" => {
                if let Some(&(ca, big)) = o.first().and_then(|x| x.as_name().ok()).and_then(|n| egmap.get(n)) {
                    if let Some(a) = ca {
                        g.fill_a = a;
                    }
                    if let Some(a) = big {
                        g.stroke_a = a;
                    }
                }
            }
            "w" if !o.is_empty() => g.lw = num(&o[0]),
            "g" if !o.is_empty() => g.fill = gray(num(&o[0])),
            "G" if !o.is_empty() => g.stroke = gray(num(&o[0])),
            "rg" if o.len() >= 3 => g.fill = rgb(num(&o[0]), num(&o[1]), num(&o[2])),
            "RG" if o.len() >= 3 => g.stroke = rgb(num(&o[0]), num(&o[1]), num(&o[2])),
            "k" if o.len() >= 4 => g.fill = cmyk(num(&o[0]), num(&o[1]), num(&o[2]), num(&o[3])),
            "K" if o.len() >= 4 => g.stroke = cmyk(num(&o[0]), num(&o[1]), num(&o[2]), num(&o[3])),
            "sc" | "scn" => {
                if let Some(c) = comps(o) {
                    g.fill = c;
                }
            }
            "SC" | "SCN" => {
                if let Some(c) = comps(o) {
                    g.stroke = c;
                }
            }
            "m" if o.len() >= 2 => {
                let (x, y) = g.ctm.apply(num(&o[0]), num(&o[1]));
                cur.push(Seg::M(x, y));
            }
            "l" if o.len() >= 2 => {
                let (x, y) = g.ctm.apply(num(&o[0]), num(&o[1]));
                cur.push(Seg::L(x, y));
            }
            "c" if o.len() >= 6 => {
                let p1 = g.ctm.apply(num(&o[0]), num(&o[1]));
                let p2 = g.ctm.apply(num(&o[2]), num(&o[3]));
                let p3 = g.ctm.apply(num(&o[4]), num(&o[5]));
                cur.push(Seg::C(p1.0, p1.1, p2.0, p2.1, p3.0, p3.1));
            }
            "v" if o.len() >= 4 => {
                let last = cur.last().and_then(|s| match s {
                    Seg::M(x, y) | Seg::L(x, y) => Some((*x, *y)),
                    Seg::C(_, _, _, _, x, y) => Some((*x, *y)),
                    _ => None,
                });
                let (sx, sy) = last.unwrap_or((0.0, 0.0));
                let p2 = g.ctm.apply(num(&o[0]), num(&o[1]));
                let p3 = g.ctm.apply(num(&o[2]), num(&o[3]));
                cur.push(Seg::C(sx, sy, p2.0, p2.1, p3.0, p3.1));
            }
            "y" if o.len() >= 4 => {
                let p1 = g.ctm.apply(num(&o[0]), num(&o[1]));
                let p3 = g.ctm.apply(num(&o[2]), num(&o[3]));
                cur.push(Seg::C(p1.0, p1.1, p3.0, p3.1, p3.0, p3.1));
            }
            "re" if o.len() >= 4 => {
                let (x, y, w, h) = (num(&o[0]), num(&o[1]), num(&o[2]), num(&o[3]));
                let p = [g.ctm.apply(x, y), g.ctm.apply(x + w, y), g.ctm.apply(x + w, y + h), g.ctm.apply(x, y + h)];
                cur.push(Seg::M(p[0].0, p[0].1));
                cur.push(Seg::L(p[1].0, p[1].1));
                cur.push(Seg::L(p[2].0, p[2].1));
                cur.push(Seg::L(p[3].0, p[3].1));
                cur.push(Seg::Z);
            }
            "h" => cur.push(Seg::Z),
            "W" | "W*" => pending_clip = true,
            "f" | "F" | "f*" | "S" | "s" | "B" | "B*" | "b" | "b*" | "n" => {
                // A pending `W`/`W*` clip applies after this paint op: intersect the current
                // path's bbox into the graphics-state clip (q/Q scopes it via the GState copy).
                if pending_clip {
                    if let Some(bb) = path_bbox(&cur) {
                        g.clip = Some(match g.clip {
                            Some((x0, y0, x1, y1)) => (x0.max(bb.0), y0.max(bb.1), x1.min(bb.2), y1.min(bb.3)),
                            None => bb,
                        });
                    }
                    pending_clip = false;
                }
                match op.operator.as_str() {
                    "f" | "F" | "f*" => finish(&mut cur, eff_fill(&g), None, g.fill_a, g.stroke_a, g.clip, out),
                    "S" | "s" => finish(&mut cur, None, eff_stroke(&g), g.fill_a, g.stroke_a, g.clip, out),
                    "B" | "B*" | "b" | "b*" => finish(&mut cur, eff_fill(&g), eff_stroke(&g), g.fill_a, g.stroke_a, g.clip, out),
                    _ => cur.clear(), // "n": clip-only path → no ink
                }
            }
            "Do" => {
                let id = match o.first().and_then(|x| x.as_name().ok()).and_then(|n| xmap.get(n)) {
                    Some(&id) => id,
                    None => continue,
                };
                let stream = match doc.get_object(id).and_then(|x| x.as_stream().map(|s| s.clone())) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if stream.dict.get(b"Subtype").and_then(|x| x.as_name()).unwrap_or(b"") != b"Form" {
                    continue; // images handled by crate::img
                }
                let fm = stream
                    .dict
                    .get(b"Matrix")
                    .ok()
                    .and_then(|x| x.as_array().ok())
                    .filter(|a| a.len() >= 6)
                    .map(|a| M { a: num(&a[0]), b: num(&a[1]), c: num(&a[2]), d: num(&a[3]), e: num(&a[4]), f: num(&a[5]) })
                    .unwrap_or(M::ID);
                let (mut child_x, mut child_eg) = (xmap.clone(), egmap.clone());
                if let Some(fr) = stream.dict.get(b"Resources").ok().and_then(|x| deref(doc, x)).and_then(|x| x.as_dict().ok()) {
                    for (k, v) in xobjects_of(doc, fr) {
                        child_x.insert(k, v);
                    }
                    for (k, v) in extgstates_of(doc, fr) {
                        child_eg.insert(k, v);
                    }
                }
                if let Ok(content) = lopdf::content::Content::decode(&stream.decompressed_content().unwrap_or_default()) {
                    let mut sub = g;
                    sub.ctm = fm.mul(g.ctm);
                    walk(doc, &content.operations, &child_x, &child_eg, sub, out, depth + 1);
                }
            }
            _ => {}
        }
    }
}

/// sc/scn operands → colour by component count (1 gray, 3 rgb, 4 cmyk). A
/// trailing name (pattern) yields no usable colour.
fn comps(o: &[Object]) -> Option<[u8; 3]> {
    let nums: Vec<f32> = o.iter().take_while(|x| matches!(x, Object::Integer(_) | Object::Real(_))).map(num).collect();
    match nums.len() {
        1 => Some(gray(nums[0])),
        3 => Some(rgb(nums[0], nums[1], nums[2])),
        4 => Some(cmyk(nums[0], nums[1], nums[2], nums[3])),
        _ => None,
    }
}

/// Group painted paths into vertically-contiguous clusters, keep only the ones
/// big enough and inky enough to be real figures.
fn cluster_figures(mut paths: Vec<Painted>) -> Vec<Vec<Painted>> {
    // Drop full-page background fills (a single huge rectangle) up front.
    paths.retain(|p| !(p.x1 - p.x0 > 400.0 && p.y1 - p.y0 > 600.0 && p.segs.len() <= 5));
    if paths.is_empty() {
        return Vec::new();
    }
    paths.sort_by(|a, b| b.y1.partial_cmp(&a.y1).unwrap_or(std::cmp::Ordering::Equal));
    let mut clusters: Vec<Vec<Painted>> = Vec::new();
    let mut band_lo = f32::INFINITY; // current cluster's lowest y
    for p in paths {
        if let Some(cur) = clusters.last_mut() {
            if p.y1 >= band_lo - BAND_GAP {
                band_lo = band_lo.min(p.y0);
                cur.push(p);
                continue;
            }
        }
        band_lo = p.y0;
        clusters.push(vec![p]);
    }
    clusters.retain(|c| {
        let x0 = c.iter().map(|p| p.x0).fold(f32::INFINITY, f32::min);
        let x1 = c.iter().map(|p| p.x1).fold(f32::NEG_INFINITY, f32::max);
        let y0 = c.iter().map(|p| p.y0).fold(f32::INFINITY, f32::min);
        let y1 = c.iter().map(|p| p.y1).fold(f32::NEG_INFINITY, f32::max);
        c.len() >= MIN_PATHS && x1 - x0 >= MIN_W && y1 - y0 >= MIN_H
    });
    // Restore stream paint order within each cluster (banding sorted by y): a fill
    // drawn after an outline must paint on top of it, not be reordered by position.
    for c in &mut clusters {
        c.sort_by_key(|p| p.seq);
    }
    clusters
}

fn fmt(v: f32) -> String {
    // compact: 2 decimals, trim trailing zeros
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" { "0".into() } else { s.into() }
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Transcode one figure cluster into the path geometry of a [`PlacedSvg`]
/// (paths in stream order, y flipped). The `<svg>` wrapper + any text labels are
/// emitted later by [`PlacedSvg::svg`].
fn build_svg(cluster: &Vec<Painted>, page_w: f32) -> PlacedSvg {
    let x0 = cluster.iter().map(|p| p.x0).fold(f32::INFINITY, f32::min);
    let x1 = cluster.iter().map(|p| p.x1).fold(f32::NEG_INFINITY, f32::max);
    let y0 = cluster.iter().map(|p| p.y0).fold(f32::INFINITY, f32::min);
    let y1 = cluster.iter().map(|p| p.y1).fold(f32::NEG_INFINITY, f32::max);
    let (w, h) = (x1 - x0, y1 - y0);
    // page space (y up) -> local SVG space (y down): lx = x-x0, ly = y1-y. A stray point
    // (one coordinate left in the wrong space, surviving the per-path extent gate) is
    // clamped to within one figure-extent of the box, so it can never draw a huge line.
    let tx = |x: f32| fmt((x - x0).clamp(-w, 2.0 * w));
    let ty = |y: f32| fmt((y1 - y).clamp(-h, 2.0 * h));

    let area = (w * h).max(1.0);
    let mut plot: Option<(f32, f32, f32, f32)> = None;
    // SVG <clipPath> definitions for paths drawn under a PDF clip (a plot's reference curves
    // clipped to the axes box). Deduped per distinct rect; ids are namespaced by the figure
    // origin so they stay unique across every figure in the page's HTML.
    let id_prefix = format!("{}_{}", x0 as i32, y1 as i32);
    let mut clip_defs = String::new();
    let mut clip_ids: Vec<((i32, i32, i32, i32), String)> = Vec::new();
    let mut clip_id_for = |c: (f32, f32, f32, f32), defs: &mut String| -> String {
        // page space -> figure-local (y flipped): a clip rect (cx0,cy0,cx1,cy1).
        let (lx, lw_) = (c.0 - x0, (c.2 - c.0).max(0.0));
        let (ly, lh_) = (y1 - c.3, (c.3 - c.1).max(0.0));
        let key = ((lx * 4.0) as i32, (ly * 4.0) as i32, (lw_ * 4.0) as i32, (lh_ * 4.0) as i32);
        if let Some((_, id)) = clip_ids.iter().find(|(k, _)| *k == key) {
            return id.clone();
        }
        let id = format!("c{}_{}", id_prefix, clip_ids.len());
        defs.push_str(&format!("<clipPath id=\"{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/></clipPath>", id, fmt(lx), fmt(ly), fmt(lw_), fmt(lh_)));
        clip_ids.push((key, id.clone()));
        id
    };
    let mut paths = String::new();
    for p in cluster {
        // Skip a near-white background fill that covers a large part of the figure:
        // invisible on the white page anyway, and in a raster+vector composite it would
        // otherwise occlude the embedded raster (a plot's opaque white plot-area behind
        // its data). The plot background covers the axes box but not the legend / overshoot
        // curves, so a moderate area share (not near-100%) must qualify. Remember its local
        // bbox as the plot area, used to crop overshooting ink (uncliped reference curves).
        if p.stroke.is_none() {
            if let Some([r, g, b]) = p.fill {
                let pa = (p.x1 - p.x0).max(0.0) * (p.y1 - p.y0).max(0.0);
                if r >= 248 && g >= 248 && b >= 248 && pa >= area * 0.3 {
                    let (bx0, bx1, by0, by1) = (p.x0 - x0, p.x1 - x0, y1 - p.y1, y1 - p.y0);
                    plot = Some(match plot {
                        Some((mx0, my0, mx1, my1)) => (mx0.min(bx0), my0.min(by0), mx1.max(bx1), my1.max(by1)),
                        None => (bx0, by0, bx1, by1),
                    });
                    continue;
                }
            }
        }
        let mut d = String::new();
        for s in &p.segs {
            match *s {
                Seg::M(x, y) => d.push_str(&format!("M{} {}", tx(x), ty(y))),
                Seg::L(x, y) => d.push_str(&format!("L{} {}", tx(x), ty(y))),
                Seg::C(a, b, c, dd, e, f) => {
                    d.push_str(&format!("C{} {} {} {} {} {}", tx(a), ty(b), tx(c), ty(dd), tx(e), ty(f)))
                }
                Seg::Z => d.push('Z'),
            }
        }
        let fill = p.fill.map(hex).unwrap_or_else(|| "none".into());
        let fop = if p.fill.is_some() && p.fill_op < 0.999 { format!(" fill-opacity=\"{}\"", fmt(p.fill_op)) } else { String::new() };
        let stroke = match p.stroke {
            Some((c, lw)) => {
                let sop = if p.stroke_op < 0.999 { format!(" stroke-opacity=\"{}\"", fmt(p.stroke_op)) } else { String::new() };
                format!(" stroke=\"{}\" stroke-width=\"{}\"{sop}", hex(c), fmt(lw.max(0.3)))
            }
            None => String::new(),
        };
        let clip_attr = match p.clip {
            Some(c) => format!(" clip-path=\"url(#{})\"", clip_id_for(c, &mut clip_defs)),
            None => String::new(),
        };
        paths.push_str(&format!("<path d=\"{d}\" fill=\"{fill}\"{fop}{stroke}{clip_attr}/>"));
    }
    let paths = if clip_defs.is_empty() { paths } else { format!("<defs>{clip_defs}</defs>{paths}") };
    PlacedSvg { y_top: y1, y_bottom: y0, x_left: x0, x_right: x1, paths, w, h, page_w, labels: Vec::new(), plot }
}
