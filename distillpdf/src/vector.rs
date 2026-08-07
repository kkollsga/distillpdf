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
use crate::access::read_resolved;
use crate::pdfobj::{num, num_resolved};
use crate::walker::{descend_form, overlay_xobjects, page_resource_chain, Descend, PaintSeq, ScopePolicy, XMap};
use lopdf::{Dictionary, Object, ObjectId};
#[cfg(test)]
use lopdf::Document;
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
fn parse_cs(
    access: &dyn crate::access::DocumentAccess,
    res: &crate::walker::ResourceScope,
    o: &Object,
    depth: u32,
) -> Option<PaintCs> {
    if depth > crate::raster::MAX_CS_DEPTH {
        return None;
    }
    // `resolve_cs` follows the reference AND the `/Resources`-`/ColorSpace` name lookup that
    // makes `/CS0` mean anything (`raster.rs` owns that reader; there is one copy of it).
    crate::raster::read_color_space(access, res, o, 0, |resolved| {
        if let Object::Name(n) = resolved {
            if n.as_slice() == b"Pattern" {
                return Some(PaintCs::Pattern);
            }
        }
        if let Object::Array(a) = resolved {
            let head = read_resolved(access, a.first()?, |value| {
                value.as_name().ok().map(<[u8]>::to_vec)
            })
            .ok()
            .flatten()?;
            match head.as_slice() {
            b"Separation" | b"DeviceN" => {
                // `/Separation` is one colorant by definition; `/DeviceN`'s count is the
                // length of its names array (§8.6.6.4/§8.6.6.5).
                let k = if head == b"Separation" {
                    1
                } else {
                    read_resolved(access, a.get(1)?, |names| {
                        names.as_array().ok().map(Vec::len)
                    })
                    .ok()
                    .flatten()?
                };
                if k == 0 || k > MAX_COLORANTS {
                    return None;
                }
                // The alternate space reduces to a component count — and an `/Indexed`
                // alternate is illegal, so it degrades rather than being read as gray.
                let alt = crate::raster::cs_model(access, res, a.get(2)?, depth + 1).and_then(|c| match c {
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
                    .and_then(|f| Function::parse(access, f))
                    .filter(|f| !matches!((f.n_outputs(), alt), (Some(n), Some(k)) if n != k));
                return Some(PaintCs::Tint { k, tint, alt });
            }
            b"Pattern" => return Some(PaintCs::Pattern),
            _ => {}
            }
        }
        crate::raster::cs_model(access, res, o, depth).map(|_| PaintCs::Device)
    })?
}

/// The colour spaces one resource dictionary defines, by name — the `/ColorSpace` half of
/// what `cs`/`CS` resolve against, folded over the page's resource chain exactly as the
/// `/ExtGState` map is.
fn colorspaces_of(
    access: &dyn crate::access::DocumentAccess,
    scope: &crate::walker::ResourceScope,
    resources: &Dictionary,
) -> HashMap<Vec<u8>, Rc<PaintCs>> {
    let mut map = HashMap::new();
    if let Ok(color_spaces) = resources.get(b"ColorSpace") {
        let _ = read_resolved(access, color_spaces, |color_spaces| {
            let Ok(color_spaces) = color_spaces.as_dict() else {
                return;
            };
            for (name, value) in color_spaces.iter() {
                if let Some(color_space) = parse_cs(access, scope, value, 0) {
                    map.insert(name.clone(), Rc::new(color_space));
                }
            }
        });
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

/// A stroke as the SVG needs it: colour, width, and the dash pattern — all in PAGE space,
/// the line width and the dash lengths having been through the same `ctm.scale()`.
///
/// A struct rather than the `([u8; 3], f32)` pair it replaced because a dash is not
/// decoration: `econ_EM_2606_02234`'s six DAGs state in their captions that dashed nodes are
/// *unobserved variables*, so rendering them solid does not degrade the figure, it changes
/// what it says.
#[derive(Clone)]
struct Stroke {
    color: [u8; 3],
    width: f32,
    /// `(pattern, phase)` from the `d` operator, scaled. `None` = solid, which is both the
    /// PDF default and what an empty or invalid array means (§8.4.3.6).
    dash: Option<(Vec<f32>, f32)>,
}

/// A painted path with its colours, opacities and page-space bounding box.
struct Painted {
    segs: Vec<Seg>,
    fill: Option<[u8; 3]>,
    stroke: Option<Stroke>,
    fill_op: f32,
    stroke_op: f32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    seq: PaintSeq, // paint position in the content TREE — preserved for correct z-order
    // Active clip rect (page space) when this path was painted, if it actually crops it.
    // Rendered as an SVG <clipPath> so the visible ink matches the PDF (no overshoot).
    clip: Option<ClipRect>,
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
    fill_a: f32,   // ExtGState `ca` — fill alpha, as set by `gs` in THIS stream
    stroke_a: f32, // ExtGState `CA` — stroke alpha, likewise
    // The product of the alphas of every enclosing TRANSPARENCY GROUP (§11.6.6). A group's
    // own initial state starts at `ca`/`CA` = 1.0 (§11.4.7.2) and the alpha in force at its
    // `Do` applies to the group's composited result — so the caller's alpha must be carried
    // beside `fill_a`/`stroke_a`, never inside them, or the group's own first `gs` erases it.
    group_a: (f32, f32),
    // The `d` operator's dash pattern and phase, in USER space — scaled by `ctm.scale()`
    // at paint time exactly as `lw` is. `None` = solid, the PDF default.
    dash: Option<(Vec<f32>, f32)>,
    // Active clipping rectangle in PAGE space (x0, y0, x1, y1), the intersection of every
    // `W`/`W*` clip seen so far on the q/Q stack. `None` = unclipped (page bounds). A plot
    // clips its reference curves to the axes box; honouring it crops the curve overshoot.
    clip: Option<ClipRect>,
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
        GState { ctm, fill, stroke, lw, fill_a, stroke_a, group_a: (1.0, 1.0), dash: None, clip: None, fill_cs: None, stroke_cs: None }
    }
    /// The alpha a paint made right now actually reaches the page with: this stream's `ca`
    /// scaled by every enclosing transparency group's.
    fn fill_alpha(&self) -> f32 {
        self.fill_a * self.group_a.0
    }
    fn stroke_alpha(&self) -> f32 {
        self.stroke_a * self.group_a.1
    }
}

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
    /// itself, so its position among the ink is free. Kept as [`ClipDefs`] rather than the
    /// finished string because [`PlacedSvg::composite_svg`] adds to it — a clipped raster
    /// needs a `<clipPath>` too, and it must not duplicate one the ink already defined.
    defs: ClipDefs,
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
    /// Whether the cluster carries **graphic ink** — see [`has_graphic_ink`]. A map, a DAG or
    /// a plot has it; a ruled table, a page-furniture card and a dot-leader row do not.
    graphic_ink: bool,
    /// Set when this candidate cleared the STRONG size bar but was demoted to weak by the
    /// figure-ink gate ([`passes_ink_gate`]) — i.e. it is page furniture, a ruled table or a
    /// monochrome chrome card as far as the gate can tell. It is still emitted if a figure
    /// caption anchors it (`html.rs`'s weak promotion), so the gate never deletes outright;
    /// the count is what `PdfDocument::figure_gate_stats` reports.
    demoted: bool,
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

/// A clipping rectangle in PAGE space, `(x0, y0, x1, y1)` with y up — the shape the walks,
/// the emitters and [`ClipDefs`] all pass around.
pub(crate) type ClipRect = (f32, f32, f32, f32);

/// The `<clipPath>` definitions one figure's SVG carries, and the geometry-keyed ids that
/// name them.
///
/// **One copy, two emitters.** [`build_svg`] mints these for path ink and
/// [`PlacedSvg::composite_svg`] mints them for a clipped raster; both need the id of "the
/// clipPath for THIS page-space rect in THIS figure", and two independently-written copies
/// of that rule would drift apart the moment either changed.
///
/// The id is derived from the clip's own figure-LOCAL geometry so that `same id ⟺ same
/// <rect>`. That is what keeps clipping correct once the whole document is assembled: ids
/// must be globally unique, but every figure shares the same page content origin, so an
/// origin- or index-based id collides across figures — and the doc-wide `dedup_ids` pass
/// then renames the colliding `id=` WITHOUT touching the `clip-path="url(#…)"` reference,
/// silently breaking the clip. A geometry-keyed id avoids that: distinct clips get distinct
/// ids (no rename), and any genuinely identical clip that does get renamed still resolves to
/// a clipPath with an identical rect.
#[derive(Clone, Default)]
struct ClipDefs {
    ids: Vec<(i32, i32, i32, i32)>,
    body: String,
}

impl ClipDefs {
    /// The id for page-space clip rect `c` inside a figure whose page box is
    /// `(fx0, fy0, fx1, fy1)` under page turn `rot`, defining the `<clipPath>` on first use.
    fn id_for(&mut self, rot: i32, fx0: f32, fy0: f32, fx1: f32, fy1: f32, c: ClipRect) -> String {
        // page space -> figure-local (y flipped, turned by /Rotate). The ORIGIN comes from
        // the corner map; the EXTENTS are the page-space spans merely transposed — taking
        // them as a difference of two mapped corners instead would reassociate the
        // subtraction (`(c.2-x0)-(c.0-x0)` is not `c.2-c.0` in f32) and move an upright
        // figure's clip by a rounding step.
        let (ax, ay) = to_local(rot, fx0, fy0, fx1, fy1, c.0, c.3);
        let (bx, by) = to_local(rot, fx0, fy0, fx1, fy1, c.2, c.1);
        let (lx, ly) = (ax.min(bx), ay.min(by));
        let (lw, lh) = local_extent(rot, (c.2 - c.0).max(0.0), (c.3 - c.1).max(0.0));
        let key = ((lx * 4.0) as i32, (ly * 4.0) as i32, (lw * 4.0) as i32, (lh * 4.0) as i32);
        let id = format!("clip_{}_{}_{}_{}", key.0, key.1, key.2, key.3);
        if !self.ids.contains(&key) {
            self.body.push_str(&format!(
                "<clipPath id=\"{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/></clipPath>",
                id,
                fmt(lx),
                fmt(ly),
                fmt(lw),
                fmt(lh)
            ));
            self.ids.push(key);
        }
        id
    }

    /// The `<defs>` block to emit before any ink — empty when nothing is clipped.
    fn render(&self) -> String {
        if self.body.is_empty() {
            String::new()
        } else {
            format!("<defs>{}</defs>", self.body)
        }
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

/// The share of a figure's own claimed text that has to be cells of a table the page also
/// emits before the figure **yields** that text to the table — see [`PlacedSvg::attach`].
///
/// `0.6` sits in the gap the corpus leaves: the two callout panels that duplicate a table
/// score 0.95 and 0.79, and the cover map that must keep its labels scores at most 0.32.
const TABLE_YIELD_SHARE: f32 = 0.6;

/// The **contents** of an `image/svg+xml` data URI — everything between its `<svg …>` root
/// and `</svg>` — or `None` for any other URI.
///
/// Only [`crate::img`]'s codec placeholder produces such a URI; a decoded raster is always
/// PNG or JPEG. See the call site in [`PlacedSvg::composite_svg`] for why a placeholder is
/// pasted in rather than referenced.
pub fn inline_svg_payload(href: &str) -> Option<String> {
    use base64::Engine as _;
    let b64 = href.strip_prefix("data:image/svg+xml;base64,")?;
    let svg = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64).ok()?).ok()?;
    let body = svg.split_once('>')?.1.strip_suffix("</svg>")?;
    Some(body.to_string())
}

impl PlacedSvg {
    /// Whether this figure draws curves or slanted lines — see [`has_graphic_ink`]. Used by
    /// `html.rs` to tell a diagram's own label grid (which must not suppress the diagram)
    /// from a real data table that happens to overlap it.
    pub fn graphic_ink(&self) -> bool {
        self.graphic_ink
    }

    /// Whether the figure-ink gate demoted this cluster from strong to weak — see
    /// [`PlacedSvg::demoted`](#structfield.demoted). `html.rs` reads it to tell a demoted
    /// candidate it promoted back (a caption anchored it) from one the gate really suppressed.
    pub fn demoted(&self) -> bool {
        self.demoted
    }

    /// Attach form-internal text spans that belong to this figure, mapping each
    /// into local SVG coords. A span is claimed when its centre lies within the
    /// bbox expanded by [`LABEL_MARGIN`].
    ///
    /// The *claim* stays in page space — spans arrive in page space and so does the bbox, so
    /// which figure owns a label is decided exactly as it was before `/Rotate` existed here.
    /// Only the label's **local placement** is turned into display orientation.
    fn attach(&mut self, spans: &[(LabelSpan, bool)]) {
        let mine: Vec<&(LabelSpan, bool)> = spans
            .iter()
            .filter(|(s, _)| {
                let cx = s.x + s.width * 0.5;
                let cy = s.y + s.size * 0.5;
                cx >= self.x_left - LABEL_MARGIN && cx <= self.x_right + LABEL_MARGIN && cy >= self.y_bottom - LABEL_MARGIN && cy <= self.y_top + LABEL_MARGIN
            })
            .collect();
        // A figure whose text is almost entirely a table's cells is not a figure with
        // labels — it is a panel REPRODUCING a table the page also emits as `<table>`, and
        // the reader gets the numbers twice (`geology_usgs_fs` p3's "Benchmarks for
        // evaluating groundwater quality" callout, `nonenglish_spanish_astrofisica` p24's
        // "Gravedad" callout). Such a figure yields those spans; it keeps whatever is its
        // own (the panel's heading, its ink).
        //
        // The share is measured over the FIGURE's own text, not the table's, and that is the
        // whole discrimination. Measured on the pages in question: the two duplicating
        // callouts are at 0.95 and 0.79, while `geology_usgs_fs` p1's cover MAP — which a
        // spurious label grid overlaps, and which must keep every city name — is at 0.21,
        // 0.17 and 0.32 against the three grids it overlaps. A map is a figure that happens
        // to overlap a table; a callout panel is a table with a box drawn round it.
        let claimed = mine.iter().filter(|(_, t)| *t).count();
        let yields = !mine.is_empty() && claimed as f32 >= mine.len() as f32 * TABLE_YIELD_SHARE;
        for (s, in_table) in mine {
            if yields && *in_table {
                continue;
            }
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
        let mut out = self.defs.render();
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
    /// `html.rs` positions this box over the raster's `<img>`, whose pixels the raster path
    /// now emits in DISPLAY orientation (`img::turn_pixels`) — so this renderer, its viewBox
    /// and the CSS box that carries it are all in display orientation too, and the ink needs
    /// no turn back. (It used to be the one renderer left in page orientation, wrapped in an
    /// `un_rotate` matrix to register with an unturned `<img>`; that `<img>` no longer exists.)
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
            self.ink(),
            self.label_texts(),
        )
    }

    /// The CSS box that places this figure's [`PlacedSvg::overlay_svg`] over a raster whose
    /// page-space rect is `(x_left, x_right, y_bottom, y_top)`.
    ///
    /// Both sides are taken in the raster's DISPLAY orientation, since that is what the `<img>`
    /// shows. For an upright page every term is the page-space difference this replaced, so no
    /// overlay moves by a rounding step.
    pub fn overlay_style(&self, im: (f32, f32, f32, f32)) -> String {
        let (ix0, ix1, iy0, iy1) = im;
        let (dw, dh) = local_extent(self.rot, (ix1 - ix0).max(1.0), (iy1 - iy0).max(1.0));
        let (ax, ay) = to_local(self.rot, ix0, iy0, ix1, iy1, self.x_left, self.y_top);
        let (bx, by) = to_local(self.rot, ix0, iy0, ix1, iy1, self.x_right, self.y_bottom);
        // Extents are the page-space spans transposed, never a difference of mapped corners
        // (`ClipDefs::id_for` records why).
        let (lw, lh) = local_extent(self.rot, self.x_right - self.x_left, self.y_top - self.y_bottom);
        format!(
            "position:absolute;left:{:.2}%;top:{:.2}%;width:{:.2}%;height:{:.2}%",
            ax.min(bx) / dw * 100.0,
            ay.min(by) / dh * 100.0,
            lw / dw * 100.0,
            lh / dh * 100.0,
        )
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
        let mut defs = self.defs.clone();
        for r in rasters {
            let (ix0, ix1, iy0, iy1) = r.rect;
            // The clip in force at this raster's `Do`, when it actually cropped it. `r.rect`
            // is already the CROPPED extent (so the viewBox bounds to what shows), while the
            // pixels still fill the full placement — which is why a clipped raster always
            // carries a matrix and never takes the plain-rect branch below.
            //
            // The clip goes on a WRAPPING `<g>`, never on the `<image>` itself: `transform`
            // establishes a new user space for the element it sits on, and a `clip-path` on
            // that same element resolves in the NEW space — so the mask would be scaled by
            // the placement matrix (here a 160x120 factor) and land off the page, hiding the
            // raster completely. The `<g>` keeps the mask in the figure's own coordinates,
            // which is the space `ClipDefs` mints the rect in. Verified against a renderer:
            // on the `<image>` the fixture rendered blank, on the `<g>` it renders its window.
            let (clip_open, clip_close) = match r.clip {
                Some(c) => (
                    format!("<g clip-path=\"url(#{})\">", defs.id_for(self.rot, self.x_left, self.y_bottom, self.x_right, self.y_top, c)),
                    "</g>",
                ),
                None => (String::new(), ""),
            };
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
            // A CODEC PLACEHOLDER is SVG, and an `<image href>` pointing at SVG is where
            // renderers disagree: browsers draw it, and mupdf (and anything else that only
            // decodes rasters for `<image>`) logs "ignoring external image" and draws
            // nothing — which is the blank frame the placeholder exists to end. Inside an
            // `<svg>` we can simply *be* the SVG: drop the placeholder's own root and paste
            // its shapes into this figure under a translate+scale. Nothing else takes this
            // branch — no decoded raster is ever an `image/svg+xml` URI.
            if let Some(inner) = r.placeholder {
                // A placeholder the figure paints ALL of its graphic ink over is a BASEMAP,
                // and a basemap we cannot decode has to stay a hole. The frame and the two
                // label lines are ours, not the document's: under a map's coastlines and
                // county fills they do not read as "we could not decode this", they show
                // through the gaps in the ink as a grey box with words across it — which on
                // a map is indistinguishable from cartography. `geology_usgs_fs` p1 is the
                // case: a JPX hillshade under the whole study-area map, whose "JPEG 2000
                // image / not decoded (JPXDecode)" surfaced through the vector layer and a
                // reviewer read it as a semi-transparent watermark the source does not have.
                //
                // The test is exact, not a coverage heuristic: the placeholder precedes
                // EVERY path of a cluster that carries graphic ink (curves or diagonals —
                // a map, not a frame). A figure that IS the undecodable image keeps its
                // label, which is what the placeholder exists for: in the corpus every
                // other composited placeholder (26 of 27, across
                // `geology_usgs_volcanic_hazards_california`) is painted AFTER ink and is
                // untouched. The decline itself is still reported by `stream_integrity()`
                // and `extract_images()`, where a program looks for it.
                //
                // The viewBox still grows by the placement: the raster really did occupy
                // that box, and cropping the figure to the ink alone could cut the map.
                if self.graphic_ink && !self.paths.is_empty() && self.paths.iter().all(|(seq, _)| seq > r.seq) {
                    continue;
                }
                let (sw, sh) = (img_lw / (ix1 - ix0).max(0.1), img_lh / (iy1 - iy0).max(0.1));
                content.push((
                    r.seq,
                    format!(
                        "{clip_open}<g transform=\"translate({} {}) scale({} {})\">{inner}</g>{clip_close}",
                        fmt(img_lx),
                        fmt(img_ly),
                        fmt(sw),
                        fmt(sh)
                    ),
                ));
                continue;
            }
            let el = match self.rot_image_matrix(r) {
                Some([a, b, c, d, e, f]) => format!(
                    "{clip_open}<image href=\"{}\" x=\"0\" y=\"0\" width=\"1\" height=\"1\" preserveAspectRatio=\"none\" transform=\"matrix({} {} {} {} {} {})\"/>{clip_close}",
                    r.href,
                    fmt(a),
                    fmt(b),
                    fmt(c),
                    fmt(d),
                    fmt(e),
                    fmt(f),
                ),
                None => format!(
                    "{clip_open}<image href=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\"/>{clip_close}",
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
        let mut body = defs.render();
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
    /// The **body** of this raster's URI when it is a codec placeholder rather than pixels
    /// ([`inline_svg_payload`]) — pasted into the figure instead of referenced. `None` for
    /// every decoded raster, which is all of them but a JPEG 2000 or JBIG2 image.
    ///
    /// It has to arrive here from `html.rs`: by the time a raster reaches `composite_svg`
    /// its `href` is a substitution token, not the URI.
    pub placeholder: Option<&'a str>,
    pub rect: (f32, f32, f32, f32),
    pub ctm: Option<[f32; 6]>,
    /// The page-space clipping rectangle in force at the `Do`, when it actually cropped the
    /// placement (`(x0, y0, x1, y1)`, y up). Emitted as an SVG `clip-path`; `None` — the
    /// common case — leaves the `<image>` unmasked.
    pub clip: Option<ClipRect>,
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
fn extgstates_of(
    access: &dyn crate::access::DocumentAccess,
    resources: &Dictionary,
) -> HashMap<Vec<u8>, (Option<f32>, Option<f32>)> {
    let mut map = HashMap::new();
    if let Ok(states) = resources.get(b"ExtGState") {
        let _ = read_resolved(access, states, |states| {
            let Ok(states) = states.as_dict() else {
                return;
            };
            for (name, value) in states.iter() {
                let _ = read_resolved(access, value, |state| {
                    let Ok(state) = state.as_dict() else {
                        return;
                    };
                    let ca = state
                        .get(b"ca")
                        .ok()
                        .map(|value| num_resolved(access, value));
                    let big = state
                        .get(b"CA")
                        .ok()
                        .map(|value| num_resolved(access, value));
                    map.insert(name.clone(), (ca, big));
                });
            }
        });
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
fn finish(cur: &mut Vec<Seg>, fill: Option<[u8; 3]>, stroke: Option<Stroke>, fill_op: f32, stroke_op: f32, clip: Option<ClipRect>, seq: PaintSeq, out: &mut Vec<Painted>) {
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

/// The axis-aligned RULING a page paints, in page space (y up).
///
/// A ruled table publishes its own cell boundaries geometrically — the rules *are* the grid —
/// and that evidence is completely independent of where the text sits, which is what makes it
/// able to see a blank cell (invisible to text clustering by construction) and a band title
/// that would otherwise terminate a text run. [`crate::lattice`] turns these segments into
/// closed-cell frames; nothing here knows what a table is.
#[derive(Default)]
pub struct PageRules {
    /// `(x0, x1, y)` per horizontal rule, `x0 < x1`.
    pub h: Vec<(f32, f32, f32)>,
    /// `(x, y0, y1)` per vertical rule, `y0 < y1`.
    pub v: Vec<(f32, f32, f32)>,
}

/// A rule is THIN — anything fatter is a filled panel (a shaded header band, a callout box),
/// whose *edges* may bound cells but whose body is not a boundary.
const RULE_THICK: f32 = 3.0;
/// Shorter than this is a tick, a hyphen glyph outline or a leader dot, not a cell boundary.
const RULE_MIN_LEN: f32 = 8.0;
/// How far off-axis a segment may run and still be read as a rule.
const RULE_STRAIGHT: f32 = 0.8;
/// Ruling budget for one page. A cartographic page paints tens of thousands of short
/// segments; the lattice caps its own line count anyway, and this bounds the collection.
const MAX_RULES: usize = 20_000;

/// One straight segment's contribution to the ruling, if it is one.
fn push_rule(out: &mut PageRules, ax: f32, ay: f32, bx: f32, by: f32) {
    let (dx, dy) = ((bx - ax).abs(), (by - ay).abs());
    if dy <= RULE_STRAIGHT && dx >= RULE_MIN_LEN {
        out.h.push((ax.min(bx), ax.max(bx), (ay + by) * 0.5));
    } else if dx <= RULE_STRAIGHT && dy >= RULE_MIN_LEN {
        out.v.push(((ax + bx) * 0.5, ay.min(by), ay.max(by)));
    }
}

/// Read the page's painted paths as ruling. Both forms a producer uses are collected: a THIN
/// FILLED box (how every government form paints its grid) and every axis-aligned straight
/// segment of a STROKED path (which hands over all four edges of a stroked cell box).
fn rules_of(painted: &[Painted]) -> PageRules {
    let mut out = PageRules::default();
    for p in painted {
        if out.h.len() + out.v.len() >= MAX_RULES {
            break;
        }
        if p.fill.is_some() && p.fill_op > 0.05 {
            let (w, h) = (p.x1 - p.x0, p.y1 - p.y0);
            if h <= RULE_THICK && w >= RULE_MIN_LEN {
                out.h.push((p.x0, p.x1, (p.y0 + p.y1) * 0.5));
            } else if w <= RULE_THICK && h >= RULE_MIN_LEN {
                out.v.push(((p.x0 + p.x1) * 0.5, p.y0, p.y1));
            }
        }
        if p.stroke.is_some() && p.stroke_op > 0.05 {
            let (mut cx, mut cy, mut sx, mut sy) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            let mut open = false;
            for s in &p.segs {
                match *s {
                    Seg::M(x, y) => {
                        cx = x;
                        cy = y;
                        sx = x;
                        sy = y;
                        open = true;
                    }
                    Seg::L(x, y) => {
                        if open {
                            push_rule(&mut out, cx, cy, x, y);
                        }
                        cx = x;
                        cy = y;
                        open = true;
                    }
                    Seg::C(_, _, _, _, x, y) => {
                        cx = x;
                        cy = y;
                    }
                    Seg::Z => {
                        if open {
                            push_rule(&mut out, cx, cy, sx, sy);
                        }
                        cx = sx;
                        cy = sy;
                    }
                }
            }
        }
    }
    out
}

/// Vector figures on a page, top-to-bottom.
/// Returns `(strong, weak)` placed vector figures. STRONG are emitted unconditionally; WEAK
/// are sub-threshold candidates html.rs promotes only when a figure caption anchors to one.
pub fn positioned_vectors(
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
) -> (Vec<PlacedSvg>, Vec<PlacedSvg>) {
    let (s, w, _) = positioned_vectors_capped(access, page_id, MAX_OPS);
    (s, w)
}

/// [`positioned_vectors`] plus the page's ruling — one walk, two answers, for the caller
/// (`html.rs`) that needs both. The ruling is the table pillar's second evidence source.
pub fn positioned_vectors_ruled(
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
) -> (Vec<PlacedSvg>, Vec<PlacedSvg>, PageRules) {
    positioned_vectors_capped(access, page_id, MAX_OPS)
}

/// Just the page's ruling, for the table pillar's own entry point (`extract_tables`), which
/// has no use for the figures.
pub fn page_rules(
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
) -> PageRules {
    rules_of(&painted_page(access, page_id, MAX_OPS))
}

/// [`positioned_vectors`] with an explicit operation budget (the public entry point passes
/// [`MAX_OPS`]). Exposed internally so the truncation behaviour is unit-testable with a tiny
/// cap instead of a half-million-operation fixture.
fn positioned_vectors_capped(
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
    cap: usize,
) -> (Vec<PlacedSvg>, Vec<PlacedSvg>, PageRules) {
    let painted = painted_page(access, page_id, cap);
    let rules = rules_of(&painted);
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
    let rot = crate::pdfobj::page_rotation(access, page_id);
    let page_w = page_width(access, page_id, rot);
    let (strong, weak) = cluster_figures(painted, rot);
    let strong: Vec<PlacedSvg> = strong.iter().map(|c| build_svg(c, page_w, rot)).collect();
    let weak: Vec<PlacedSvg> = weak
        .iter()
        .map(|(c, demoted)| {
            let mut s = build_svg(c, page_w, rot);
            s.demoted = *demoted;
            s
        })
        .collect();
    (strong, weak, rules)
}

/// Interpret one page's content (and its annotation appearances) into painted paths — the
/// shared front half of [`positioned_vectors_capped`] and [`page_rules`].
fn painted_page(
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
    cap: usize,
) -> Vec<Painted> {
    // A page with no `/Resources` anywhere in its tree used to return here, empty. But the
    // path operators — `m`/`l`/`c`/`re`/`v`/`y`/`h` and the `f`/`S`/`B` that paint them —
    // name no resource at all: `/Resources` is only needed to resolve an `/ExtGState` alpha
    // or a form `Do`, and a page can legally draw its whole figure without either. The
    // guard therefore deleted every direct path a resource-less page drew, and the SVG with
    // it. Both maps below are simply empty for such a page, `Do` resolves to nothing, and
    // the graphics state starts at the spec defaults (opaque, black) — which is exactly
    // what a page with no `/ExtGState` is entitled to.
    let chain = page_resource_chain(access, page_id);
    let content = match access
        .page_content(page_id)
        .ok()
        .and_then(|bytes| lopdf::content::Content::decode(&bytes).ok())
    {
        Some(content) => content,
        None => return Vec::new(),
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
    let resource_scope = crate::walker::ResourceScope::page(access, page_id);
    for res in &chain {
        let _ = res.read(|dictionary| {
            overlay_xobjects(access, dictionary, &mut xmap);
            egmap.extend(extgstates_of(access, dictionary));
            csmap.extend(colorspaces_of(access, &resource_scope, dictionary));
        });
    }
    let mut painted = Vec::new();
    let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
    walk(access, ops, &xmap, &egmap, &csmap, GState::new(Mat::ID, [0; 3], [0; 3], 1.0, 1.0, 1.0), &mut painted, 0, &mut budget, &[]);
    // §12.5.5: an annotation's appearance is page content, painted on top of the content
    // stream and reachable from neither it nor the page's `/Resources`. Its ink is this
    // walk's business exactly as a form's is — see `walker::placed_appearances` for the
    // `/BBox`→`/Rect` mapping that makes it land where a viewer puts it.
    for (k, (_, ap, actm)) in crate::walker::placed_appearances(access, page_id).into_iter().enumerate() {
        // The appearance's resources are its OWN, so the scope it descends from is empty.
        let f = match descend_form(
            access,
            &ap,
            &XMap::new(),
            ScopePolicy::OverlayParent,
            0,
            &mut budget,
            0,
        ) {
            Descend::Into(f) => f,
            Descend::Skip => continue,
            Descend::Halt => break,
        };
        let (mut aeg, mut acs) = (HashMap::new(), HashMap::new());
        if let Some(fr) = &f.scope.resources {
            let _ = fr.read(|resources| {
                aeg.extend(extgstates_of(access, resources));
                acs.extend(colorspaces_of(
                    access,
                    &crate::walker::ResourceScope::own(fr.clone()),
                    resources,
                ));
            });
        }
        let mut g = GState::new(f.matrix.mul(actm), [0; 3], [0; 3], 1.0, 1.0, 1.0);
        if let Some(bb) = ap
            .read(|ap| crate::walker::form_bbox_clip(access, ap, g.ctm))
            .flatten()
        {
            g.clip = Some(intersect_clip(g.clip, (bb.x0, bb.y0, bb.x1, bb.y1)));
        }
        let here = PaintSeq::at(&[], content.operations.len() + k);
        walk(access, &f.ops, &f.scope.xobjects, &aeg, &acs, g, &mut painted, 1, &mut budget, here.as_slice());
    }
    painted
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
fn page_width(access: &dyn crate::access::DocumentAccess, page_id: ObjectId, rot: i32) -> f32 {
    crate::pdfobj::page_box(access, page_id)
        .map(|b| if rot % 180 == 0 { (b[2] - b[0]).abs() } else { (b[3] - b[1]).abs() })
        .filter(|w| *w > 1.0)
        .unwrap_or(crate::pdfobj::DEFAULT_PAGE_PTS.0)
}

/// Distribute form-internal text labels among the figures on a page (each label
/// goes to the figure whose bbox, expanded by a margin, contains its centre).
///
/// The spans are [`coalesce_glyph_runs`]-joined first, so a label a cartographic exporter
/// drew one glyph per `Tj` reaches its figure as the WORD it is.
/// `in_table[i]` says whether `spans[i]` sits inside a table the page emits as `<table>` —
/// see [`PlacedSvg::attach`] for what a figure does about it. Passed as a parallel slice
/// rather than a field on [`LabelSpan`] because the test is made in DISPLAY space (against
/// the turned table boxes) while the span handed here is page space; keeping it out of the
/// struct keeps the two spaces from being confused for each other.
pub fn attach_labels(figs: &mut [PlacedSvg], spans: &[LabelSpan], in_table: &[bool]) {
    // A joined run inherits its FIRST member's verdict: the run is one word, and a word is
    // in the table or it is not.
    let spans: Vec<(LabelSpan, bool)> = coalesce_glyph_runs(spans)
        .into_iter()
        .map(|(s, first)| (s, in_table.get(first).copied().unwrap_or(false)))
        .collect();
    for f in figs.iter_mut() {
        f.attach(&spans);
    }
}

/// Join label spans that are consecutive glyphs of one word back into one span.
///
/// **The defect this exists for.** `geology_usgs_fs.pdf` p1 draws its map's place names one
/// `Tj` per glyph. Every glyph was claimed, mapped and emitted — and the figure rendered ten
/// `<text>` elements reading `C` `l` `o` `v` `e` `r` `d` `a` `l` `e`. Visually the map is
/// right, but the word does not exist in the output: it cannot be searched, selected, or
/// counted, and the label reads as ten one-letter labels to anything downstream. The body
/// path never had this problem — `layout::lines_of` assembles spans into runs — so it showed
/// up as "the label is decoded but never rendered" when in fact it was never *assembled*.
///
/// **Deliberately narrow.** Only a gap the reader would not see as a break is closed
/// ([`crate::textutil::glyph_adjacent`]), and no space is ever invented: a span pair separated by a real word
/// space stays two spans, exactly as before. Adjacency is measured **along the baseline**,
/// not along x, so a 90° axis title's glyphs join the same way. Style, size and angle must
/// all match — a run that changes any of them is a different run.
///
/// Output order is the input's: each joined run takes the position of its FIRST member, so
/// no figure's existing `<text>` sequence is reshuffled by this pass. That first index is
/// returned beside the run — a caller carrying a per-span verdict ([`attach_labels`]'s
/// `in_table`) needs to know which input the run speaks for.
fn coalesce_glyph_runs(spans: &[LabelSpan]) -> Vec<(LabelSpan, usize)> {
    // Along-baseline and across-baseline coordinates of a span's anchor.
    let uv = |s: &LabelSpan| {
        let (sin, cos) = s.angle.sin_cos();
        (s.x * cos + s.y * sin, -s.x * sin + s.y * cos)
    };
    // Group candidates: same style/size/angle and the same baseline, ordered along it. The
    // sort is on a copy of the INDICES, so the spans themselves never move.
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by(|&a, &b| {
        let (sa, sb) = (&spans[a], &spans[b]);
        let (ua, va) = uv(sa);
        let (ub, vb) = uv(sb);
        (sa.angle, sa.size, sa.bold, sa.italic, va, ua, a)
            .partial_cmp(&(sb.angle, sb.size, sb.bold, sb.italic, vb, ub, b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // (first input index, the run built so far, its end along the baseline).
    let mut runs: Vec<(usize, LabelSpan, f32)> = Vec::new();
    for &i in &order {
        let s = &spans[i];
        let (u, v) = uv(s);
        if let Some((first, run, end)) = runs.last_mut() {
            let (_, rv) = uv(run);
            let gap = u - *end;
            if run.angle == s.angle
                && (run.size - s.size).abs() < 0.05
                && run.bold == s.bold
                && run.italic == s.italic
                && (rv - v).abs() <= s.size * 0.15
                && crate::textutil::glyph_adjacent(gap, s.size)
            {
                run.text.push_str(&s.text);
                *end = u + s.width;
                // The run's width is its PAINTED extent — the distance from the first glyph's
                // anchor to the last glyph's end — not the sum of the glyph widths, so the
                // claim tests and the viewBox see the space the word really occupies.
                run.width = *end - uv(run).0;
                *first = (*first).min(i);
                continue;
            }
        }
        runs.push((i, clone_label(s), u + s.width));
    }
    runs.sort_by_key(|(first, _, _)| *first);
    runs.into_iter().map(|(first, s, _)| (s, first)).collect()
}

fn clone_label(s: &LabelSpan) -> LabelSpan {
    LabelSpan {
        x: s.x,
        y: s.y,
        size: s.size,
        width: s.width,
        text: s.text.clone(),
        bold: s.bold,
        italic: s.italic,
        angle: s.angle,
    }
}

/// Walk a content stream, collecting painted paths in page space. Recurses into
/// Form XObjects (most figures are a single form `Do`) applying the form `/Matrix`
/// — without this, vector figures drawn inside a form are invisible. Images are
/// left to [`crate::img`].
#[allow(clippy::too_many_arguments)]
fn walk(
    access: &dyn crate::access::DocumentAccess,
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
    // Effective fill/stroke for a paint op, after applying ExtGState alpha. Only a paint
    // that is EXACTLY invisible (`ca 0`) is dropped; everything else renders at its own
    // opacity, however faint. The old bar — drop anything under 0.04 — deleted the data
    // itself in the figures where alpha IS the quantity: an attention map's weights arrive
    // as 0.0039..0.98, so three quarters of every such figure was thresholded away.
    let eff_fill = |g: &GState| if g.fill_alpha() > 0.0 { Some(g.fill) } else { None };
    let eff_stroke = |g: &GState| {
        if g.stroke_alpha() <= 0.0 {
            return None;
        }
        let s = g.ctm.scale();
        // The dash lengths live in user space and follow the CTM exactly as the line width
        // does — one scale factor, applied to both, so a scaled-down figure's dashes stay in
        // proportion to its strokes.
        let dash = g.dash.as_ref().map(|(pat, phase)| (pat.iter().map(|v| (v * s).max(0.01)).collect(), phase * s));
        Some(Stroke { color: g.stroke, width: (g.lw * s).max(0.3), dash })
    };

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
            // `d` — the dash pattern. The walk had no arm for it at all, so every dashed
            // stroke rendered SOLID: in `econ_EM_2606_02234`'s DAGs, whose captions say
            // dashed nodes are unobserved variables, that erases the distinction the figure
            // exists to draw. An empty array is the spec's own "solid", and so is a pattern
            // that is invalid (negative, or all zeros) — §8.4.3.6 calls those an error, and
            // solid is the reading that cannot invent a dash the file never asked for.
            "d" => {
                let pat: Vec<f32> = o.first().and_then(|x| x.as_array().ok()).map(|a| a.iter().map(num).collect()).unwrap_or_default();
                let ok = !pat.is_empty() && pat.iter().all(|v| *v >= 0.0 && v.is_finite()) && pat.iter().any(|v| *v > 0.0);
                g.dash = ok.then(|| (pat, o.get(1).map(num).filter(|p| p.is_finite() && *p >= 0.0).unwrap_or(0.0)));
            }
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
                        g.clip = Some(intersect_clip(g.clip, bb));
                    }
                    pending_clip = false;
                }
                match op.operator.as_str() {
                    "f" | "F" | "f*" => finish(&mut cur, eff_fill(&g), None, g.fill_alpha(), g.stroke_alpha(), g.clip, PaintSeq::at(here, opi), out),
                    "S" | "s" => finish(&mut cur, None, eff_stroke(&g), g.fill_alpha(), g.stroke_alpha(), g.clip, PaintSeq::at(here, opi), out),
                    "B" | "B*" | "b" | "b*" => finish(&mut cur, eff_fill(&g), eff_stroke(&g), g.fill_alpha(), g.stroke_alpha(), g.clip, PaintSeq::at(here, opi), out),
                    _ => cur.clear(), // "n": clip-only path → no ink
                }
            }
            "Do" => {
                // Images are `crate::img`'s business; only forms carry path ink. The
                // descent inherits the page's scope (`OverlayParent`) so a form can paint
                // through an ExtGState or XObject the page defines.
                let Some((_, stream)) = crate::walker::xobject_at(access, xmap, o) else {
                    continue;
                };
                let f = match descend_form(access, &stream, xmap, ScopePolicy::OverlayParent, depth, budget, egmap.len()) {
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
                    let _ = fr.read(|resources| {
                        for (k, v) in extgstates_of(access, resources) {
                            child_eg.insert(k, v);
                        }
                        for (k, v) in colorspaces_of(
                            access,
                            &crate::walker::ResourceScope::own(fr.clone()),
                            resources,
                        ) {
                            child_cs.insert(k, v);
                        }
                    });
                }
                let mut sub = g.clone();
                // A TRANSPARENCY GROUP's alpha applies to its composited result, and the
                // group's own state starts opaque (§11.4.7.2/§11.6.6). Carrying the caller's
                // alpha into `fill_a` instead is what let a group's first `gs` — routinely
                // `ca 1 CA 1`, since inside the group that IS the initial value — erase it.
                let transparency_group = stream
                    .read(|stream| crate::walker::is_transparency_group(access, stream))
                    .unwrap_or(false);
                if transparency_group {
                    sub.group_a = (g.fill_alpha(), g.stroke_alpha());
                    sub.fill_a = 1.0;
                    sub.stroke_a = 1.0;
                }
                sub.ctm = f.matrix.mul(g.ctm);
                // §8.10.2: the form's `/BBox`, in form space, CLIPS its content. Intersect it
                // into the clip the child inherits; `finish` already keeps a clip only when it
                // actually crops, so the ubiquitous full-page BBox costs nothing.
                if let Some(bb) = stream
                    .read(|stream| crate::walker::form_bbox_clip(access, stream, sub.ctm))
                    .flatten()
                {
                    sub.clip = Some(intersect_clip(sub.clip, (bb.x0, bb.y0, bb.x1, bb.y1)));
                }
                walk(access, &f.ops, &f.scope.xobjects, &child_eg, &child_cs, sub, out, depth + 1, budget, PaintSeq::at(here, opi).as_slice());
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

/// A cluster held aside as a WEAK candidate, paired with `true` when it cleared the strong
/// size bar and was demoted there by [`passes_ink_gate`] rather than never clearing it.
type WeakCluster = (Vec<Painted>, bool);

/// Group painted paths into vertically-contiguous clusters and split them into
/// `(strong, weak)`: STRONG clusters are real figures emitted unconditionally; WEAK
/// clusters clear only the relaxed bar and are emitted by html.rs solely when a figure
/// caption anchors to one (a small diagram the strong bar would drop). Clusters failing
/// even the weak bar (single rules, stray marks) are discarded.
///
/// Each weak entry carries a flag: `true` means it cleared the strong SIZE bar and was
/// **demoted** by [`passes_ink_gate`] (page furniture), `false` means it never cleared it.
/// The two are handled identically downstream — the flag exists so the demotions can be
/// counted and reported rather than disappearing silently.
///
/// `rot` is the page's `/Rotate`, and it reaches this function for exactly one reason: on a
/// quarter-turned page **every** text span is rotated in page space, and `layout::lines_of`
/// drops rotated spans from the body reading order outright — so the figure is the only thing
/// carrying that page's text into the output. Demoting it there deletes the page. Until the
/// body pipeline reads a turned page upright (filed separately), the gate stands down on
/// 90°/270° pages; upright pages, which is every page of every document that motivated the
/// gate, are unaffected.
fn cluster_figures(mut paths: Vec<Painted>, rot: i32) -> (Vec<Vec<Painted>>, Vec<WeakCluster>) {
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
    let (mut strong, mut weak): (Vec<Vec<Painted>>, Vec<WeakCluster>) = (Vec::new(), Vec::new());
    for c in clusters {
        let (w, h) = extent(&c);
        if c.len() >= MIN_PATHS && w >= MIN_W && h >= MIN_H {
            // The strong bar measures SIZE; the gate asks whether the ink is a figure's.
            // A cluster that fails it falls through to weak rather than out of the document:
            // the strong size bars are all at or above the weak ones, so it always lands.
            if rot % 180 != 0 || passes_ink_gate(&c) {
                strong.push(c);
            } else {
                weak.push((c, true));
            }
        } else if c.len() >= WEAK_MIN_PATHS && w >= WEAK_MIN_W && h >= WEAK_MIN_H {
            weak.push((c, false));
        }
    }
    // Restore stream paint order within each cluster (banding sorted by y): a fill
    // drawn after an outline must paint on top of it, not be reordered by position.
    for c in strong.iter_mut().chain(weak.iter_mut().map(|(c, _)| c)) {
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

/// An opacity, at the precision opacity needs. [`fmt`]'s two decimals are right for
/// coordinates and wrong here: an attention weight of `0.0039` rounds to `"0"`, which is
/// SVG for "delete this element" — the very paint the alpha fix exists to keep.
fn fmt_alpha(v: f32) -> String {
    let s = format!("{v:.4}");
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
    // clipped to the axes box) — see [`ClipDefs`], which the raster emitter shares.
    let mut defs = ClipDefs::default();
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
        let fop = if p.fill.is_some() && p.fill_op < 0.999 { format!(" fill-opacity=\"{}\"", fmt_alpha(p.fill_op)) } else { String::new() };
        let stroke = match &p.stroke {
            Some(s) => {
                let sop = if p.stroke_op < 0.999 { format!(" stroke-opacity=\"{}\"", fmt_alpha(p.stroke_op)) } else { String::new() };
                // A dash pattern is content, not styling — see [`Stroke`]. `stroke-dashoffset`
                // is only emitted when the phase is nonzero, so an ordinary dash stays terse.
                let dash = match &s.dash {
                    Some((pat, phase)) => {
                        let arr = pat.iter().map(|v| fmt(*v)).collect::<Vec<_>>().join(" ");
                        let off = if *phase > 0.005 { format!(" stroke-dashoffset=\"{}\"", fmt(*phase)) } else { String::new() };
                        format!(" stroke-dasharray=\"{arr}\"{off}")
                    }
                    None => String::new(),
                };
                format!(" stroke=\"{}\" stroke-width=\"{}\"{sop}{dash}", hex(s.color), fmt(s.width.max(0.3)))
            }
            None => String::new(),
        };
        let clip_attr = match p.clip {
            Some(c) => format!(" clip-path=\"url(#{})\"", defs.id_for(rot, x0, y0, x1, y1, c)),
            None => String::new(),
        };
        paths.push((p.seq.clone(), format!("<path d=\"{d}\" fill=\"{fill}\"{fop}{stroke}{clip_attr}/>")));
    }
    PlacedSvg { y_top: y1, y_bottom: y0, x_left: x0, x_right: x1, defs, paths, w: lw, h: lh, page_w, labels: Vec::new(), plot, graphic_ink: has_graphic_ink(cluster), demoted: false, rot }
}

/// Does this cluster draw anything a **ruled table cannot**?
///
/// A data table, a form's cell grid, a dot-leader row and an SEC filing's backdrop card are
/// drawn entirely from horizontal and vertical straight edges. A map coastline, a DAG's
/// arrows, a plot's curves and a logo are not: they need Béziers or slanted lines. So "the
/// cluster contains at least one curve segment or one non-axis-aligned line" separates
/// diagram ink from tabular chrome without looking at colour or size at all.
///
/// `TOL` is a third of a point: a hairline rule authored at a fractionally non-integer
/// coordinate (or one that has been through a CTM) must still read as axis-aligned, while a
/// genuine diagonal on a figure-sized cluster is orders of magnitude longer than this.
fn has_graphic_ink(cluster: &[Painted]) -> bool {
    const TOL: f32 = 0.34;
    cluster.iter().any(|p| {
        let mut cur: Option<(f32, f32)> = None;
        let mut start: Option<(f32, f32)> = None;
        for s in &p.segs {
            match *s {
                Seg::C(..) => return true,
                Seg::M(x, y) => {
                    cur = Some((x, y));
                    start = Some((x, y));
                }
                Seg::L(x, y) => {
                    if let Some((px, py)) = cur {
                        if (x - px).abs() > TOL && (y - py).abs() > TOL {
                            return true;
                        }
                    }
                    cur = Some((x, y));
                }
                // `h` closes back to the subpath's start; that closing edge is ink too.
                Seg::Z => {
                    if let (Some((px, py)), Some((sx, sy))) = (cur, start) {
                        if (sx - px).abs() > TOL && (sy - py).abs() > TOL {
                            return true;
                        }
                    }
                    cur = start;
                }
            }
        }
        false
    })
}

/// How many **saturated** colours the cluster paints with.
///
/// The companion of [`has_graphic_ink`] for the one honest exception to it: a bar/column
/// chart is drawn entirely from axis-aligned rectangles and so carries no graphic ink, but
/// it encodes its data in a *palette*. Page furniture does not — a backdrop card is a tint
/// and an accent, a ruled table is black on white.
///
/// Only genuinely saturated colours count (`SAT`, HSV saturation): the near-greys that make
/// up chrome (`#f1f2f2` banding, `#cccccc` hairlines, `#e0e0ed` table shading) are excluded
/// by construction, and so is the single pale wash a card is filled with. Measured over the
/// 54-document local corpus, the SEC filings' furniture tops out at **one** saturated colour
/// (a `#0000ff` link rule; the `#cceeff` card wash is 0.20 saturated and does not count),
/// while every rects-only real figure in the corpus carries **three or more** — a
/// Word/Excel column chart's series (`#4a7ebb #98b954 #be4b48`), a USGS legend's 6–15 keys.
fn palette_variety(cluster: &[Painted]) -> usize {
    // HSV saturation, i.e. chroma relative to the brightest channel: an absolute chroma bar
    // would call a pale tint (#cceeff) and a dark one (#361c43) the same thing.
    const SAT: f32 = 0.25;
    let mut seen: Vec<[u8; 3]> = Vec::new();
    for c in cluster.iter().flat_map(|p| p.fill.into_iter().chain(p.stroke.as_ref().map(|s| s.color))) {
        let (mx, mn) = (c.iter().copied().max().unwrap_or(0), c.iter().copied().min().unwrap_or(0));
        if mx == 0 || (mx - mn) as f32 / mx as f32 <= SAT {
            continue; // black, a grey, or a wash — not a palette entry
        }
        if !seen.contains(&c) {
            seen.push(c);
        }
    }
    seen.len()
}

/// The **figure-ink gate**: does a cluster that clears the strong size bar actually look like
/// a figure, or like page furniture?
///
/// A strong cluster is accepted only if it draws something a ruled table cannot
/// ([`has_graphic_ink`] — a curve or a slanted line) **or** it paints with a real palette
/// ([`palette_variety`]). The disjunction is the point: the ink test alone would reject a
/// legitimate rects-only bar chart, and the palette test alone would reject a black-and-white
/// line plot. Everything that fails *both* — an SEC filing's backdrop card, a TOC's
/// dot-leader block, a financial table's cell rules, an invisible white-rect layer — is
/// demoted to a WEAK candidate, **not deleted**: a figure caption sitting beside it still
/// promotes it in `html.rs`. What the gate demoted is counted and reportable
/// (`PdfDocument::figure_gate_stats`), because a silent filter is how real content is lost
/// without anyone noticing.
fn passes_ink_gate(cluster: &[Painted]) -> bool {
    const MIN_PALETTE: usize = 3;
    has_graphic_ink(cluster) || palette_variety(cluster) >= MIN_PALETTE
}

/// Intersect a new page-space clip rectangle into the one already in force. Shared with
/// [`crate::img`]'s walk, which tracks the same state.
pub(crate) fn intersect_clip(cur: Option<ClipRect>, add: ClipRect) -> ClipRect {
    match cur {
        Some(c) => {
            let n = Rect::new(c.0, c.1, c.2, c.3).intersect(Rect::new(add.0, add.1, add.2, add.3));
            (n.x0, n.y0, n.x1, n.y1)
        }
        None => add,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::test_adapter;

    fn page_width(doc: &Document, page_id: ObjectId, rot: i32) -> f32 {
        super::page_width(&test_adapter(doc), page_id, rot)
    }

    /// Load an adversarial fixture (`tests/gen_fixtures.py::gen_form_bomb`) and set up the
    /// exact state `positioned_vectors_capped` hands to [`walk`], so a test can drive the
    /// walker with its own budget.
    fn adversarial(name: &str) -> (Document, ObjectId) {
        let path = format!("{}/../tests/fixtures_pdf/adversarial/{name}", env!("CARGO_MANIFEST_DIR"));
        let doc = Document::load(&path).unwrap_or_else(|e| panic!("{name} fixture must load: {e}"));
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        (doc, page_id)
    }

    /// Every cluster that cleared the strong SIZE bar, in page order: the figures
    /// [`passes_ink_gate`] accepted **plus** the ones it demoted to weak candidates.
    ///
    /// Most tests below exercise how a cluster is *built* — geometry, colour, alpha, dashes,
    /// clipping, page rotation — on deliberately minimal fixtures that are often a handful of
    /// axis-aligned monochrome rules, i.e. exactly what the ink gate demotes. That judgement
    /// is orthogonal to what they assert and has its own fixtures
    /// (`the_ink_gate_*`), so they read the size-bar view instead of the gated one.
    fn size_bar_figures(doc: &Document, page_id: ObjectId) -> Vec<PlacedSvg> {
        let (strong, weak) = positioned_vectors(&test_adapter(doc), page_id);
        let mut all: Vec<PlacedSvg> = strong.into_iter().chain(weak.into_iter().filter(|v| v.demoted())).collect();
        all.sort_by(|a, b| b.y_top.partial_cmp(&a.y_top).unwrap_or(std::cmp::Ordering::Equal));
        all
    }

    /// The page's own (nearest) resource dictionary — the last entry of the overlay chain.
    fn page_res(doc: &Document, page_id: ObjectId) -> Dictionary {
        page_resource_chain(&test_adapter(doc), page_id)
            .pop()
            .expect("fixture page has resources")
            .read(Clone::clone)
            .unwrap()
    }

    fn walk_page(doc: &Document, page_id: ObjectId, budget: usize) -> Vec<Painted> {
        let access = test_adapter(doc);
        let content = doc.get_and_decode_page_content(page_id).expect("fixture page has content");
        let mut xmap = XMap::new();
        let mut egmap: HashMap<Vec<u8>, (Option<f32>, Option<f32>)> = HashMap::new();
        let mut csmap: HashMap<Vec<u8>, Rc<PaintCs>> = HashMap::new();
        let resource_scope = crate::walker::ResourceScope::page(&access, page_id);
        for res in &page_resource_chain(&access, page_id) {
            let _ = res.read(|dictionary| {
                overlay_xobjects(&access, dictionary, &mut xmap);
                egmap.extend(extgstates_of(&access, dictionary));
                csmap.extend(colorspaces_of(
                    &access,
                    &resource_scope,
                    dictionary,
                ));
            });
        }
        let mut painted = Vec::new();
        let mut budget = crate::WalkBudget::new(budget);
        walk(&access, &content.operations, &xmap, &egmap, &csmap, GState::new(Mat::ID, [0; 3], [0; 3], 1.0, 1.0, 1.0), &mut painted, 0, &mut budget, &[]);
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
        let _ = positioned_vectors(&test_adapter(&doc), page_id);
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
    fn a_map_carries_graphic_ink_and_a_ruled_table_does_not() {
        // The primitive `html.rs` uses to tell a diagram's own label grid (which must not
        // suppress the diagram) from a real data table that overlaps one. The fixture is a
        // controlled A/B: `map_label_grid.pdf` draws the SAME 4x4 label grid on both pages,
        // over a Bézier coastline with slanted borders on page 1 and over nothing but
        // horizontal/vertical rules on page 2.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/map_label_grid.pdf");
        let doc = Document::load(path).expect("map_label_grid.pdf fixture must load");
        let pages = doc.get_pages();
        for (n, want) in [(1u32, true), (2, false)] {
            let page_id = *pages.get(&n).expect("fixture has this page");
            let strong = size_bar_figures(&doc, page_id);
            assert_eq!(strong.len(), 1, "page {n}: the fixture draws exactly one cluster");
            assert_eq!(strong[0].graphic_ink(), want, "page {n}: graphic ink misread");
        }
    }

    /// The gate fixture (`tests/gen_fixtures.py::gen_ink_gate`), by page.
    fn ink_gate_page(n: u32) -> (Document, ObjectId) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/ink_gate.pdf");
        let doc = Document::load(path).expect("ink_gate.pdf fixture must load");
        let page_id = *doc.get_pages().get(&n).expect("fixture has this page");
        (doc, page_id)
    }

    #[test]
    fn a_rects_only_chart_keeps_its_figure_but_furniture_is_demoted() {
        // `ink_gate.pdf`: three rects-only pages, so `has_graphic_ink` is false on all three
        // and the palette is the whole verdict. Before the gate all three were emitted as
        // figures — 315 of 315 SVGs on the three SEC filings in the local corpus were this
        // kind of chrome, and one of them (SpaceX p4) had no text on the page at all.
        for (n, accept, why) in [
            (1u32, true, "a column chart's four series colours are a palette"),
            (2, false, "a grey header band and zebra shading are not"),
            (3, false, "a white-on-white layer paints nothing at all"),
        ] {
            let (doc, page_id) = ink_gate_page(n);
            let (strong, weak) = positioned_vectors(&test_adapter(&doc), page_id);
            let demoted: Vec<&PlacedSvg> = weak.iter().filter(|v| v.demoted()).collect();
            // Every page draws exactly one cluster over the strong size bar; the gate decides
            // which side of the line it lands on.
            assert_eq!(strong.len() + demoted.len(), 1, "page {n}: one cluster over the size bar");
            assert_eq!(strong.len() == 1, accept, "page {n}: {why}");
        }
    }

    #[test]
    fn a_demoted_cluster_is_held_as_a_candidate_not_deleted() {
        // The gate's safety property: what it rejects stays reachable. A demoted cluster is a
        // WEAK candidate, so a figure caption beside it promotes it back in `html.rs` exactly
        // as it would a small hand-drawn diagram — a rejection is never a deletion, and the
        // count is reported (`PdfDocument::figure_gate_stats`).
        let (doc, page_id) = ink_gate_page(2);
        let (strong, weak) = positioned_vectors(&test_adapter(&doc), page_id);
        assert!(strong.is_empty(), "the shaded table is not a figure");
        let d = weak.iter().find(|v| v.demoted()).expect("the rejected cluster is still a candidate");
        assert!(d.ink().contains("<path"), "and it kept its geometry, ready to be promoted");
    }

    #[test]
    fn the_gate_stands_down_on_a_quarter_turned_page() {
        // `layout::lines_of` drops rotated spans from the body reading order, so on a 90°/270°
        // page the figure is the ONLY carrier of the page's text (verified on
        // `med_crispr_clinical_trials_pmc.pdf` p19-24: with the gate applied there, six pages
        // of a clinical-trials table rendered as 160-character stubs). The exemption is what
        // keeps the gate from deleting those pages. `ink_gate.pdf` p2 and p4 draw the same
        // shaded table, so the turn is the only difference between the two verdicts.
        let (doc, page_id) = ink_gate_page(2);
        let (upright, uweak) = positioned_vectors(&test_adapter(&doc), page_id);
        assert!(upright.is_empty() && uweak.iter().any(|v| v.demoted()), "upright: the gate applies");
        let (doc, page_id) = ink_gate_page(4);
        let (turned, tweak) = positioned_vectors(&test_adapter(&doc), page_id);
        assert_eq!(turned.len(), 1, "a quarter-turned page keeps its figure");
        assert!(!tweak.iter().any(|v| v.demoted()), "and nothing was demoted on it");
    }

    #[test]
    fn an_undecodable_basemap_paints_nothing_while_an_undecodable_top_layer_still_names_itself() {
        // `tests/gen_fixtures.py::gen_codec_basemap`: the same `/JPXDecode` image and the same
        // eight curves on two pages, differing only in paint order. The placeholder names the
        // codec so a hole is not mistaken for a figure we chose not to emit — but under a
        // BASEMAP that inverts. Its frame and label are ours, not the document's, and beneath
        // a map's ink they do not read as "we could not decode this": they show through the
        // gaps as a grey box with words across it, which on a map is indistinguishable from
        // cartography. `geology_usgs_fs` p1's JPX hillshade did exactly that, and a reviewer
        // read the label as a semi-transparent watermark the source does not have.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/codec_basemap.pdf");
        let doc = lopdf::Document::load(path).expect("codec_basemap.pdf fixture must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let html = crate::html::to_html(&crate::access::test_adapter_with_source(&doc, &raw), crate::html::Mode::Page, true, true);
        let svgs: Vec<&str> = html.match_indices("<svg").map(|(i, _)| &html[i..html[i..].find("</svg>").map(|e| i + e).unwrap_or(html.len())]).collect();
        assert_eq!(svgs.len(), 2, "one composited figure per page");
        assert_eq!(svgs[0].matches("<path").count(), 8, "page 1 keeps every stroke of its ink");
        assert!(!svgs[0].contains("not decoded"), "the basemap under all the ink paints nothing: {}", svgs[0]);
        assert!(!svgs[0].contains("stroke-dasharray=\"6 4\""), "not even the frame: {}", svgs[0]);
        // The control, and the case the placeholder exists for: the same image painted OVER
        // the ink is the figure's top layer and must still say which codec it is.
        assert!(svgs[1].contains("JPEG 2000 image"), "the top-layer placeholder is kept: {}", svgs[1]);
        assert!(svgs[1].contains("not decoded (JPXDecode)"), "and names the filter: {}", svgs[1]);
    }

    /// One label span: `text` starting at `(x, y)`, `size`-tall, `w` wide, at `angle`.
    fn lspan(x: f32, y: f32, w: f32, text: &str, angle: f32) -> LabelSpan {
        LabelSpan { x, y, size: 8.0, width: w, text: text.to_string(), bold: false, italic: false, angle }
    }

    #[test]
    fn a_label_drawn_one_glyph_per_tj_is_reassembled_into_its_word() {
        // `geology_usgs_fs.pdf` p1 draws its place names a glyph at a time. Every glyph was
        // claimed and emitted, so nothing was "lost" — and the figure rendered ten <text>
        // elements reading C l o v e r d a l e. The word did not exist in the output.
        let mut spans = Vec::new();
        let (mut x, w) = (100.0f32, 4.0f32);
        for ch in "Cloverdale".chars() {
            spans.push(lspan(x, 200.0, w, &ch.to_string(), 0.0));
            x += w;
        }
        // A word space is a gap the reader DOES see: it must stay two spans, and no space may
        // be invented (the run keeps exactly the characters it was given).
        spans.push(lspan(x + 8.0 * 0.5, 200.0, 12.0, "Napa", 0.0));
        // A second baseline, and a run at 90 deg whose glyphs advance in +y: adjacency is
        // measured along the BASELINE, so this joins exactly like the upright one.
        let mut y = 300.0f32;
        for ch in "Sonoma".chars() {
            spans.push(lspan(400.0, y, w, &ch.to_string(), std::f32::consts::FRAC_PI_2));
            y += w;
        }
        // Same place, different size: a different run, however adjacent.
        let mut big = lspan(x + 8.0 * 0.5 + 12.0, 200.0, 6.0, "XL", 0.0);
        big.size = 14.0;
        spans.push(big);

        let out: Vec<LabelSpan> = coalesce_glyph_runs(&spans).into_iter().map(|(s, _)| s).collect();
        let texts: Vec<&str> = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["Cloverdale", "Napa", "Sonoma", "XL"], "runs, in the input's order");
        // The joined run's width is the PAINTED extent, so the claim tests and the viewBox
        // see the space the word really occupies.
        assert!((out[0].width - 40.0).abs() < 1e-4, "run width {}", out[0].width);
        assert_eq!((out[0].x, out[0].y), (100.0, 200.0), "a run keeps its first glyph's anchor");
        assert_eq!(out[2].angle, std::f32::consts::FRAC_PI_2, "the rotated run keeps its angle");

        // THE invariant, and the one the corpus proved: this pass never adds, drops or alters
        // a character — over all 54 corpus documents the <text> character multiset was
        // byte-identical before and after, while 2,502 glyph fragments became words.
        let chars = |v: &[LabelSpan]| {
            let mut c: Vec<char> = v.iter().flat_map(|s| s.text.chars()).collect();
            c.sort_unstable();
            c
        };
        assert_eq!(chars(&spans), chars(&out), "coalescing is a regrouping, never a rewrite");
    }

    #[test]
    fn a_hairline_rule_off_the_integer_grid_is_still_axis_aligned() {
        // The tolerance is what keeps a ruled table honest: its rules land at fractional
        // coordinates once a CTM has been through them, and a quarter-point of slop must
        // not read as a diagonal. A real diagonal on a figure-sized cluster is orders of
        // magnitude longer than this.
        let rule = |dy: f32| Painted {
            segs: vec![Seg::M(10.0, 100.0), Seg::L(400.0, 100.0 + dy)],
            fill: None,
            stroke: Some(Stroke { color: [0, 0, 0], width: 0.5, dash: None }),
            fill_op: 1.0,
            stroke_op: 1.0,
            x0: 10.0,
            y0: 100.0,
            x1: 400.0,
            y1: 100.0 + dy,
            seq: PaintSeq::at(&[], 0),
            clip: None,
        };
        assert!(!has_graphic_ink(&[rule(0.0)]), "a flat rule is not graphic ink");
        assert!(!has_graphic_ink(&[rule(0.25)]), "a hairline off the grid is not a diagonal");
        assert!(has_graphic_ink(&[rule(6.0)]), "a genuine slant IS graphic ink");
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
        // Size-bar view (see `size_bar_figures`): the fixture's grid is 12 black rules, so
        // the ink gate demotes it — what this test is about is that the CAP does not delete it.
        let (strong, weak, _) = positioned_vectors_capped(&test_adapter(&doc), page_id, 50);
        let strong: Vec<PlacedSvg> = strong.into_iter().chain(weak.into_iter().filter(|v| v.demoted())).collect();
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
        let strong = size_bar_figures(&doc, page_id);
        assert_eq!(strong.len(), 2, "expected the grid AND the scatter field");
        let scatter = strong.iter().find(|f| f.y_bottom < 400.0).expect("scatter figure");
        assert!(scatter.ink().matches("<path").count() > 200, "scatter kept {} paths", scatter.ink().matches("<path").count());
    }

    #[test]
    fn a_form_stream_with_no_filter_still_paints_its_paths() {
        // `unfiltered_form.pdf` (`gen_fixtures.py::gen_unfiltered_form`): the page's only ink
        // is five filled bars inside a Form XObject whose stream carries NO /Filter. Through
        // lopdf 0.43 `decompressed_content()` *errored* for such a stream, so the old
        // `.unwrap_or_default()` handed the decoder zero bytes and the whole figure vanished
        // — while `extract.rs`/`img.rs`, which carry the raw-bytes fallback, saw it fine.
        // lopdf 0.44 returns the raw content instead; the fallback keeps us correct either way.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/unfiltered_form.pdf");
        let doc = Document::load(path).expect("unfiltered_form.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        // The premise, asserted rather than assumed: the form really is unfiltered.
        let res = page_res(&doc, page_id);
        let form_id = crate::walker::xobjects_of(&test_adapter(&doc), &res).get(b"UF".as_slice()).copied().expect("/UF form");
        let form = doc.get_object(form_id).unwrap().as_stream().unwrap();
        assert!(form.dict.get(b"Filter").is_err(), "the fixture's form must carry no /Filter");
        assert_eq!(
            form.decompressed_content().ok().as_deref(),
            Some(&form.content[..]),
            "the premise, lopdf 0.44: an unfiltered stream decodes to its raw content",
        );

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
        let eg = doc
            .get_object(eg.as_reference().unwrap())
            .unwrap()
            .as_dict()
            .unwrap();
        assert!(matches!(eg.get(b"ca").unwrap(), Object::Reference(_)));
        assert!(matches!(eg.get(b"CA").unwrap(), Object::Reference(_)));

        // The alphas resolve to the authored values, not 0.0 …
        let egmap = extgstates_of(&test_adapter(&doc), &res);
        assert_eq!(egmap.get(b"GA".as_slice()).copied(), Some((Some(0.85), Some(0.6))));
        // … and the ink they gate survives the walk: 8 filled bars + 2 stroked axis rules.
        let painted = walk_page(&doc, page_id, crate::MAX_FORM_WORK);
        assert_eq!(painted.len(), 10, "8 bars + 2 axes must paint under a resolved alpha");
        assert_eq!(painted.iter().filter(|p| p.fill.is_some()).count(), 8);
        assert!(painted.iter().all(|p| (p.fill_op - 0.85).abs() < 1e-6 && (p.stroke_op - 0.6).abs() < 1e-6));
        // The figure reaches the render as one placed <svg>, carrying the recovered opacity.
        let strong = size_bar_figures(&doc, page_id);
        assert_eq!(strong.len(), 1, "the bar chart must be one figure");
        assert!(strong[0].ink().contains("fill-opacity=\"0.85\""), "{}", strong[0].ink());
    }

    #[test]
    fn a_dashed_stroke_reaches_the_svg_dashed() {
        // `d` had no arm in the walk, so the dash state was never read and every dashed
        // stroke rendered SOLID. Destructive, not cosmetic: `econ_EM_2606_02234`'s six DAGs
        // dash the nodes that are UNOBSERVED variables and say so in their captions, and
        // `cs_DS_2606_02492` p24 describes "edges shown in dashed light blue" — solid
        // strokes silently assert the opposite.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/dashes.pdf");
        let doc = Document::load(path).expect("dashes.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        let strong = size_bar_figures(&doc, page_id);
        assert_eq!(strong.len(), 1, "the five rules and their frame are one figure");
        let ink = strong[0].ink();
        // The five strokes differ ONLY in dash state, so every difference below is `d`'s.
        assert!(ink.contains("stroke-dasharray=\"3 2\""), "[3 2] must survive: {ink}");
        assert!(ink.contains("stroke-dasharray=\"6 3\" stroke-dashoffset=\"2\""), "a nonzero phase is an offset: {ink}");
        // Solid, reset (`[] d`) and invalid (`[0 0] d`) all carry no dash at all: 7 paths
        // (5 rules, the frame and the fixture's slanted stroke), exactly 2 dashed.
        // `stroke-dasharray="0 0"` renders as NOTHING in a browser, so the invalid case
        // degrades visibly rather than being deleted.
        assert_eq!(ink.matches("<path").count(), 7);
        assert_eq!(ink.matches("stroke-dasharray").count(), 2, "only the two valid patterns dash: {ink}");
        assert!(!ink.contains("stroke-dasharray=\"0"), "an all-zero pattern must not reach the SVG: {ink}");
    }

    #[test]
    fn a_transparency_groups_own_gs_does_not_erase_the_alpha_it_was_invoked_with() {
        // THE defect. A `/Group << /S /Transparency >>` form is composited as a unit: the
        // `ca`/`CA` at its `Do` applies to the group's RESULT, and the group's own state
        // starts opaque (§11.4.7.2, §11.6.6). The walk inherited the caller's alpha into the
        // child state instead, where the child's first `gs` — `ca 1 CA 1`, which inside a
        // group is simply the initial value spelled out — overwrote it. Every element then
        // painted fully opaque: `attention_1706.03762` p13's 615 weighted cells emitted ZERO
        // opacity attributes, and its 561 `ca 0` cells, which must be invisible, painted
        // solid.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/alpha_groups.pdf");
        let doc = Document::load(path).expect("alpha_groups.pdf fixture must load");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        let painted = walk_page(&doc, page_id, crate::MAX_FORM_WORK);
        // 5 group rects + the non-group control + the frame + the fixture's slanted stroke.
        // The `ca 0` element is ABSENT: transparent is not faint, and the no-ink rule still
        // deletes it.
        assert_eq!(painted.len(), 8, "got {:?}", painted.iter().map(|p| p.fill_op).collect::<Vec<_>>());
        let fills: Vec<f32> = painted.iter().filter(|p| p.fill.is_some()).map(|p| p.fill_op).collect();
        for want in [0.0039, 0.02, 0.04, 0.5, 0.98] {
            assert!(fills.iter().any(|a| (a - want).abs() < 1e-4), "alpha {want} lost; got {fills:?}");
        }
        // The control: an ORDINARY form inherits the graphics state, so its own `gs` is
        // authoritative. Only a group may not overwrite what it was invoked with.
        assert!(fills.iter().any(|a| (a - 0.25).abs() < 1e-4), "a plain form's own gs must still win: {fills:?}");
        assert!(fills.iter().all(|a| *a > 0.0), "a `ca 0` paint must not reach the output");

        // …and every one of them reaches the SVG at its own opacity. Sub-0.04 paints used to
        // be dropped outright by `ALPHA_HIDDEN`, which in an attention or density figure
        // deletes the data itself — alpha IS the quantity there.
        let strong = size_bar_figures(&doc, page_id);
        assert_eq!(strong.len(), 1, "the box is one figure");
        let ink = strong[0].ink();
        for want in ["0.0039", "0.02", "0.04", "0.5", "0.98", "0.25"] {
            assert!(ink.contains(&format!("fill-opacity=\"{want}\"")), "opacity {want} missing from {ink}");
        }
        // Two decimals would have printed the faintest weight as `fill-opacity="0"`, which
        // is SVG for "delete this element" — the exact paint the fix exists to keep.
        assert!(!ink.contains("fill-opacity=\"0\""), "a faint paint was rounded to invisible: {ink}");
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
        let strong = size_bar_figures(&doc, page_id);
        assert_eq!(strong.len(), 2, "the two bands must cluster as two figures");
        let images = crate::img::positioned_images(&test_adapter(&doc), page_id, true);
        assert_eq!(images.len(), 2, "one raster per figure");

        // Pair each figure with the raster inside it, exactly as html.rs's absorb does.
        for (fi, fig) in strong.iter().enumerate() {
            let im = images
                .iter()
                .find(|im| im.x_left >= fig.x_left && im.x_right <= fig.x_right && im.y_bottom >= fig.y_bottom && im.y_top <= fig.y_top)
                .unwrap_or_else(|| panic!("figure {fi} must contain a raster"));
            let svg = fig.composite_svg(&[Raster {
                placeholder: None,
                href: "IMG",
                rect: (im.x_left, im.x_right, im.y_bottom, im.y_top),
                ctm: im.ctm,
                clip: im.clip,
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
        assert!(page_resource_chain(&test_adapter(&doc), bare).is_empty(), "page 1 must reach no /Resources at all");
        assert!(!page_resource_chain(&test_adapter(&doc), with_res).is_empty(), "page 2 is the control");

        for (page_id, label) in [(bare, "no /Resources"), (with_res, "/Resources << >>")] {
            let painted = walk_page_bare(&doc, page_id);
            assert_eq!(painted.len(), 9, "{label}: eight filled bars + the trend stroke must paint");
            // Nothing supplies an /ExtGState, so the spec defaults must hold: fully opaque.
            assert!(
                painted.iter().all(|p| p.fill_op == 1.0 && p.stroke_op == 1.0),
                "{label}: a page with no /ExtGState paints at full opacity"
            );
            let strong = size_bar_figures(&doc, page_id);
            assert_eq!(strong.len(), 1, "{label}: the bars must reach the render as one figure");
        }
    }

    /// `tests/gen_fixtures.py::gen_separation` — three pages of spot-colour fills, one per
    /// path through the tint evaluator. Returns the ink of page `n`'s single figure.
    fn separation_ink(n: u32) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/separation.pdf");
        let doc = Document::load(path).expect("separation.pdf fixture must load");
        let page_id = *doc.get_pages().get(&n).unwrap_or_else(|| panic!("fixture has page {n}"));
        let strong = size_bar_figures(&doc, page_id);
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
        let strong = size_bar_figures(&doc, page_id);
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

    #[test]
    fn a_forms_bbox_clips_the_ink_it_paints_outside_it() {
        // `tests/gen_fixtures.py::gen_form_bbox`. PDF 32000-1 §8.10.2 makes a form's `/BBox` a
        // CLIP on its content; nothing in the crate read the key, so a form that deliberately
        // overflows its box had that overflow emitted as figure ink — which then fed the
        // cluster bbox and the viewBox. Corpus repro: `attention_1706.03762` p13, eight opaque
        // tab10 swatches painted above a `/BBox` that ends below them, where the source page
        // shows blank paper.
        //
        // Two forms, byte-identical content, differing ONLY in `/BBox`, both invoked through a
        // non-identity `/Matrix` so the box goes through the transform and not just the clip.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_bbox.pdf");
        let doc = Document::load(path).expect("form_bbox.pdf fixture must load");
        let page_id = *doc.get_pages().values().next().expect("fixture has a page");
        let mut strong = size_bar_figures(&doc, page_id);
        strong.sort_by(|a, b| b.y_top.partial_cmp(&a.y_top).expect("finite"));
        assert_eq!(strong.len(), 2, "both forms must cluster into a figure");
        let (clipped, control) = (&strong[0], &strong[1]);

        let svg = clipped.svg();
        assert!(svg.contains("#3366cc"), "the in-box fill must survive: {svg}");
        assert!(!svg.contains("#cc3333"), "ink outside the /BBox must not be emitted: {svg}");
        // The figure's extent is the BBox mapped through /Matrix 1.5 at (72, 500) — it must not
        // stretch to the off-box bars at x 420..492 / y 710..755.
        for (got, want, what) in [
            (clipped.x_left, 72.0, "x_left"),
            (clipped.y_bottom, 500.0, "y_bottom"),
            (clipped.x_right, 372.0, "x_right"),
            (clipped.y_top, 680.0, "y_top"),
        ] {
            assert!((got - want).abs() < 1.5, "{what} {got} (want {want})");
        }
        // The control's box contains every mark, so nothing crops and NO mask is emitted —
        // the ubiquitous full-page `/BBox` must stay free.
        let ctl = control.svg();
        assert!(ctl.contains("#cc3333"), "a BBox that contains its ink clips nothing: {ctl}");
        assert!(!ctl.contains("clip-path"), "a non-cropping /BBox must emit no mask: {ctl}");
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
            assert_eq!(crate::pdfobj::page_rotation(&test_adapter(&doc), page_id), rot);
            let strong = size_bar_figures(&doc, page_id);
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
            let rot = crate::pdfobj::page_rotation(&test_adapter(&doc), page_id);
            let spans = crate::text::extract_spans(&crate::access::test_adapter_with_source(&doc, &raw), page_id).unwrap();
            // The premise, asserted not assumed: the two labels really are drawn at 0° and
            // +90° in PAGE space, identically on every page.
            for (t, a) in [("Alpha", 0.0f32), ("Beta", std::f32::consts::FRAC_PI_2)] {
                let s = spans.iter().find(|s| s.text.contains(t)).unwrap_or_else(|| panic!("/Rotate {rot}: span {t} missing"));
                assert!((s.angle - a).abs() < 0.01, "/Rotate {rot}: {t} page angle {} want {a}", s.angle);
            }
            let mut strong = size_bar_figures(&doc, page_id);
            let labels: Vec<LabelSpan> = spans
                .iter()
                .map(|s| LabelSpan { x: s.x, y: s.y, size: s.size, width: s.width, text: s.text.clone(), bold: s.bold, italic: s.italic, angle: s.angle })
                .collect();
            attach_labels(&mut strong, &labels, &vec![false; labels.len()]);
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
        let strong = size_bar_figures(&doc, ids[0]);
        let images = crate::img::positioned_images(&test_adapter(&doc), ids[0], true);
        assert_eq!(images.len(), 1, "one raster per page");
        assert!(images[0].ctm.is_none(), "the fixture's placement is axis-aligned");
        fn raster(im: &crate::img::Placed) -> Raster<'_> {
            Raster { href: "IMG", placeholder: None, rect: (im.x_left, im.x_right, im.y_bottom, im.y_top), ctm: im.ctm, clip: im.clip, seq: &im.seq }
        }
        let up = strong[0].composite_svg(&[raster(&images[0])]);
        assert!(up.contains("<image href=\"IMG\" x=\"20\" y=\"50\" width=\"40\" height=\"30\""), "upright: {up}");
        assert!(!up.contains("matrix"), "upright must keep the plain rect form: {up}");

        // /Rotate 90: the raster's 40x30 pt rect stands up as 30x40 at local (220, 20). The
        // PIXELS are turned by `img::turn_pixels` and the placement matrix describes that
        // turned unit square (`img::turned_placement` composes `[0 30 -40 0 160 420]`), so the
        // matrix that comes out here is a plain axis-aligned box — the turn is in the samples,
        // not in the transform. (Before the raster path turned its own pixels this was
        // `matrix(0 40 -30 0 250 20)`: the same box with the rotation baked into the matrix,
        // which was right for a composite and wrong for the `<img>` sharing the same URI.)
        let strong = size_bar_figures(&doc, ids[1]);
        let images = crate::img::positioned_images(&test_adapter(&doc), ids[1], true);
        assert!(images[0].ctm.is_some(), "a turned raster must carry the matrix for its turned unit square");
        let turned = strong[0].composite_svg(&[raster(&images[0])]);
        assert!(turned.contains("transform=\"matrix(30 0 0 40 220 20)\""), "/Rotate 90: {turned}");
    }

    #[test]
    fn a_callout_panel_yields_the_table_it_boxes_and_keeps_its_own_text() {
        // `tests/gen_fixtures.py::gen_panel_table`. A shaded callout that clears the figure
        // bar reproduced, as SVG `<text>`, the cells of the real table drawn inside it —
        // while that table was ALSO emitted as `<table>`, so the reader saw every number
        // twice and the SVG copy was the lossy one (`geology_usgs_fs` p3 kept `Perchlorate`
        // and dropped `Radon-222`). Before the fix this fixture emits one `<table>`, one
        // `<svg>`, and every cell exactly TWICE — verified.
        //
        // The panel's own heading must survive: the answer is not "a figure holding a table
        // emits nothing", it is "a figure whose text is almost entirely a table's yields
        // that text and keeps the rest".
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/panel_table.pdf");
        let doc = Document::load(path).expect("panel_table.pdf fixture must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let html = crate::html::to_html(&crate::access::test_adapter_with_source(&doc, &raw), crate::html::Mode::Page, true, true);
        assert_eq!(html.matches("<table").count(), 1, "the ruled grid is still a table");
        assert_eq!(html.matches("<svg").count(), 1, "the panel is still a figure");
        for cell in ["Constituent", "Arsenic", "Federal MCL", "10 ppb", "Boron", "Federal HAL", "Radon-222", "Proposed MCL", "4,000 pCi"] {
            assert_eq!(html.matches(cell).count(), 1, "{cell:?} appears {} time(s), not once", html.matches(cell).count());
        }
        let svg = html.split("<svg").nth(1).and_then(|s| s.split("</svg>").next()).expect("the figure's svg");
        assert!(svg.contains("Benchmarks"), "the panel keeps its OWN heading: {svg}");
        assert!(!svg.contains("Radon-222"), "and none of the table's cells: {svg}");
    }

    #[test]
    fn a_css_overlay_registers_with_the_display_oriented_image_it_sits_on() {
        // `overlay_svg` used to be the ONE renderer left in page orientation, wrapped in an
        // `un_rotate` matrix, because `html.rs` laid it over an `<img>` the raster path emitted
        // UNTURNED. That `<img>` no longer exists — `img::turn_pixels` turns the samples — so
        // the overlay, its viewBox and the CSS box that carries it are all in display
        // orientation, and the wrapper is gone. Upright output is byte-identical either way.
        let (doc, ids) = rotated_pages();
        let upright = size_bar_figures(&doc, ids[0]);
        let up = upright[0].overlay_svg("width:100%");
        assert!(!up.contains("<g transform"), "no renderer needs an un-turn any more: {up}");
        assert!(up.contains("viewBox=\"-1 -1 202 302\""), "upright viewBox: {up}");
        // A quarter turn transposes the box; a half turn leaves it.
        for (i, vb) in [(1, "-1 -1 302 202"), (2, "-1 -1 202 302"), (3, "-1 -1 302 202")] {
            let figs = size_bar_figures(&doc, ids[i]);
            let ov = figs[0].overlay_svg("width:100%");
            assert!(!ov.contains("<g transform"), "page {}: {ov}", i + 1);
            assert!(ov.contains(&format!("viewBox=\"{vb}\"")), "page {}: want {vb}, got {ov}", i + 1);
        }
        // And the CSS box is measured in the same orientation. The figure is x 100..300,
        // y 200..500; place it over a raster whose rect is exactly the page, 400x600.
        let page = (0.0, 400.0, 0.0, 600.0);
        assert_eq!(upright[0].overlay_style(page), "position:absolute;left:25.00%;top:16.67%;width:50.00%;height:50.00%");
        let r90 = size_bar_figures(&doc, ids[1]);
        // /Rotate 90 displays the page 600x400; the figure's local origin is (y0, x0) = (200, 100)
        // and its extents transpose to 300x200.
        assert_eq!(r90[0].overlay_style(page), "position:absolute;left:33.33%;top:25.00%;width:50.00%;height:50.00%");
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
            &test_adapter(doc),
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
