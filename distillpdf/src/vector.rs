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

use crate::function::Function;
use crate::geom::{Mat, Rect};
use crate::pdfobj::{deref, num, num_deref, sub_dict};
use crate::walker::{descend_form, overlay_xobjects, page_resource_chain, Descend, PaintSeq, ScopePolicy, XMap};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashMap;
use std::rc::Rc;

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

/// `n` colour components as RGB, by the component count alone — PDF's own device dispatch
/// (1 gray, 3 rgb, 4 cmyk). The one place that mapping lives; both a bare `scn` operand
/// list and a tint transform's output go through it.
fn from_components(n: usize, v: &[f32]) -> Option<[u8; 3]> {
    match n {
        1 if !v.is_empty() => Some(gray(v[0])),
        3 if v.len() >= 3 => Some(rgb(v[0], v[1], v[2])),
        4 if v.len() >= 4 => Some(cmyk(v[0], v[1], v[2], v[3])),
        _ => None,
    }
}

/// A colour space as the *vector* path needs it: how many operands an `scn` carries, and
/// what those operands mean.
///
/// Only the distinction `scn` acts on is modelled. Everything that is not a spot colour
/// collapses to [`PaintCs::Device`] with a component count, which is exactly the dispatch
/// `scn` did before any of this existed — so a well-formed device-colour stream is
/// bit-preserved whether or not it names its space.
enum PaintCs {
    /// A device-ish space (`DeviceRGB`, `ICCBased`, `CalGray`, `Indexed`, …). Carries no
    /// component count on purpose: for these the `scn` OPERAND count is authoritative and
    /// already equals it, so dispatching on the space instead would only change behaviour
    /// for a stream whose operands disagree with the space it named — i.e. for malformed
    /// input, in a direction nothing has evidence for.
    Device,
    /// `Separation` or `DeviceN`: `k` tints that mean nothing until they pass through the
    /// space's **tint transform** into an alternate space. `alt` is the alternate's
    /// component count (1/3/4); either being `None` is what sends `scn` to the ink-coverage
    /// fallback.
    Tint { k: usize, tint: Option<Function>, alt: Option<usize> },
    /// `Pattern`: `scn` names a pattern, which carries no directly usable colour — the same
    /// `None` the component dispatch produced for a trailing name.
    Pattern,
}

/// A `DeviceN` with more colorants than this is not a colour space we will serve.
const MAX_COLORANTS: usize = 32;

/// Parse one colour-space object. `None` means "not a space this path models" — the caller
/// then leaves the active space unset, which is precisely today's behaviour.
fn parse_cs(doc: &Document, res: &Dictionary, o: &Object, depth: u32) -> Option<PaintCs> {
    if depth > crate::raster::MAX_CS_DEPTH {
        return None;
    }
    // `resolve_cs` follows the reference AND the `/Resources`-`/ColorSpace` name lookup that
    // makes `/CS0` mean anything (`raster.rs` owns that reader; there is one copy of it).
    let resolved = crate::raster::resolve_cs(doc, res, o, 0)?;
    if let Object::Name(n) = resolved {
        if n.as_slice() == b"Pattern" {
            return Some(PaintCs::Pattern);
        }
    }
    if let Object::Array(a) = resolved {
        let head = deref(doc, a.first()?)?.as_name().ok()?;
        match head {
            b"Separation" | b"DeviceN" => {
                // `/Separation` is one colorant by definition; `/DeviceN`'s count is the
                // length of its names array (§8.6.6.4/§8.6.6.5).
                let k = if head == b"Separation" { 1 } else { deref(doc, a.get(1)?)?.as_array().ok()?.len() };
                if k == 0 || k > MAX_COLORANTS {
                    return None;
                }
                // The alternate space reduces to a component count — and an `/Indexed`
                // alternate is illegal, so it degrades rather than being read as gray.
                let alt = crate::raster::cs_model(doc, res, a.get(2)?, depth + 1).and_then(|c| match c {
                    // An `/Indexed` or spot alternate is illegal (§8.6.6.4), so it degrades
                    // rather than being read as gray or as another space's tint count.
                    crate::raster::Cs::Indexed { .. } | crate::raster::Cs::Tint { .. } => None,
                    other => Some(other.components()),
                });
                // A transform whose output arity disagrees with the alternate space it
                // feeds is not a transform we trust — dropping it here sends `scn` to the
                // coverage fallback instead of to a confidently wrong colour.
                let tint = a
                    .get(3)
                    .and_then(|f| Function::parse(doc, f))
                    .filter(|f| !matches!((f.n_outputs(), alt), (Some(n), Some(k)) if n != k));
                return Some(PaintCs::Tint { k, tint, alt });
            }
            b"Pattern" => return Some(PaintCs::Pattern),
            _ => {}
        }
    }
    crate::raster::cs_model(doc, res, o, depth).map(|_| PaintCs::Device)
}

/// The colour spaces one resource dictionary defines, by name — the `/ColorSpace` half of
/// what `cs`/`CS` resolve against, folded over the page's resource chain exactly as the
/// `/ExtGState` map is.
fn colorspaces_of(doc: &Document, resources: &Dictionary) -> HashMap<Vec<u8>, Rc<PaintCs>> {
    let mut map = HashMap::new();
    if let Some(csd) = sub_dict(doc, resources, b"ColorSpace") {
        for (name, val) in csd.iter() {
            if let Some(cs) = parse_cs(doc, resources, val, 0) {
                map.insert(name.clone(), Rc::new(cs));
            }
        }
    }
    map
}

/// A `cs`/`CS` operand resolved to a space: the four names that mean a space in themselves,
/// then the page's `/ColorSpace` resources.
fn cs_operand(csmap: &HashMap<Vec<u8>, Rc<PaintCs>>, name: &[u8]) -> Option<Rc<PaintCs>> {
    match name {
        b"DeviceGray" | b"CalGray" | b"G" | b"DeviceRGB" | b"CalRGB" | b"RGB" | b"DeviceCMYK" | b"CMYK" => Some(Rc::new(PaintCs::Device)),
        b"Pattern" => Some(Rc::new(PaintCs::Pattern)),
        other => csmap.get(other).cloned(),
    }
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
    seq: PaintSeq, // paint position in the content TREE — preserved for correct z-order
    // Active clip rect (page space) when this path was painted, if it actually crops it.
    // Rendered as an SVG <clipPath> so the visible ink matches the PDF (no overshoot).
    clip: Option<(f32, f32, f32, f32)>,
}

/// Graphics state carried through the walk and the q/Q stack.
///
/// `Clone`, not `Copy`: the active colour spaces are shared handles (a `DeviceN` transform
/// is a whole parsed function, and `q`/`Q` copies the state on every nesting level).
#[derive(Clone)]
struct GState {
    ctm: Mat,
    fill: [u8; 3],
    stroke: [u8; 3],
    lw: f32,
    fill_a: f32,   // ExtGState `ca` — fill alpha
    stroke_a: f32, // ExtGState `CA` — stroke alpha
    // Active clipping rectangle in PAGE space (x0, y0, x1, y1), the intersection of every
    // `W`/`W*` clip seen so far on the q/Q stack. `None` = unclipped (page bounds). A plot
    // clips its reference curves to the axes box; honouring it crops the curve overshoot.
    clip: Option<(f32, f32, f32, f32)>,
    // Colour spaces selected by `cs`/`CS`. `None` = none named, in which case `scn` falls
    // back to the operand-count dispatch — exactly what the whole walk did before.
    // NOTE the spec's "selecting a space resets the colour to its initial value" is
    // deliberately NOT implemented: nothing depended on it, and doing it here would repaint
    // device-colour streams that are correct today.
    fill_cs: Option<Rc<PaintCs>>,
    stroke_cs: Option<Rc<PaintCs>>,
}
impl GState {
    fn new(ctm: Mat, fill: [u8; 3], stroke: [u8; 3], lw: f32, fill_a: f32, stroke_a: f32) -> GState {
        GState { ctm, fill, stroke, lw, fill_a, stroke_a, clip: None, fill_cs: None, stroke_cs: None }
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
    /// `<clipPath>` definitions the paths reference. Emitted before any ink; renders nothing
    /// itself, so its position among the ink is free.
    defs: String,
    /// The `<path>` elements in local coords (origin `(x_left, y_top)`, y down), each kept
    /// WITH the address of the operation that painted it. Held apart rather than
    /// pre-concatenated because [`PlacedSvg::composite_svg`] has to slot raster `<image>`
    /// elements between them at their own paint positions; the vector-only renderers just
    /// concatenate in order ([`PlacedSvg::ink`]).
    paths: Vec<(PaintSeq, String)>,
    w: f32, // vector-ink content extent
    h: f32,
    page_w: f32, // page width — figure renders at its page-width share
    labels: Vec<Label>,
    // Local bbox of the figure's opaque background rect (a plot's plot-area), if any. When
    // present the viewBox is bounded to it (plus labels) so path ink overshooting the plot
    // — reference curves the PDF clips to the axes box — is cropped by the SVG viewport
    // instead of trailing far past the figure.
    plot: Option<(f32, f32, f32, f32)>,
    /// The page's `/Rotate` (0/90/180/270). The bbox above stays in **page space** — every
    /// cross-subsystem comparison in `html.rs` (captions, raster containment, reading order)
    /// is page-space and must not move — while the local geometry below is in **display
    /// orientation**, so the emitted `<svg>` reads the way a viewer shows the page.
    rot: i32,
}

/// Page space (y up) → figure-local SVG space (y down) for a figure whose page-space bbox is
/// `(x0, y0, x1, y1)`, honouring the page's `/Rotate`.
///
/// `/Rotate` turns the page CLOCKWISE for display, so this is the unrotated local mapping
/// (`x - x0`, `y1 - y`) with the resulting `w × h` image turned clockwise by `rot`: a point
/// at unrotated local `(u, v)` lands at `(h - v, u)` for 90°, `(w - u, h - v)` for 180° and
/// `(v, w - u)` for 270°. Substituting `u`/`v` gives the closed forms below, which is why no
/// intermediate is computed: `rot == 0` is *literally* the expression this replaced, so an
/// upright page's output is byte-identical, not merely equal.
fn to_local(rot: i32, x0: f32, y0: f32, x1: f32, y1: f32, x: f32, y: f32) -> (f32, f32) {
    match rot {
        90 => (y - y0, x - x0),
        180 => (x1 - x, y - y0),
        270 => (y1 - y, x1 - x),
        _ => (x - x0, y1 - y),
    }
}

/// Local extents of a figure whose page-space extents are `w × h` — transposed by a
/// quarter turn.
fn local_extent(rot: i32, w: f32, h: f32) -> (f32, f32) {
    if rot % 180 == 0 {
        (w, h)
    } else {
        (h, w)
    }
}

// A label whose centre is within this margin (pt) of the vector-ink bbox is taken
// to belong to the figure (form text sits just outside the boxes it annotates).
const LABEL_MARGIN: f32 = 24.0;

impl PlacedSvg {
    /// Attach form-internal text spans that belong to this figure, mapping each
    /// into local SVG coords. A span is claimed when its centre lies within the
    /// bbox expanded by [`LABEL_MARGIN`].
    ///
    /// The *claim* stays in page space — spans arrive in page space and so does the bbox, so
    /// which figure owns a label is decided exactly as it was before `/Rotate` existed here.
    /// Only the label's **local placement** is turned into display orientation.
    fn attach(&mut self, spans: &[LabelSpan]) {
        for s in spans {
            let cx = s.x + s.width * 0.5;
            let cy = s.y + s.size * 0.5;
            if cx >= self.x_left - LABEL_MARGIN
                && cx <= self.x_right + LABEL_MARGIN
                && cy >= self.y_bottom - LABEL_MARGIN
                && cy <= self.y_top + LABEL_MARGIN
            {
                let (lx, ly) = self.to_local(s.x, s.y);
                self.labels.push(Label {
                    lx,
                    ly,
                    size: s.size,
                    w: s.width,
                    text: s.text.clone(),
                    bold: s.bold,
                    italic: s.italic,
                    // COMPOSE, never overwrite: a 90° y-axis title on a `/Rotate 90` page is
                    // upright on screen, and a label already upright in page space reads
                    // sideways there. `angle` is CCW-positive (`text.rs` takes `atan2(b, a)`)
                    // while `/Rotate` turns the page clockwise, so the page's turn SUBTRACTS.
                    angle: self.rot_label_angle(s.angle),
                });
            }
        }
    }

    /// This figure's page-space → local-SVG mapping (see [`to_local`]).
    fn to_local(&self, x: f32, y: f32) -> (f32, f32) {
        to_local(self.rot, self.x_left, self.y_bottom, self.x_right, self.y_top, x, y)
    }

    /// A page-space baseline angle in this figure's display orientation. Exactly the input
    /// for an upright page, so no upright figure's `<text>` moves by a rounding step.
    fn rot_label_angle(&self, angle: f32) -> f32 {
        if self.rot == 0 {
            angle
        } else {
            angle - (self.rot as f32).to_radians()
        }
    }

    /// The SVG `transform` matrix that places one raster's unit square in this figure's local
    /// coords, or `None` when the plain `x/y/width/height` rect form says the same thing.
    ///
    /// A PDF image's unit square has `(0,0)` bottom-left with its FIRST pixel row at the top
    /// (`v = 1`), so SVG image space `(su, sv)` (y down, top-left origin) maps as `u = su`,
    /// `v = 1 - sv`. Mapping that through the page CTM and then this figure's page→local
    /// turn gives the matrix below.
    ///
    /// Two cases need it: a rotated *placement* (the pre-existing one, whose closed form is
    /// kept verbatim so an upright page's output is byte-identical), and a rotated *page* —
    /// where the pixels turn with the page even though the placement rect stays axis-aligned,
    /// so an axis-aligned image needs a matrix it never needed before.
    fn rot_image_matrix(&self, r: &Raster<'_>) -> Option<[f32; 6]> {
        if self.rot == 0 {
            // Upright: exactly the expressions this replaced (`matrix(a, -b, -c, d,
            // c+e-x_left, y_top-d-f)`), association included.
            let [a, b, c, d, e, f] = r.ctm?;
            return Some([a, -b, -c, d, c + e - self.x_left, self.y_top - d - f]);
        }
        // A rotated page. Without a placement matrix the image's own is the implicit
        // `[w 0 0 h x0 y0]` that stretches its unit square over the page-space rect.
        let (ix0, ix1, iy0, iy1) = r.rect;
        let [a, b, c, d, e, f] = r.ctm.unwrap_or([ix1 - ix0, 0.0, 0.0, iy1 - iy0, ix0, iy0]);
        // Page-space image of the three unit-square points SVG's matrix is defined by.
        let origin = (c + e, d + f); // (su,sv) = (0,0) -> (u,v) = (0,1)
        let du = (a + c + e, b + d + f); // (1,0) -> (1,1)
        let dv = (e, f); // (0,1) -> (0,0)
        let (ox, oy) = self.to_local(origin.0, origin.1);
        let (ux, uy) = self.to_local(du.0, du.1);
        let (vx, vy) = self.to_local(dv.0, dv.1);
        Some([ux - ox, uy - oy, vx - ox, vy - oy, ox, oy])
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
            self.ink(),
            texts
        )
    }

    /// This figure's vector ink as SVG: the clip definitions followed by every `<path>` in
    /// paint order. What every renderer here except [`PlacedSvg::composite_svg`] wants —
    /// that one has rasters to slot in between the paths.
    fn ink(&self) -> String {
        let mut out = self.defs.clone();
        for (_, p) in &self.paths {
            out.push_str(p);
        }
        out
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
    /// The one renderer that stays in PAGE orientation. `html.rs` positions this box with
    /// percentages of the raster's page-space rect, over an `<img>` the raster path emits
    /// **unturned** — so on a `/Rotate` page the overlay must register with that unturned
    /// image, not with the display. `un_rotate` maps the display-oriented ink back; for an
    /// upright page it is `None` and the output is byte-identical.
    pub fn overlay_svg(&self, style: &str) -> String {
        const PAD: f32 = 1.0;
        // Page-space extents: the local ones transposed back by the same quarter turn.
        let (pw, ph) = local_extent(self.rot, self.w, self.h);
        let (open, close) = match self.un_rotate() {
            Some(m) => (format!("<g transform=\"matrix({})\">", m), "</g>"),
            None => (String::new(), ""),
        };
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\" \
             preserveAspectRatio=\"none\" style=\"{}\" font-family=\"sans-serif\" fill=\"#000\">{}{}{}{}</svg>",
            fmt(-PAD),
            fmt(-PAD),
            fmt(pw + 2.0 * PAD),
            fmt(ph + 2.0 * PAD),
            style,
            open,
            self.ink(),
            self.label_texts(),
            close
        )
    }

    /// SVG matrix mapping this figure's display-oriented local coords back to page-oriented
    /// local coords, or `None` for an upright page. The inverse of [`to_local`]'s quarter
    /// turn, expressed about the local box: with local extents `w × h`, `90°` sends
    /// `(lx, ly)` to `(ly, w - lx)`, `180°` to `(w - lx, h - ly)` and `270°` to `(h - ly, lx)`.
    fn un_rotate(&self) -> Option<String> {
        let m = match self.rot {
            90 => [0.0, -1.0, 1.0, 0.0, 0.0, self.w],
            180 => [-1.0, 0.0, 0.0, -1.0, self.w, self.h],
            270 => [0.0, 1.0, -1.0, 0.0, self.h, 0.0],
            _ => return None,
        };
        Some(m.iter().map(|v| fmt(*v)).collect::<Vec<_>>().join(" "))
    }

    /// Render ONE self-contained `<svg>` that composites one or more raster images WITH
    /// this figure's vector ink and labels — all in the figure's local user space, so they
    /// register pixel-for-pixel. The viewBox is the union of every raster rect, the vector
    /// ink, and every label, so nothing is clipped (axis labels in the margins included).
    /// Works in BOTH directions: a vector OVER a base raster (a location map: vector lines/
    /// labels over a base photo) and rasters INSIDE a larger vector frame (a plot whose
    /// data points are a raster within the axes/legend).
    ///
    /// **Rasters and ink are interleaved by PAINT ORDER** ([`PaintSeq`]), not grouped by
    /// kind. This function used to emit every raster first and all the ink after it, which
    /// is right only when the stream painted them that way. Real figures interleave: a
    /// panel, a photo dropped into it, then the annotation on top — and painting that photo
    /// first put it *behind* the panel, i.e. deleted it from the output. (The reverse also
    /// exists, which is why `build_svg` drops a near-white full-figure background: with paint
    /// order honoured, a background that genuinely precedes the raster now stays behind it on
    /// its own merits.) Sorting is stable, so equal addresses — impossible between two paints,
    /// since an address IS an operation — keep the caller's order.
    pub fn composite_svg(&self, rasters: &[Raster<'_>]) -> String {
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
        // Raster rects in local coords (origin (x_left, y_top), y DOWN). The viewBox grows by
        // the axis-aligned placement bbox in both cases (a rotated image's bbox IS that box).
        let mut content: Vec<(&PaintSeq, String)> = Vec::with_capacity(rasters.len() + self.paths.len());
        for r in rasters {
            let (ix0, ix1, iy0, iy1) = r.rect;
            // The raster arrives in PAGE space (that is the space `html.rs` pairs it with this
            // figure in); a quarter turn maps its rect to a rect, so the two opposite corners
            // re-ordered give the local box exactly. Upright, this is `ix0 - x_left` etc.
            let (ax, ay) = self.to_local(ix0, iy1);
            let (bx, by) = self.to_local(ix1, iy0);
            let img_lx = ax.min(bx);
            let img_ly = ay.min(by);
            // Extents are the page-space spans transposed, NOT a difference of mapped corners
            // — the latter reassociates the subtraction and moves an upright raster by a
            // rounding step (same reason as `clip_id_for`).
            let (img_lw, img_lh) = local_extent(self.rot, (ix1 - ix0).max(0.1), (iy1 - iy0).max(0.1));
            min_x = min_x.min(img_lx);
            min_y = min_y.min(img_ly);
            max_x = max_x.max(img_lx + img_lw);
            max_y = max_y.max(img_ly + img_lh);
            // On a ROTATED page the image's PIXELS turn with the page even when its placement
            // rect is axis-aligned, so the plain-rect form below cannot express it — every
            // raster on such a page goes through the matrix path.
            let el = match self.rot_image_matrix(r) {
                Some([a, b, c, d, e, f]) => format!(
                    "<image href=\"{}\" x=\"0\" y=\"0\" width=\"1\" height=\"1\" preserveAspectRatio=\"none\" transform=\"matrix({} {} {} {} {} {})\"/>",
                    r.href,
                    fmt(a),
                    fmt(b),
                    fmt(c),
                    fmt(d),
                    fmt(e),
                    fmt(f),
                ),
                None => format!(
                    "<image href=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\"/>",
                    r.href,
                    fmt(img_lx),
                    fmt(img_ly),
                    fmt(img_lw),
                    fmt(img_lh)
                ),
            };
            content.push((r.seq, el));
        }
        for (seq, el) in &self.paths {
            content.push((seq, el.clone()));
        }
        content.sort_by(|a, b| a.0.cmp(b.0));
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
        // Clip definitions (render nothing), then rasters and vector ink interleaved in the
        // order the page painted them, then the text labels on top. Labels stay last: they
        // are the figure's own annotation and are the one thing that must never be occluded.
        let mut body = self.defs.clone();
        for (_, el) in &content {
            body.push_str(el);
        }
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\" \
             style=\"display:block;width:{}%;height:auto;margin:0 auto\" \
             font-family=\"sans-serif\" fill=\"#000\">{}{}</svg>",
            fmt(min_x),
            fmt(min_y),
            fmt(vbw),
            fmt(vbh),
            fmt(pct),
            body,
            self.label_texts()
        )
    }
}

/// One raster to composite into a figure's `<svg>` ([`PlacedSvg::composite_svg`]): the
/// (deferred) href, its PDF page rect `(x_left, x_right, y_bottom, y_top)` in y-up page
/// space, an optional placement matrix `[a,b,c,d,e,f]` when the image is ROTATED — then
/// the pixels are mapped through it instead of stretched into the rect — and where the
/// page painted it, which is what decides whether it lands above or below each path.
pub struct Raster<'a> {
    pub href: &'a str,
    pub rect: (f32, f32, f32, f32),
    pub ctm: Option<[f32; 6]>,
    pub seq: &'a PaintSeq,
}

// Figure filter: a real vector figure is a cluster of ink at least this big with
// at least this many painted paths (so single rules / underlines / a lone box
// don't qualify).
const MIN_W: f32 = 72.0;
const MIN_H: f32 = 54.0;
const MIN_PATHS: usize = 6;
// A relaxed "weak" bar: a cluster that fails the strong bar but clears these is a CANDIDATE
// figure, kept aside and only promoted in html.rs when a figure caption sits right next to it
// (a small diagram — a few ellipse curves, a TikZ sketch — that the strong bar drops). The
// weak bar still rejects a single rule / underline (needs ≥2 paths and a real 2-D extent).
const WEAK_MIN_W: f32 = 36.0;
const WEAK_MIN_H: f32 = 24.0;
const WEAK_MIN_PATHS: usize = 2;
const BAND_GAP: f32 = 24.0; // vertical gap that separates two figures
// Operation budget for one page's vector walk. Not a parse budget — the content stream is
// already decoded before this applies — so it only bounds the interpret+cluster pass, which
// measures in the low milliseconds per 100k ops. 60_000 was far tighter than that cost
// justified: the densest page in the local corpus is ~500k ops and the next-densest DOCUMENT
// peaks at ~34k, so the old value cut real figures (a USGS cover map) out of the middle of the
// distribution. A page over budget is truncated, never dropped (see `positioned_vectors_capped`).
const MAX_OPS: usize = 600_000;

/// ExtGState name -> (fill alpha `ca`, stroke alpha `CA`) where defined.
fn extgstates_of(doc: &Document, resources: &Dictionary) -> HashMap<Vec<u8>, (Option<f32>, Option<f32>)> {
    let mut map = HashMap::new();
    if let Some(eg) = resources.get(b"ExtGState").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        for (name, val) in eg.iter() {
            if let Some(d) = deref(doc, val).and_then(|o| o.as_dict().ok()) {
                // Dictionary VALUES, so `num_deref` — a writer that hoists a shared alpha into
                // an indirect object is legal, and the direct-only `num` read it as 0.0, i.e.
                // fully transparent, which silently deletes every paint made under this state.
                let ca = d.get(b"ca").ok().map(|o| num_deref(doc, o));
                let big = d.get(b"CA").ok().map(|o| num_deref(doc, o));
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
// One more argument than clippy's default bar, for the same reason `walk` (below) and
// `text::decode_spans` carry the `#[allow]`: this is an interpreter's flat state, and
// bundling it into a struct would hide which parts a paint operator supplies fresh
// (colour, alpha, clip) from the address/output it merely threads through.
#[allow(clippy::too_many_arguments)]
fn finish(cur: &mut Vec<Seg>, fill: Option<[u8; 3]>, stroke: Option<([u8; 3], f32)>, fill_op: f32, stroke_op: f32, clip: Option<(f32, f32, f32, f32)>, seq: PaintSeq, out: &mut Vec<Painted>) {
    if cur.is_empty() {
        return;
    }
    if fill.is_none() && stroke.is_none() {
        cur.clear();
        return;
    }
    // `path_bbox` is `None` for exactly the point-free path this used to detect as `x1 < x0`.
    let (mut x0, mut y0, mut x1, mut y1) = match path_bbox(cur) {
        Some(bb) => bb,
        None => {
            cur.clear();
            return;
        }
    };
    // Drop paths whose extent is implausibly large. A real figure element never exceeds
    // page size (~800 pt); a span of thousands+ means a coordinate was left in the wrong
    // space (page coords leaking into a figure-local frame, a mis-applied matrix), which
    // otherwise draws a line shooting off the figure or collapses its viewBox.
    const MAX_EXTENT: f32 = 2000.0;
    if (x1 - x0).max(y1 - y0) > MAX_EXTENT {
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
            let n = Rect::new(x0, y0, x1, y1).intersect(Rect::new(cx0, cy0, cx1, cy1));
            if n.x1 <= n.x0 || n.y1 <= n.y0 {
                cur.clear(); // path lies entirely outside its clip — invisible
                return;
            }
            x0 = n.x0;
            y0 = n.y0;
            x1 = n.x1;
            y1 = n.y1;
            crop = clip;
        }
    }
    out.push(Painted { segs: std::mem::take(cur), fill, stroke, fill_op, stroke_op, x0, y0, x1, y1, seq, clip: crop });
}

/// Page-space bounding box of a path under construction (a clip path is just a path
/// followed by `W`/`W*`); `None` if it has no points.
fn path_bbox(cur: &[Seg]) -> Option<(f32, f32, f32, f32)> {
    let mut bb = Rect::EMPTY;
    for s in cur {
        let pts: &[(f32, f32)] = match s {
            Seg::M(x, y) | Seg::L(x, y) => &[(*x, *y)],
            Seg::C(a, b, c, d, e, f) => &[(*a, *b), (*c, *d), (*e, *f)],
            Seg::Z => &[],
        };
        for &(x, y) in pts {
            bb.include(x, y);
        }
    }
    bb.is_valid().then_some((bb.x0, bb.y0, bb.x1, bb.y1))
}

/// Vector figures on a page, top-to-bottom.
/// Returns `(strong, weak)` placed vector figures. STRONG are emitted unconditionally; WEAK
/// are sub-threshold candidates html.rs promotes only when a figure caption anchors to one.
pub fn positioned_vectors(doc: &Document, page_id: ObjectId) -> (Vec<PlacedSvg>, Vec<PlacedSvg>) {
    positioned_vectors_capped(doc, page_id, MAX_OPS)
}

/// [`positioned_vectors`] with an explicit operation budget (the public entry point passes
/// [`MAX_OPS`]). Exposed internally so the truncation behaviour is unit-testable with a tiny
/// cap instead of a half-million-operation fixture.
fn positioned_vectors_capped(doc: &Document, page_id: ObjectId, cap: usize) -> (Vec<PlacedSvg>, Vec<PlacedSvg>) {
    // A page with no `/Resources` anywhere in its tree used to return here, empty. But the
    // path operators — `m`/`l`/`c`/`re`/`v`/`y`/`h` and the `f`/`S`/`B` that paint them —
    // name no resource at all: `/Resources` is only needed to resolve an `/ExtGState` alpha
    // or a form `Do`, and a page can legally draw its whole figure without either. The
    // guard therefore deleted every direct path a resource-less page drew, and the SVG with
    // it. Both maps below are simply empty for such a page, `Do` resolves to nothing, and
    // the graphics state starts at the spec defaults (opaque, black) — which is exactly
    // what a page with no `/ExtGState` is entitled to.
    let chain = page_resource_chain(doc, page_id);
    let content = match doc.get_and_decode_page_content(page_id) {
        Ok(c) => c,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    // Over-budget pages DEGRADE, they do not vanish: interpret the first `cap` operations and
    // keep whatever was painted by then. Returning empty here made a dense page look like a page
    // with no figures at all (a whole cover map disappeared), which is strictly worse than a
    // partial one — the ops are already parsed by this point, so the cap only bounds the walk.
    let ops = &content.operations[..content.operations.len().min(cap)];
    // Both maps are folded over the WHOLE resource chain, outermost first, so the page's
    // own dictionary still shadows an ancestor's entry of the same name while a name only
    // an ancestor defines finally resolves (see `walker::page_resource_chain`).
    let mut xmap = XMap::new();
    let mut egmap: HashMap<Vec<u8>, (Option<f32>, Option<f32>)> = HashMap::new();
    let mut csmap: HashMap<Vec<u8>, Rc<PaintCs>> = HashMap::new();
    for res in &chain {
        overlay_xobjects(doc, res, &mut xmap);
        egmap.extend(extgstates_of(doc, res));
        csmap.extend(colorspaces_of(doc, res));
    }
    let mut painted = Vec::new();
    let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
    walk(doc, ops, &xmap, &egmap, &csmap, GState::new(Mat::ID, [0; 3], [0; 3], 1.0, 1.0, 1.0), &mut painted, 0, &mut budget, &[]);
    // Paint order is stamped by the walk itself (`PaintSeq`, the operation's address in the
    // content tree) rather than re-derived from this vector's order here. The two are the
    // same ordering, but only the address is comparable with `img::positioned_images`'s
    // rasters, which is what lets a composited figure interleave the two.
    // The page's `/Rotate` reaches only the FIGURE geometry (`build_svg`'s local mapping and
    // the label/raster plumbing that shares it). Everything upstream of this line — the walk,
    // the clip crop, `cluster_figures`'s banding and its size bars — deliberately stays in
    // page space: those thresholds (`BAND_GAP`, `MIN_W`/`MIN_H`, the 400×600 full-page-fill
    // filter) are orientation-sensitive, so folding the turn into the base CTM would silently
    // change which clusters become figures on a rotated page for reasons unrelated to this
    // defect. Turning at the page→SVG-local boundary fixes the orientation and leaves every
    // selection rule — and every page-space comparison `html.rs` makes against these boxes —
    // exactly as it was.
    let rot = crate::pdfobj::page_rotation(doc, page_id);
    let page_w = page_width(doc, page_id, rot);
    let (strong, weak) = cluster_figures(painted);
    let build = |cs: Vec<Vec<Painted>>| cs.iter().map(|c| build_svg(c, page_w, rot)).collect();
    (build(strong), build(weak))
}

/// **Displayed** page width from the page box (used to size each figure as a share of the
/// page).
///
/// The box itself — `/MediaBox` then `/CropBox`, inherited up `/Parent`, extents resolved
/// through indirect references — comes from [`crate::pdfobj::page_box`]. A width of zero or
/// one point is not a page, so it degrades to the letter default rather than scaling every
/// figure on the page by a garbage denominator.
///
/// A quarter-turn `/Rotate` makes the page's HEIGHT its displayed width. The figure's own
/// extent is likewise measured in display orientation, so both sides of the share must be —
/// otherwise a landscape table is sized against a portrait denominator and renders at half
/// the width it occupies.
fn page_width(doc: &Document, page_id: ObjectId, rot: i32) -> f32 {
    crate::pdfobj::page_box(doc, page_id)
        .map(|b| if rot % 180 == 0 { (b[2] - b[0]).abs() } else { (b[3] - b[1]).abs() })
        .filter(|w| *w > 1.0)
        .unwrap_or(crate::pdfobj::DEFAULT_PAGE_PTS.0)
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
    xmap: &XMap,
    egmap: &HashMap<Vec<u8>, (Option<f32>, Option<f32>)>,
    csmap: &HashMap<Vec<u8>, Rc<PaintCs>>,
    base: GState,
    out: &mut Vec<Painted>,
    depth: u32,
    budget: &mut crate::WalkBudget,
    // Address of the stream being walked (empty for the page's own content) — each
    // operation's index is appended to it to stamp a path's `PaintSeq`.
    here: &[u32],
) {
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

    for (opi, op) in ops.iter().enumerate() {
        // Total-work budget (see `crate::WalkBudget`). `MAX_OPS` above truncates only the
        // page's TOP-LEVEL operator list; a self-referential form re-enters `walk` with a
        // fresh, tiny slice each time, so only a budget shared across all levels bounds it.
        // Out of budget → return what has been painted, exactly as the `MAX_OPS` truncation
        // does. A dense page degrades; it never comes back looking empty. The in-flight
        // subpath in `cur` is dropped rather than flushed: it has not reached its painting
        // operator, so painting it here would fabricate ink the page never showed.
        if !budget.spend(1) {
            return;
        }
        let o = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push(g.clone()),
            "Q" => {
                if let Some(s) = stack.pop() {
                    g = s;
                }
            }
            "cm" if o.len() >= 6 => {
                g.ctm = Mat { a: num(&o[0]), b: num(&o[1]), c: num(&o[2]), d: num(&o[3]), e: num(&o[4]), f: num(&o[5]) }.mul(g.ctm);
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
            // `cs`/`CS` name the space the following `scn`/`SCN` operands live in. Without
            // this arm the walk never knew, so a Separation tint was read as a grey level.
            "cs" => g.fill_cs = o.first().and_then(|x| x.as_name().ok()).and_then(|n| cs_operand(csmap, n)),
            "CS" => g.stroke_cs = o.first().and_then(|x| x.as_name().ok()).and_then(|n| cs_operand(csmap, n)),
            "sc" | "scn" => {
                if let Some(c) = scn_color(g.fill_cs.as_deref(), o) {
                    g.fill = c;
                }
            }
            "SC" | "SCN" => {
                if let Some(c) = scn_color(g.stroke_cs.as_deref(), o) {
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
                            Some(cl) => {
                                let n = Rect::new(cl.0, cl.1, cl.2, cl.3).intersect(Rect::new(bb.0, bb.1, bb.2, bb.3));
                                (n.x0, n.y0, n.x1, n.y1)
                            }
                            None => bb,
                        });
                    }
                    pending_clip = false;
                }
                match op.operator.as_str() {
                    "f" | "F" | "f*" => finish(&mut cur, eff_fill(&g), None, g.fill_a, g.stroke_a, g.clip, PaintSeq::at(here, opi), out),
                    "S" | "s" => finish(&mut cur, None, eff_stroke(&g), g.fill_a, g.stroke_a, g.clip, PaintSeq::at(here, opi), out),
                    "B" | "B*" | "b" | "b*" => finish(&mut cur, eff_fill(&g), eff_stroke(&g), g.fill_a, g.stroke_a, g.clip, PaintSeq::at(here, opi), out),
                    _ => cur.clear(), // "n": clip-only path → no ink
                }
            }
            "Do" => {
                // Images are `crate::img`'s business; only forms carry path ink. The
                // descent inherits the page's scope (`OverlayParent`) so a form can paint
                // through an ExtGState or XObject the page defines.
                let Some((_, stream)) = crate::walker::xobject_at(doc, xmap, o) else {
                    continue;
                };
                let f = match descend_form(doc, stream, xmap, ScopePolicy::OverlayParent, depth, budget, egmap.len()) {
                    Descend::Into(f) => f,
                    Descend::Skip => continue,
                    Descend::Halt => return,
                };
                // The ExtGState and ColorSpace halves of the scope are this walker's own
                // interpretation, so they are overlaid here rather than in the shared
                // descent. A form routinely defines the spot colour it paints with.
                let mut child_eg = egmap.clone();
                let mut child_cs = csmap.clone();
                if let Some(fr) = &f.scope.resources {
                    for (k, v) in extgstates_of(doc, fr) {
                        child_eg.insert(k, v);
                    }
                    for (k, v) in colorspaces_of(doc, fr) {
                        child_cs.insert(k, v);
                    }
                }
                let mut sub = g.clone();
                sub.ctm = f.matrix.mul(g.ctm);
                walk(doc, &f.ops, &f.scope.xobjects, &child_eg, &child_cs, sub, out, depth + 1, budget, PaintSeq::at(here, opi).as_slice());
            }
            _ => {}
        }
    }
}

/// `sc`/`scn` operands → colour, **in the colour space `cs`/`CS` selected**.
///
/// Without a space the operands are dispatched by count alone (1 gray, 3 rgb, 4 cmyk), and
/// a trailing name (a pattern) yields no usable colour — the whole of what this did before.
///
/// With a `Separation` / `DeviceN` space the operands are *tints*, not colour: `.1 scn`
/// means "10% of this colorant", and reading it as the grey level 0.1 paints a pale spot
/// colour near-BLACK. The tint transform is what turns it into a colour, so it is evaluated
/// into the alternate space.
///
/// **When the transform cannot be evaluated** (a Type 4 PostScript calculator, a malformed
/// or absent function, an alternate space we cannot reduce) the tint is read as **ink
/// coverage** instead: `t` of the colorant laid on white paper is luminance `1 - t`. That
/// degrades a pale tint to pale and a solid one to dark — the right *direction*, where the
/// old reading inverted it. For `DeviceN` the heaviest colorant sets the coverage.
fn scn_color(cs: Option<&PaintCs>, o: &[Object]) -> Option<[u8; 3]> {
    let nums: Vec<f32> = o.iter().take_while(|x| matches!(x, Object::Integer(_) | Object::Real(_))).map(num).collect();
    match cs {
        Some(PaintCs::Tint { k, tint, alt }) if nums.len() == *k => {
            if let (Some(f), Some(n)) = (tint, alt) {
                if let Some(c) = f.eval(&nums).and_then(|out| from_components(*n, &out)) {
                    return Some(c);
                }
            }
            let ink = nums.iter().copied().fold(0.0f32, f32::max).clamp(0.0, 1.0);
            Some(gray(1.0 - ink))
        }
        Some(PaintCs::Pattern) => None,
        // Device spaces, an arity that does not match the named space, or no space at all:
        // the operand-count dispatch, bit-for-bit as before.
        _ => from_components(nums.len(), &nums),
    }
}

/// Group painted paths into vertically-contiguous clusters and split them into
/// `(strong, weak)`: STRONG clusters are real figures emitted unconditionally; WEAK
/// clusters clear only the relaxed bar and are emitted by html.rs solely when a figure
/// caption anchors to one (a small diagram the strong bar would drop). Clusters failing
/// even the weak bar (single rules, stray marks) are discarded.
fn cluster_figures(mut paths: Vec<Painted>) -> (Vec<Vec<Painted>>, Vec<Vec<Painted>>) {
    // Drop full-page background fills (a single huge rectangle) up front.
    paths.retain(|p| !(p.x1 - p.x0 > 400.0 && p.y1 - p.y0 > 600.0 && p.segs.len() <= 5));
    if paths.is_empty() {
        return (Vec::new(), Vec::new());
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
    let extent = |c: &[Painted]| {
        let bb = cluster_bbox(c);
        (bb.width(), bb.height())
    };
    let (mut strong, mut weak): (Vec<Vec<Painted>>, Vec<Vec<Painted>>) = (Vec::new(), Vec::new());
    for c in clusters {
        let (w, h) = extent(&c);
        if c.len() >= MIN_PATHS && w >= MIN_W && h >= MIN_H {
            strong.push(c);
        } else if c.len() >= WEAK_MIN_PATHS && w >= WEAK_MIN_W && h >= WEAK_MIN_H {
            weak.push(c);
        }
    }
    // Restore stream paint order within each cluster (banding sorted by y): a fill
    // drawn after an outline must paint on top of it, not be reordered by position.
    for c in strong.iter_mut().chain(weak.iter_mut()) {
        c.sort_by(|a, b| a.seq.cmp(&b.seq));
    }
    (strong, weak)
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

use crate::textutil::esc_text as esc;

/// Enclosing page-space box of a cluster of painted paths.
fn cluster_bbox(cluster: &[Painted]) -> Rect {
    cluster
        .iter()
        .fold(Rect::EMPTY, |acc, p| acc.union(Rect::new(p.x0, p.y0, p.x1, p.y1)))
}

/// Transcode one figure cluster into the path geometry of a [`PlacedSvg`]
/// (paths in stream order, y flipped). The `<svg>` wrapper + any text labels are
/// emitted later by [`PlacedSvg::svg`].
fn build_svg(cluster: &Vec<Painted>, page_w: f32, rot: i32) -> PlacedSvg {
    let Rect { x0, y0, x1, y1 } = cluster_bbox(cluster);
    let (w, h) = (x1 - x0, y1 - y0);
    // Local extents are the page-space ones transposed by a quarter turn.
    let (lw, lh) = local_extent(rot, w, h);
    // page space (y up) -> local SVG space (y down), turned by the page's `/Rotate` (see
    // `to_local`; the upright form is lx = x-x0, ly = y1-y). A stray point (one coordinate
    // left in the wrong space, surviving the per-path extent gate) is clamped to within one
    // figure-extent of the box, so it can never draw a huge line.
    let pt = |x: f32, y: f32| {
        let (lx, ly) = to_local(rot, x0, y0, x1, y1, x, y);
        (fmt(lx.clamp(-lw, 2.0 * lw)), fmt(ly.clamp(-lh, 2.0 * lh)))
    };
    // The local axis-aligned box of a page-space rect: a quarter turn maps a rect to a rect,
    // so mapping the two opposite corners and re-ordering is exact.
    let lrect = |rx0: f32, ry0: f32, rx1: f32, ry1: f32| {
        let (ax, ay) = to_local(rot, x0, y0, x1, y1, rx0, ry1);
        let (bx, by) = to_local(rot, x0, y0, x1, y1, rx1, ry0);
        (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by))
    };

    let area = (w * h).max(1.0);
    let mut plot: Option<(f32, f32, f32, f32)> = None;
    // SVG <clipPath> definitions for paths drawn under a PDF clip (a plot's reference curves
    // clipped to the axes box). The id is derived from the clip's own figure-LOCAL geometry
    // so that `same id ⟺ same <rect>`. This is what keeps clipping correct once the whole
    // document is assembled: ids must be globally unique, but every figure shares the same
    // page content origin, so an origin- or index-based id collides across figures — and the
    // doc-wide `dedup_ids` pass then renames the colliding `id=` WITHOUT touching the
    // `clip-path="url(#…)"` reference, silently breaking the clip. A geometry-keyed id avoids
    // that: distinct clips get distinct ids (no rename), and any genuinely identical clip
    // that does get renamed still resolves to a clipPath with an identical rect.
    let mut clip_defs = String::new();
    let mut clip_ids: Vec<(i32, i32, i32, i32)> = Vec::new();
    let mut clip_id_for = |c: (f32, f32, f32, f32), defs: &mut String| -> String {
        // page space -> figure-local (y flipped, turned by /Rotate): a clip rect (cx0,cy0,cx1,cy1).
        // The ORIGIN comes from the corner map; the EXTENTS are the page-space spans merely
        // transposed — taking them as a difference of two mapped corners instead would
        // reassociate the subtraction (`(c.2-x0)-(c.0-x0)` is not `c.2-c.0` in f32) and move
        // an upright figure's clip by a rounding step.
        let (cl0, cl1, _, _) = lrect(c.0, c.1, c.2, c.3);
        let (cw, ch) = local_extent(rot, (c.2 - c.0).max(0.0), (c.3 - c.1).max(0.0));
        let (lx, lw_) = (cl0, cw);
        let (ly, lh_) = (cl1, ch);
        let key = ((lx * 4.0) as i32, (ly * 4.0) as i32, (lw_ * 4.0) as i32, (lh_ * 4.0) as i32);
        let id = format!("clip_{}_{}_{}_{}", key.0, key.1, key.2, key.3);
        if !clip_ids.contains(&key) {
            defs.push_str(&format!("<clipPath id=\"{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/></clipPath>", id, fmt(lx), fmt(ly), fmt(lw_), fmt(lh_)));
            clip_ids.push(key);
        }
        id
    };
    let mut paths: Vec<(PaintSeq, String)> = Vec::new();
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
                    let (bx0, by0, bx1, by1) = lrect(p.x0, p.y0, p.x1, p.y1);
                    plot = Some(match plot {
                        Some(m) => {
                            let u = Rect::new(m.0, m.1, m.2, m.3).union(Rect::new(bx0, by0, bx1, by1));
                            (u.x0, u.y0, u.x1, u.y1)
                        }
                        None => (bx0, by0, bx1, by1),
                    });
                    continue;
                }
            }
        }
        let mut d = String::new();
        for s in &p.segs {
            match *s {
                Seg::M(x, y) => {
                    let (lx, ly) = pt(x, y);
                    d.push_str(&format!("M{lx} {ly}"))
                }
                Seg::L(x, y) => {
                    let (lx, ly) = pt(x, y);
                    d.push_str(&format!("L{lx} {ly}"))
                }
                Seg::C(a, b, c, dd, e, f) => {
                    let (p1x, p1y) = pt(a, b);
                    let (p2x, p2y) = pt(c, dd);
                    let (p3x, p3y) = pt(e, f);
                    d.push_str(&format!("C{p1x} {p1y} {p2x} {p2y} {p3x} {p3y}"))
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
        paths.push((p.seq.clone(), format!("<path d=\"{d}\" fill=\"{fill}\"{fop}{stroke}{clip_attr}/>")));
    }
    let defs = if clip_defs.is_empty() { String::new() } else { format!("<defs>{clip_defs}</defs>") };
    PlacedSvg { y_top: y1, y_bottom: y0, x_left: x0, x_right: x1, defs, paths, w: lw, h: lh, page_w, labels: Vec::new(), plot, rot }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load an adversarial fixture (`tests/gen_fixtures.py::gen_form_bomb`) and set up the
    /// exact state `positioned_vectors_capped` hands to [`walk`], so a test can drive the
    /// walker with its own budget.
    fn adversarial(name: &str) -> (Document, ObjectId) {
        let path = format!("{}/../tests/fixtures_pdf/adversarial/{name}", env!("CARGO_MANIFEST_DIR"));
        let doc = Document::load(&path).unwrap_or_else(|e| panic!("{name} fixture must load: {e}"));
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        (doc, page_id)
    }

    /// The page's own (nearest) resource dictionary — the last entry of the overlay chain.
    fn page_res(doc: &Document, page_id: ObjectId) -> Dictionary {
        page_resource_chain(doc, page_id).pop().expect("fixture page has resources")
    }

    fn walk_page(doc: &Document, page_id: ObjectId, budget: usize) -> Vec<Painted> {
        let content = doc.get_and_decode_page_content(page_id).expect("fixture page has content");
        let mut xmap = XMap::new();
        let mut egmap: HashMap<Vec<u8>, (Option<f32>, Option<f32>)> = HashMap::new();
        let mut csmap: HashMap<Vec<u8>, Rc<PaintCs>> = HashMap::new();
        for res in &page_resource_chain(doc, page_id) {
            overlay_xobjects(doc, res, &mut xmap);
            egmap.extend(extgstates_of(doc, res));
            csmap.extend(colorspaces_of(doc, res));
        }
        let mut painted = Vec::new();
        let mut budget = crate::WalkBudget::new(budget);
        walk(doc, &content.operations, &xmap, &egmap, &csmap, GState::new(Mat::ID, [0; 3], [0; 3], 1.0, 1.0, 1.0), &mut painted, 0, &mut budget, &[]);
        painted
    }

    #[test]
    fn a_self_referential_form_cannot_hang_the_vector_walk() {
        // `form_bomb.pdf`: form /X invokes /X twice, so the walk branches 2x per level.
        // `MAX_FORM_DEPTH` alone allowed ~2^40 descents — this call never returned.
        let (doc, page_id) = adversarial("form_bomb.pdf");
        let t = std::time::Instant::now();
        let painted = walk_page(&doc, page_id, crate::MAX_FORM_WORK);
        assert!(t.elapsed().as_secs() < 10, "form bomb ran for {:?} — the budget is not bounding it", t.elapsed());
        assert!(painted.is_empty(), "the bomb paints no ink, so nothing may be invented for it");
        // The full render entry point must be bounded too, not just the raw walker.
        let t = std::time::Instant::now();
        let _ = positioned_vectors(&doc, page_id);
        assert!(t.elapsed().as_secs() < 10, "positioned_vectors ran for {:?}", t.elapsed());
    }

    #[test]
    fn a_form_drawn_three_times_is_painted_three_times() {
        // The control, and the reason this fix is a BUDGET and not a visited set: one form
        // invoked at three offsets is three real paint sites. An `ObjectId` dedupe would
        // return 1 here and silently drop two thirds of the page's ink.
        let (doc, page_id) = adversarial("form_repeat.pdf");
        let painted = walk_page(&doc, page_id, crate::MAX_FORM_WORK);
        assert_eq!(painted.len(), 3, "a repeated form must paint once per invocation");
        // …and at three DISTINCT positions, so this cannot pass on three copies of one rect.
        let mut ys: Vec<i32> = painted.iter().map(|p| p.y0.round() as i32).collect();
        ys.sort_unstable();
        ys.dedup();
        assert_eq!(ys.len(), 3, "the three occurrences must land at three offsets, got {ys:?}");
    }

    #[test]
    fn an_exhausted_work_budget_degrades_a_repeated_form_instead_of_emptying_it() {
        // Degrade, don't vanish (the precedent `MAX_OPS` set): a walk that runs out mid-page
        // returns what it painted, never an empty page that reads as "nothing here".
        let (doc, page_id) = adversarial("form_repeat.pdf");
        let painted = walk_page(&doc, page_id, 700);
        assert!(!painted.is_empty(), "a tripped budget must not empty the page");
        assert!(painted.len() < 3, "the budget must really bite, got {} paths", painted.len());
    }

    /// The owned dense-vector fixture (`tests/gen_fixtures.py::gen_dense_vector`): a grid
    /// figure painted in the first ~54 operators, then ~300 scatter rects, then all text.
    fn dense_page() -> (Document, ObjectId) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/dense_vector.pdf");
        let doc = Document::load(path).expect("dense_vector.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        (doc, page_id)
    }

    #[test]
    fn page_width_inherits_media_box_from_the_page_tree() {
        // `/MediaBox` is inheritable, and the fixture states it only on the /Pages node.
        // Reading the page dict alone fell through to the 612pt letter guess, which sized
        // every figure on such a file as the wrong share of its page.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/inherited_mediabox.pdf");
        let doc = Document::load(path).expect("inherited_mediabox.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        // The page really does lack the key — otherwise this test proves nothing.
        let page = doc.get_object(page_id).unwrap().as_dict().unwrap();
        assert!(page.get(b"MediaBox").is_err() && page.get(b"CropBox").is_err());
        assert_eq!(page_width(&doc, page_id, 0), 842.0, "must inherit A4 landscape, not guess 612");
    }

    #[test]
    fn page_width_resolves_indirect_media_box_entries() {
        // `indirect_mediabox.pdf`: the inheritable /MediaBox on the /Pages node is written
        // `[0 0 9 0 R 10 0 R]` — legal for an array value, and what the direct-only `num`
        // reader turned into `[0 0 0 0]`. The zero-width box then failed the sanity filter
        // and the page silently became the guessed 612pt letter, so the 504pt grid on it was
        // sized as 82% of the page instead of the 50% it really spans.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/indirect_mediabox.pdf");
        let doc = Document::load(path).expect("indirect_mediabox.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        // The premise: the page carries no box of its own, and the inherited one really is
        // written with indirect entries.
        let page = doc.get_object(page_id).unwrap().as_dict().unwrap();
        assert!(page.get(b"MediaBox").is_err() && page.get(b"CropBox").is_err());
        let parent = page.get(b"Parent").unwrap().as_reference().unwrap();
        let mb = doc.get_object(parent).unwrap().as_dict().unwrap().get(b"MediaBox").unwrap().as_array().unwrap();
        assert!(matches!(mb[2], Object::Reference(_)), "the fixture's box extents must be indirect");
        assert_eq!(page_width(&doc, page_id, 0), 1008.0, "an indirect /MediaBox width must resolve, not read 0");
    }

    #[test]
    fn page_width_falls_back_to_the_crop_box_when_no_media_box_exists() {
        // Page 2 of the same fixture states only a /CropBox, with nothing to inherit.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/indirect_mediabox.pdf");
        let doc = Document::load(path).expect("indirect_mediabox.pdf fixture must load");
        let page_id = *doc.get_pages().get(&2).expect("fixture has page 2");
        assert_eq!(page_width(&doc, page_id, 0), 400.0, "the /CropBox is the fallback page box");
    }

    #[test]
    fn page_width_falls_back_when_no_ancestor_carries_a_box() {
        // A page dict with no /MediaBox anywhere up the chain still gets the letter default
        // rather than a zero-width figure scale.
        let (doc, page_id) = dense_page();
        assert_eq!(page_width(&doc, page_id, 0), 612.0, "letter fixture is 612pt wide");
        // A dangling page id resolves to nothing at all.
        assert_eq!(page_width(&doc, (9_999, 0), 0), 612.0);
    }

    #[test]
    fn over_budget_pages_degrade_to_the_early_figures_instead_of_vanishing() {
        // The defect: a page above the operation budget returned `(vec![], vec![])`, which is
        // indistinguishable from "this page has no figures" — a whole USGS cover map
        // disappeared. Truncating the walk instead keeps everything painted before the cap.
        let (doc, page_id) = dense_page();
        let (strong, _weak) = positioned_vectors_capped(&doc, page_id, 50);
        assert_eq!(strong.len(), 1, "the early-painted grid figure must survive a tripped cap");
        let grid = &strong[0];
        let kept = grid.ink().matches("<path").count();
        // The cap really does bite mid-figure (the fixture's grid is 12 rules), and what it
        // leaves behind still clears the figure bar.
        assert!((6..12).contains(&kept), "grid kept {kept} of 12 paths");
        // The grid spans 200x100pt; the scatter field (drawn after the cap) must be absent.
        assert!((grid.x_right - grid.x_left - 200.0).abs() < 2.0, "grid width {}", grid.x_right - grid.x_left);
        assert!(grid.y_bottom > 400.0, "only the top figure may survive, got y_bottom {}", grid.y_bottom);
    }

    #[test]
    fn a_dense_page_under_the_default_budget_returns_every_figure() {
        // 600_000 ops is far above this fixture, so nothing is truncated: both the grid and
        // the scatter field come back. Locks the default cap against silently re-tightening.
        let (doc, page_id) = dense_page();
        let (strong, _weak) = positioned_vectors(&doc, page_id);
        assert_eq!(strong.len(), 2, "expected the grid AND the scatter field");
        let scatter = strong.iter().find(|f| f.y_bottom < 400.0).expect("scatter figure");
        assert!(scatter.ink().matches("<path").count() > 200, "scatter kept {} paths", scatter.ink().matches("<path").count());
    }

    #[test]
    fn a_form_stream_with_no_filter_still_paints_its_paths() {
        // `unfiltered_form.pdf` (`gen_fixtures.py::gen_unfiltered_form`): the page's only ink
        // is five filled bars inside a Form XObject whose stream carries NO /Filter. lopdf
        // *errors* on `decompressed_content()` for such a stream, so the old
        // `.unwrap_or_default()` handed the decoder zero bytes and the whole figure vanished
        // — while `extract.rs`/`img.rs`, which carry the raw-bytes fallback, saw it fine.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/unfiltered_form.pdf");
        let doc = Document::load(path).expect("unfiltered_form.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        // The premise, asserted rather than assumed: the form really is unfiltered.
        let res = page_res(&doc, page_id);
        let form_id = crate::walker::xobjects_of(&doc, &res).get(b"UF".as_slice()).copied().expect("/UF form");
        let form = doc.get_object(form_id).unwrap().as_stream().unwrap();
        assert!(form.dict.get(b"Filter").is_err(), "the fixture's form must carry no /Filter");
        assert!(form.decompressed_content().is_err(), "the premise: lopdf errors without /Filter");

        let painted = walk_page(&doc, page_id, crate::MAX_FORM_WORK);
        assert_eq!(painted.len(), 5, "the five bars inside the unfiltered form must all be painted");
        // …at five distinct x offsets, so this cannot pass on five copies of one bar.
        let mut xs: Vec<i32> = painted.iter().map(|p| p.x0.round() as i32).collect();
        xs.sort_unstable();
        xs.dedup();
        assert_eq!(xs.len(), 5, "the bars must land at five offsets, got {xs:?}");
    }

    #[test]
    fn an_extgstate_defined_two_ancestors_up_still_governs_the_ink() {
        // `tests/gen_fixtures.py::gen_form_inherit`. The page has NO /Resources; the
        // /ExtGState /GA the form paints its panel through lives on the GRANDPARENT node.
        // A nearest-only read of the inherited resources left `gs` resolving to nothing, so
        // the panel painted fully opaque; and the form's indirect /Matrix, read directly,
        // degraded to the identity and put the panel 100 pt to the left.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_inherit.pdf");
        let doc = Document::load(path).expect("form_inherit.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        let painted = walk_page(&doc, page_id, crate::MAX_FORM_WORK);
        assert_eq!(painted.len(), 1, "the form paints exactly one panel");
        let p = &painted[0];
        assert!((p.fill_op - 0.5).abs() < 1e-6, "fill opacity {} (1.0 means /GA never resolved)", p.fill_op);
        assert!((p.x0 - 172.0).abs() < 0.5, "x0 {} (72 means the indirect /Matrix was lost)", p.x0);
    }

    #[test]
    fn an_extgstate_alpha_written_indirectly_does_not_hide_the_figure() {
        // `indirect_numbers.pdf`: `/GA` is `<< /ca 10 0 R /CA 11 0 R >>`. A dictionary value
        // may legally be an indirect reference, but the direct-only `num` reader returned 0.0
        // for one — below `ALPHA_HIDDEN` — so `eff_fill`/`eff_stroke` reported "no ink" and
        // every bar and axis rule drawn under `/GA gs` was DROPPED. The figure disappeared.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/indirect_numbers.pdf");
        let doc = Document::load(path).expect("indirect_numbers.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        // The premise: the alphas really are indirect.
        let res = page_res(&doc, page_id);
        let eg = res.get(b"ExtGState").unwrap().as_dict().unwrap().get(b"GA").unwrap();
        let eg = deref(&doc, eg).unwrap().as_dict().unwrap();
        assert!(matches!(eg.get(b"ca").unwrap(), Object::Reference(_)));
        assert!(matches!(eg.get(b"CA").unwrap(), Object::Reference(_)));

        // The alphas resolve to the authored values, not 0.0 …
        let egmap = extgstates_of(&doc, &res);
        assert_eq!(egmap.get(b"GA".as_slice()).copied(), Some((Some(0.85), Some(0.6))));
        // … and the ink they gate survives the walk: 8 filled bars + 2 stroked axis rules.
        let painted = walk_page(&doc, page_id, crate::MAX_FORM_WORK);
        assert_eq!(painted.len(), 10, "8 bars + 2 axes must paint under a resolved alpha");
        assert_eq!(painted.iter().filter(|p| p.fill.is_some()).count(), 8);
        assert!(painted.iter().all(|p| (p.fill_op - 0.85).abs() < 1e-6 && (p.stroke_op - 0.6).abs() < 1e-6));
        // The figure reaches the render as one placed <svg>, carrying the recovered opacity.
        let (strong, _weak) = positioned_vectors(&doc, page_id);
        assert_eq!(strong.len(), 1, "the bar chart must be one figure");
        assert!(strong[0].ink().contains("fill-opacity=\"0.85\""), "{}", strong[0].ink());
    }

    #[test]
    fn a_composited_figure_paints_its_raster_where_the_stream_did() {
        // `tests/gen_fixtures.py::gen_paint_order` is a controlled A/B in one file: two
        // geometrically identical figures, the sole difference the order of two operators.
        // The TOP one paints the raster then an opaque grey panel over it (the panel must
        // win); the BOTTOM one paints the panel then the raster (the raster must win).
        // Before the fix `composite_svg` emitted every raster first and all path ink after,
        // so BOTH figures rendered as a bare grey panel — the top one correctly, the bottom
        // one by accident of grouping, and the raster it carried was invisible.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/paint_order.pdf");
        let doc = Document::load(path).expect("paint_order.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("page 1");
        let (strong, _weak) = positioned_vectors(&doc, page_id);
        assert_eq!(strong.len(), 2, "the two bands must cluster as two figures");
        let images = crate::img::positioned_images(&doc, page_id, true);
        assert_eq!(images.len(), 2, "one raster per figure");

        // Pair each figure with the raster inside it, exactly as html.rs's absorb does.
        for (fi, fig) in strong.iter().enumerate() {
            let im = images
                .iter()
                .find(|im| im.x_left >= fig.x_left && im.x_right <= fig.x_right && im.y_bottom >= fig.y_bottom && im.y_top <= fig.y_top)
                .unwrap_or_else(|| panic!("figure {fi} must contain a raster"));
            let svg = fig.composite_svg(&[Raster {
                href: "IMG",
                rect: (im.x_left, im.x_right, im.y_bottom, im.y_top),
                ctm: im.ctm,
                seq: &im.seq,
            }]);
            let image_at = svg.find("<image ").expect("the raster must be in the composite");
            let panel_at = svg.find("fill=\"#808080\"").expect("the opaque panel must be in the composite");
            // `strong` is ordered top-of-page first, and the top figure is the raster-first one.
            if fi == 0 {
                assert!(image_at < panel_at, "raster painted first must sit BEHIND the panel: {svg}");
            } else {
                assert!(image_at > panel_at, "raster painted last must sit ON TOP of the panel: {svg}");
            }
        }
    }

    #[test]
    fn a_page_with_no_resources_still_paints_its_direct_path_ink() {
        // `tests/gen_fixtures.py::gen_no_resources_paths` is a controlled A/B in one file:
        // two pages, identical content streams, the sole difference an empty
        // `/Resources << >>` on page 2. `positioned_vectors_capped` bailed out the moment
        // the page's whole resource chain was empty — but `re`/`f` name no resource, so
        // page 1 lost its entire figure while page 2 kept it. Verified failing before the
        // fix: page 1 gave 0 strong figures and 0 painted paths, page 2 gave 1 and 8.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/no_resources_paths.pdf");
        let doc = Document::load(path).expect("no_resources_paths.pdf fixture must load");
        let pages = doc.get_pages();
        let bare = *pages.get(&1).expect("page 1");
        let with_res = *pages.get(&2).expect("page 2");
        assert!(page_resource_chain(&doc, bare).is_empty(), "page 1 must reach no /Resources at all");
        assert!(!page_resource_chain(&doc, with_res).is_empty(), "page 2 is the control");

        for (page_id, label) in [(bare, "no /Resources"), (with_res, "/Resources << >>")] {
            let painted = walk_page_bare(&doc, page_id);
            assert_eq!(painted.len(), 8, "{label}: eight filled bars must paint");
            // Nothing supplies an /ExtGState, so the spec defaults must hold: fully opaque.
            assert!(
                painted.iter().all(|p| p.fill_op == 1.0 && p.stroke_op == 1.0),
                "{label}: a page with no /ExtGState paints at full opacity"
            );
            let (strong, _weak) = positioned_vectors(&doc, page_id);
            assert_eq!(strong.len(), 1, "{label}: the bars must reach the render as one figure");
        }
    }

    /// `tests/gen_fixtures.py::gen_separation` — three pages of spot-colour fills, one per
    /// path through the tint evaluator. Returns the ink of page `n`'s single figure.
    fn separation_ink(n: u32) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/separation.pdf");
        let doc = Document::load(path).expect("separation.pdf fixture must load");
        let page_id = *doc.get_pages().get(&n).unwrap_or_else(|| panic!("fixture has page {n}"));
        let (strong, _weak) = positioned_vectors(&doc, page_id);
        assert_eq!(strong.len(), 1, "page {n}: the frame + 8 fills must be one figure");
        strong[0].ink()
    }

    #[test]
    fn a_separation_tint_goes_through_its_transform_instead_of_being_read_as_grey() {
        // THE defect: `scn` in a `Separation` space carries a TINT, and the walk had no
        // `cs` arm at all — so `.1 scn` was read as the grey level 0.1 and painted #1a1a1a.
        // The fixture's transform ramps white -> (198,198,224), the exact pale header
        // colour the visual audit found rendering near-black.
        let ink = separation_ink(1);
        assert!(ink.contains("fill=\"#c6c6e0\""), "tint 1 must be the transform's own colour: {ink}");
        // Every emitted spot fill is PALE — that is the whole claim, and it is what the
        // grey-level reading got exactly backwards.
        let mut spots = 0;
        for f in ink.split("fill=\"#").skip(1) {
            let hex = &f[..6];
            if hex == "000000" {
                continue; // the frame stroke's own fill="none" is not a hex; black is the frame
            }
            let (r, g, b) = (
                u8::from_str_radix(&hex[0..2], 16).unwrap(),
                u8::from_str_radix(&hex[2..4], 16).unwrap(),
                u8::from_str_radix(&hex[4..6], 16).unwrap(),
            );
            spots += 1;
            assert!(r >= 190 && g >= 190 && b >= 190, "spot fill #{hex} is not pale");
        }
        assert_eq!(spots, 8, "all 8 spot fills must be emitted, got {spots}");
        // …and the grey-level misreadings are gone, not merely outvoted.
        for wrong in ["#1a1a1a", "#333333", "#808080"] {
            assert!(!ink.contains(wrong), "the grey-level reading {wrong} survives: {ink}");
        }
    }

    #[test]
    fn a_devicen_tint_pair_evaluates_through_a_sampled_transform() {
        // Two colorants through a 2x2 sampled grid. This is also the end-to-end pin on
        // §7.10.2's sample order: reading the grid the other way swaps red and green here,
        // which no unit test of the evaluator alone would catch in a real colour space.
        let ink = separation_ink(2);
        for (op, want) in [("1 0", "#ff0000"), ("0 1", "#00ff00"), ("1 1", "#0000ff"), ("0 0", "#ffffff")] {
            assert!(ink.contains(&format!("fill=\"{want}\"")), "`{op} scn` must be {want}: {ink}");
        }
    }

    #[test]
    fn an_unevaluable_tint_transform_degrades_to_ink_coverage_instead_of_inverting() {
        // A Type 4 (PostScript calculator) transform is not evaluated — and MUST NOT be
        // guessed. The fallback reads the tint as ink coverage: `t` of a colorant on white
        // paper is luminance `1 - t`, so a light tint stays light. The grey-level reading
        // inverted it, which is the one thing worse than approximating.
        let g = GT_SEPARATION;
        let ink = separation_ink(3);
        assert!(ink.contains("fill=\"#cccccc\""), "tint .2 must degrade PALE: {ink}");
        assert!(ink.contains("fill=\"#404040\""), "tint .75 must degrade DARK: {ink}");
        // The inverted reading of the same two tints must be absent.
        for wrong in g {
            assert!(!ink.contains(wrong), "the inverted grey-level reading {wrong} survives: {ink}");
        }
    }

    /// The two colours the grey-level misreading produced for the Type 4 page's tints
    /// (`gray(.2)` and `gray(.75)`) — neither may appear once coverage is the fallback.
    const GT_SEPARATION: [&str; 2] = ["#333333", "#bfbfbf"];

    #[test]
    fn a_device_colour_stream_is_untouched_by_colour_space_tracking() {
        // The `cs`/`CS` arms touch the colour path of EVERY figure, so the device-space
        // fallback has to be bit-preserving: a stream that names no space, or names a
        // device one, must paint exactly what it painted before. `no_resources_paths.pdf`
        // is an ordinary `0.2 0.4 0.8 rg` device-colour figure on a page with no
        // `/Resources` at all — so it also pins that an empty colour-space map is harmless.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/no_resources_paths.pdf");
        let doc = Document::load(path).expect("no_resources_paths.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("page 1");
        let (strong, _) = positioned_vectors(&doc, page_id);
        assert!(strong[0].ink().contains("fill=\"#3366cc\""), "{}", strong[0].ink());
        // A 3-operand `scn` with no `cs` at all is still RGB, and a 1-operand one still grey.
        assert_eq!(scn_color(None, &[Object::Real(0.2), Object::Real(0.4), Object::Real(0.8)]), Some([51, 102, 204]));
        assert_eq!(scn_color(None, &[Object::Real(0.1)]), Some([26, 26, 26]));
        assert_eq!(scn_color(Some(&PaintCs::Device), &[Object::Real(0.1)]), Some([26, 26, 26]));
        // A pattern name yields no colour, exactly as the trailing-name case did.
        assert_eq!(scn_color(Some(&PaintCs::Pattern), &[Object::Name(b"P1".to_vec())]), None);
        assert_eq!(scn_color(None, &[Object::Name(b"P1".to_vec())]), None);
        // An arity that disagrees with the named Separation falls back to the count
        // dispatch rather than evaluating nonsense.
        let sep = PaintCs::Tint { k: 1, tint: None, alt: Some(3) };
        assert_eq!(scn_color(Some(&sep), &[Object::Real(0.2), Object::Real(0.4), Object::Real(0.8)]), Some([51, 102, 204]));
    }

    /// `tests/gen_fixtures.py::gen_rotated_pages` — four pages, one byte-identical content
    /// stream, `/Rotate` 0/90/180/270. Returns the doc and the four page ids in order.
    fn rotated_pages() -> (Document, Vec<ObjectId>) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/rotated_pages.pdf");
        let doc = Document::load(path).expect("rotated_pages.pdf fixture must load");
        let pages = doc.get_pages();
        let ids = (1..=4).map(|n| *pages.get(&n).unwrap_or_else(|| panic!("fixture has page {n}"))).collect();
        (doc, ids)
    }

    #[test]
    fn a_rotated_page_turns_its_figure_into_display_orientation() {
        // THE defect: `/Rotate` was read nowhere in the crate, so a landscape table on a
        // `/Rotate 90` page emitted a sideways `<svg>` — text running bottom-to-top.
        //
        // The fixture is a controlled A/B: four pages, ONE content stream, only `/Rotate`
        // differs. The 20x20 corner marker (the figure's page-space BOTTOM-LEFT rect) is
        // what makes each turn provable — a symmetric figure could not tell 90 from 270.
        let (doc, ids) = rotated_pages();
        // Local `d` of the marker at each rotation, derived from `to_local`'s closed forms.
        let want = [
            (0, "M0 300L20 300L20 280L0 280Z", (200.0, 300.0)),   // bottom-left
            (90, "M0 0L0 20L20 20L20 0Z", (300.0, 200.0)),        // top-left
            (180, "M200 0L180 0L180 20L200 20Z", (200.0, 300.0)), // top-right
            (270, "M300 200L300 180L280 180L280 200Z", (300.0, 200.0)), // bottom-right
        ];
        for (i, &page_id) in ids.iter().enumerate() {
            let (rot, marker_d, (lw, lh)) = want[i];
            assert_eq!(crate::pdfobj::page_rotation(&doc, page_id), rot);
            let (strong, _weak) = positioned_vectors(&doc, page_id);
            assert_eq!(strong.len(), 1, "/Rotate {rot}: the 9 paths must cluster as one figure");
            let f = &strong[0];
            // The PAGE-space bbox is the SAME on all four pages. `html.rs` compares these
            // boxes against rasters, captions and reading order in page space, and the turn
            // must not move them — only the figure's own local geometry turns.
            for (got, expect, what) in [(f.x_left, 100.0, "x_left"), (f.x_right, 300.0, "x_right"), (f.y_bottom, 200.0, "y_bottom"), (f.y_top, 500.0, "y_top")] {
                assert!((got - expect).abs() < 0.5, "/Rotate {rot}: {what} {got} moved out of page space");
            }
            // Local extents transpose on a quarter turn.
            assert!((f.w - lw).abs() < 0.5 && (f.h - lh).abs() < 0.5, "/Rotate {rot}: local extent {}x{} want {lw}x{lh}", f.w, f.h);
            // …and the marker lands in the corner the turn puts it in.
            let ink = f.ink();
            assert!(ink.contains(marker_d), "/Rotate {rot}: marker path {marker_d} absent from {ink}");
        }
    }

    #[test]
    fn a_page_turn_composes_with_a_label_angle_instead_of_overwriting_it() {
        // The failure mode the turn invites: a span already carries its own baseline angle
        // (a 90° y-axis title), so applying the page's turn by ASSIGNMENT double-rotates one
        // of them. The fixture draws `Alpha` upright and `Beta` at +90° in page space; on the
        // `/Rotate 90` page they must swap roles — `Beta` upright, `Alpha` turned.
        let (doc, ids) = rotated_pages();
        let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/rotated_pages.pdf")).expect("fixture bytes");
        // Emitted `rotate(...)` degrees per label, per page rotation (SVG's y-down frame
        // negates the PDF angle, so an upright label emits no transform at all).
        let want: [(&str, [f32; 4]); 2] = [("Alpha", [0.0, 90.0, 180.0, 270.0]), ("Beta", [-90.0, 0.0, 90.0, 180.0])];
        for (i, &page_id) in ids.iter().enumerate() {
            let rot = crate::pdfobj::page_rotation(&doc, page_id);
            let spans = crate::text::extract_spans(&doc, page_id, &raw);
            // The premise, asserted not assumed: the two labels really are drawn at 0° and
            // +90° in PAGE space, identically on every page.
            for (t, a) in [("Alpha", 0.0f32), ("Beta", std::f32::consts::FRAC_PI_2)] {
                let s = spans.iter().find(|s| s.text.contains(t)).unwrap_or_else(|| panic!("/Rotate {rot}: span {t} missing"));
                assert!((s.angle - a).abs() < 0.01, "/Rotate {rot}: {t} page angle {} want {a}", s.angle);
            }
            let (mut strong, _weak) = positioned_vectors(&doc, page_id);
            let labels: Vec<LabelSpan> = spans
                .iter()
                .map(|s| LabelSpan { x: s.x, y: s.y, size: s.size, width: s.width, text: s.text.clone(), bold: s.bold, italic: s.italic, angle: s.angle })
                .collect();
            attach_labels(&mut strong, &labels);
            let svg = strong[0].svg();
            for (text, degs) in &want {
                let deg = degs[i];
                let el = svg
                    .split("<text ")
                    .find(|c| c.contains(&format!(">{text}<")))
                    .unwrap_or_else(|| panic!("/Rotate {rot}: no <text> for {text} in {svg}"));
                if deg == 0.0 {
                    assert!(!el.contains("rotate("), "/Rotate {rot}: {text} must be upright, got {el}");
                } else {
                    let marker = format!("rotate({} ", fmt(deg));
                    assert!(el.contains(&marker), "/Rotate {rot}: {text} wants {marker} got {el}");
                }
            }
        }
    }

    #[test]
    fn a_composited_raster_turns_with_the_page_it_sits_on() {
        // An axis-aligned raster's placement RECT survives a quarter turn as a rect, but its
        // PIXELS turn with the page — so the plain `x/y/width/height` form (which cannot
        // express a turn) is only correct upright. On a turned page every raster must take
        // the matrix path, or the figure's ink comes out turned and the photo inside it does
        // not.
        let (doc, ids) = rotated_pages();
        // Upright: the exact rect the raster occupies in local coords, plain form.
        let (strong, _) = positioned_vectors(&doc, ids[0]);
        let images = crate::img::positioned_images(&doc, ids[0], true);
        assert_eq!(images.len(), 1, "one raster per page");
        assert!(images[0].ctm.is_none(), "the fixture's placement is axis-aligned");
        fn raster(im: &crate::img::Placed) -> Raster<'_> {
            Raster { href: "IMG", rect: (im.x_left, im.x_right, im.y_bottom, im.y_top), ctm: im.ctm, seq: &im.seq }
        }
        let up = strong[0].composite_svg(&[raster(&images[0])]);
        assert!(up.contains("<image href=\"IMG\" x=\"20\" y=\"50\" width=\"40\" height=\"30\""), "upright: {up}");
        assert!(!up.contains("matrix"), "upright must keep the plain rect form: {up}");

        // /Rotate 90: unit square (0,0)->(250,20), (1,0)->(250,60), (0,1)->(220,20), i.e.
        // matrix(0 40 -30 0 250 20) — a 40x30pt rect standing up as 30x40.
        let (strong, _) = positioned_vectors(&doc, ids[1]);
        let images = crate::img::positioned_images(&doc, ids[1], true);
        let turned = strong[0].composite_svg(&[raster(&images[0])]);
        assert!(turned.contains("transform=\"matrix(0 40 -30 0 250 20)\""), "/Rotate 90: {turned}");
    }

    #[test]
    fn a_css_overlay_stays_in_page_orientation_so_it_registers_with_its_unturned_image() {
        // `overlay_svg` is the ONE renderer that must not turn: `html.rs` positions it with
        // percentages of the raster's PAGE-space rect, over an `<img>` the raster path emits
        // unturned. Turning the ink there would slide every polygon off the photo it
        // annotates. Upright pages must be byte-identical (no wrapper at all).
        let (doc, ids) = rotated_pages();
        let (upright, _) = positioned_vectors(&doc, ids[0]);
        let up = upright[0].overlay_svg("width:100%");
        assert!(!up.contains("<g transform"), "an upright page must gain no wrapper: {up}");
        assert!(up.contains("viewBox=\"-1 -1 202 302\""), "upright viewBox is the page-space box: {up}");
        for (i, m) in [(1, "matrix(0 -1 1 0 0 300)"), (2, "matrix(-1 0 0 -1 200 300)"), (3, "matrix(0 1 -1 0 200 0)")] {
            let (figs, _) = positioned_vectors(&doc, ids[i]);
            let ov = figs[0].overlay_svg("width:100%");
            assert!(ov.contains(&format!("<g transform=\"{m}\">")), "page {}: want {m}, got {ov}", i + 1);
            // The viewBox is the PAGE-space box on every page — that is what the CSS box is.
            assert!(ov.contains("viewBox=\"-1 -1 202 302\""), "page {}: {ov}", i + 1);
        }
    }

    /// [`walk_page`] for a page that may have no resource chain at all (the helper above
    /// folds the chain, which is empty here — the point of the test).
    fn walk_page_bare(doc: &Document, page_id: ObjectId) -> Vec<Painted> {
        let content = doc.get_and_decode_page_content(page_id).expect("fixture page has content");
        let mut painted = Vec::new();
        let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
        let egmap: HashMap<Vec<u8>, (Option<f32>, Option<f32>)> = HashMap::new();
        let csmap: HashMap<Vec<u8>, Rc<PaintCs>> = HashMap::new();
        walk(
            doc,
            &content.operations,
            &XMap::new(),
            &egmap,
            &csmap,
            GState::new(Mat::ID, [0; 3], [0; 3], 1.0, 1.0, 1.0),
            &mut painted,
            0,
            &mut budget,
            &[],
        );
        painted
    }
}
