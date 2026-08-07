//! Form-XObject descent: the mechanics every content walk shares.
//!
//! Four walks in this crate descend `/Subtype /Form` XObjects — `img::walk` (raster
//! placement), `vector::walk` (path ink), `text::decode_spans` (glyph runs) and
//! `extract::walk_drawn` (which images a page actually paints). Their operator loops are
//! four different state machines and stay separate on purpose. What they had no business
//! reimplementing four times is the *descent*: resolve the `Do` name in the scope in
//! force, check the subtype, parse `/Matrix`, build the child resource scope, decode the
//! form's content stream, and charge the recursion caps. Every axis of that list had
//! already drifted between the copies — one decoded unfiltered form streams and another
//! silently read them as empty, the depth cap was spelled `>=` in one file, `>` in two and
//! `<` in a fourth, and none of them followed an indirect `/Matrix`. Those three are
//! settled here: one decode, one comparison ([`too_deep`]), one dereferencing read.
//!
//! **What is deliberately NOT unified.** The resource-inheritance *choice* is a real
//! semantic disagreement, not drift: `img`/`vector`/`extract` overlay a form's own
//! `/Resources` on the scope they inherited, while `text` uses the form's own dictionary
//! ALONE, because PDF 32000-1 §8.10.2 says a form's fonts live in its own `/Resources` and
//! resolving `/F1` against the invoking form's font map would decode glyphs through the
//! wrong encoding. That choice is expressed as [`ScopePolicy`] — a typed parameter at the
//! call site — so both stances are visible and neither can be "fixed" into the other by
//! accident.
//!
//! **Invariants callers may rely on and must not re-check:**
//! - **Termination.** [`descend_form`] refuses a descent at or beyond
//!   [`crate::MAX_FORM_DEPTH`] ([`too_deep`] — the one comparison, previously spelled four
//!   different ways) and charges [`crate::WalkBudget`] *before* the descent happens, so a
//!   self-referential form terminates on the budget long before the depth cap.
//!   [`Descend::Halt`] means the budget is gone and the walk must stop; a caller that
//!   treats it as "skip this operator" reopens the DoS.
//! - **No dedupe.** A form legitimately drawn three times descends three times. The
//!   renderers must repaint it (`tests/fixtures_pdf/adversarial/form_repeat.pdf`); only a
//!   *collector* may keep a visited set, which is why `extract::walk_drawn` composes
//!   [`form_scope`] + [`form_ops`] itself instead of calling [`descend_form`] — see
//!   [`crate::WalkBudget`].
//! - **Degrade, never fabricate.** A malformed form (bad subtype, undecodable content, no
//!   `/Resources` under [`ScopePolicy::OwnOnly`]) yields [`Descend::Skip`]: the walk
//!   continues with the operators around it. Nothing here panics.

use crate::access::{read_resolved, DictionaryHandle, DocumentAccess, StreamHandle};
use crate::geom::Mat;
use crate::pdfobj::{content_bytes, num};
use lopdf::content::Operation;
use lopdf::{Dictionary, Object, ObjectId};
use std::collections::HashMap;

/// A resource scope: XObject name → object id, as an unqualified `/Im0` resolves at this
/// point in the walk.
pub(crate) type XMap = HashMap<Vec<u8>, ObjectId>;

#[cfg(test)]
pub(crate) fn xobjects_of(access: &dyn DocumentAccess, resources: &Dictionary) -> XMap {
    let mut map = XMap::new();
    overlay_xobjects(access, resources, &mut map);
    map
}

/// **Where** a paint happened in the page's content *tree*: the index of the operation in
/// the page's content stream, then — for ink inside a Form XObject — the index of the
/// operation inside that form, and so on down. Comparing two addresses lexicographically
/// (the derived `Ord`) gives exactly the order the PDF painted them, which is the order a
/// viewer composites them in: later paint covers earlier paint.
///
/// This exists because `img::walk` (rasters) and `vector::walk` (path ink) are two
/// separate walks of the same page, and compositing them into one `<svg>`
/// ([`crate::vector::PlacedSvg::composite_svg`]) has to interleave their output by paint
/// order. A running per-walk counter cannot do that: the two walks skip different
/// operators, descend on different conditions, and charge [`crate::WalkBudget`] at
/// different rates, so their counters drift apart at the first form. An address is
/// derived from the operation indices alone, so it is meaningful **without reference to
/// the other walk** — the only property that makes the two orderings comparable.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PaintSeq(Vec<u32>);

impl PaintSeq {
    /// The address of operation `i` of the stream whose own address is `here` (the page's
    /// content stream is the empty address).
    pub(crate) fn at(here: &[u32], i: usize) -> Self {
        let mut v = Vec::with_capacity(here.len() + 1);
        v.extend_from_slice(here);
        v.push(i as u32);
        PaintSeq(v)
    }

    /// This address as the `here` of the stream it invokes.
    pub(crate) fn as_slice(&self) -> &[u32] {
        &self.0
    }
}

/// Where a descending form's resource names resolve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ScopePolicy {
    /// The form's own `/Resources` overlaid on the scope in force at the `Do`, so a name
    /// the form does not define still resolves outward. What the raster, vector and
    /// drawn-set walks use: a producer that hoists shared XObjects onto the page and
    /// invokes them from inside a template form is common enough that the strict reading
    /// loses real ink.
    OverlayParent,
    /// The form's own `/Resources` ALONE — a form with none is not descended at all.
    /// PDF 32000-1 §8.10.2: a form's resources are its own; the text walk relies on that,
    /// because resolving `/F1` against an outer font map decodes glyphs through the wrong
    /// encoding and emits confident nonsense rather than nothing.
    OwnOnly,
}

/// The resource scope a form's content runs in.
pub(crate) struct FormScope {
    /// XObject names as they resolve inside the form (per [`ScopePolicy`]).
    pub xobjects: XMap,
    /// The form's own `/Resources`, when it has one — the dictionary a caller needs to
    /// build the resource kinds this module does not model (fonts, ExtGState).
    pub resources: Option<DictionaryHandle>,
}

/// Everything a walker needs to run a form's content, in one piece.
pub(crate) struct FormDescent {
    /// The form's decoded operator list.
    pub ops: Vec<Operation>,
    /// The scope its names resolve in.
    pub scope: FormScope,
    /// Its `/Matrix`, identity when absent or malformed. Compose it with the CTM in force
    /// at the `Do` (`matrix.mul(ctm)`) — this module does not know the caller's state.
    pub matrix: Mat,
}

/// The outcome of a `Do` on a Form XObject.
pub(crate) enum Descend {
    /// Run this form's content.
    Into(Box<FormDescent>),
    /// Not a descent — wrong subtype, the depth cap, undecodable content, or no
    /// `/Resources` under [`ScopePolicy::OwnOnly`]. Skip the operator, keep walking.
    Skip,
    /// The shared work budget is exhausted. The walk must **stop** and return what it has
    /// (pages degrade, they do not vanish). Continuing would reopen the form-bomb DoS.
    Halt,
}

/// Overlay one resource dictionary's `/XObject` entries onto a name → id map. Later
/// overlays win, so a nearer scope (a form's own resources, the page's own dictionary)
/// shadows an outer one — the precedence a renderer applies.
pub(crate) fn overlay_xobjects(access: &dyn DocumentAccess, resources: &Dictionary, map: &mut XMap) {
    let Ok(value) = resources.get(b"XObject") else {
        return;
    };
    let _ = read_resolved(access, value, |resolved| {
        let Ok(xobjects) = resolved.as_dict() else {
            return;
        };
        for (name, value) in xobjects.iter() {
            if let Ok(id) = value.as_reference() {
                map.insert(name.clone(), id);
            }
        }
    });
}

/// Every resource dictionary that governs a page, in **overlay order**: the outermost
/// ancestor first, the page's own dictionary last, so a later overlay shadows an earlier
/// one. Empty when the page has no `/Resources` anywhere in its tree (or the `/Parent`
/// chain is cyclic, which lopdf reports as an error).
///
/// `/Resources` is an inheritable attribute (§7.7.3.4) and the strict reading takes the
/// *nearest* ancestor's dictionary whole. Real files break that: producers that assemble
/// pages from templates split what a page draws across two levels of the page tree, and a
/// nearest-only read makes those `Do`s resolve to nothing and the ink disappear. Folding
/// the whole chain can only ever ADD names — the nearest dictionary is applied last and
/// still wins every name it defines. `extract::drawn_images` has read the chain this way
/// since it was written; this is the three interpreters catching up.
pub(crate) fn page_resource_chain(access: &dyn DocumentAccess, page_id: ObjectId) -> Vec<DictionaryHandle> {
    access.page_resource_chain(page_id).unwrap_or_default()
}

/// The XObjects a page can `Do`, resolved over its whole resource chain.
pub(crate) fn page_xobjects(access: &dyn DocumentAccess, page_id: ObjectId) -> XMap {
    let mut map = XMap::new();
    for res in page_resource_chain(access, page_id) {
        let _ = res.read(|dictionary| overlay_xobjects(access, dictionary, &mut map));
    }
    map
}

/// Overlay one resource dictionary onto another, merging the per-kind sub-dictionaries
/// **one level deep**: `/ColorSpace`, `/Font`, `/XObject` and friends are name → resource
/// maps, so an inner dictionary that defines `/CS1` must not delete the `/CS0` an outer one
/// defined. Within a kind, the inner name wins — the same nearest-wins precedence
/// [`overlay_xobjects`] applies.
///
/// The sub-dictionaries are dereferenced on the way in (they are usually written as
/// indirect objects) so the merged result is self-contained: a consumer can read it with a
/// plain `get`, and it stays valid after the scope that produced it is gone.
pub(crate) fn overlay_resources(access: &dyn DocumentAccess, base: &mut Dictionary, inner: &Dictionary) {
    for (key, val) in inner.iter() {
        let Some(kind) = read_resolved(access, val, |o| o.as_dict().ok().cloned()).ok().flatten() else {
            base.set(key.clone(), val.clone()); // /ProcSet and friends: not a name map
            continue;
        };
        match base.get(key).ok().and_then(|o| o.as_dict().ok()).cloned() {
            Some(mut merged) => {
                for (name, res) in kind.iter() {
                    merged.set(name.clone(), res.clone());
                }
                base.set(key.clone(), Object::Dictionary(merged));
            }
            None => base.set(key.clone(), Object::Dictionary(kind.clone())),
        }
    }
}

/// A page's effective resource dictionary: its whole [`page_resource_chain`] folded
/// outermost-first through [`overlay_resources`], so every name the page can name resolves
/// in one dictionary.
///
/// This is what a `/ColorSpace /CS0` on one of the page's images has to be looked up in
/// (§8.6.3) — the name alone describes nothing.
pub(crate) fn page_resources(access: &dyn DocumentAccess, page_id: ObjectId) -> Dictionary {
    let mut out = Dictionary::new();
    for res in page_resource_chain(access, page_id) {
        let _ = res.read(|dictionary| overlay_resources(access, &mut out, dictionary));
    }
    out
}

/// Resolve a `Do` operand to the XObject stream it names, in the scope in force.
/// `None` when the operand is not a name, the name is not in scope (a dangling reference
/// — nothing to draw), or the object is not a stream.
pub(crate) fn xobject_at(
    access: &dyn DocumentAccess,
    scope: &XMap,
    operands: &[Object],
) -> Option<(ObjectId, StreamHandle)> {
    let name = operands.first()?.as_name().ok()?;
    let id = *scope.get(name)?;
    Some((id, access.stream(id).ok()?))
}

/// Whether a stream has the requested `/Subtype` name.
pub(crate) fn has_subtype(stream: &lopdf::Stream, expected: &[u8]) -> bool {
    stream
        .dict
        .get(b"Subtype")
        .and_then(|object| object.as_name())
        .is_ok_and(|name| name == expected)
}

/// Is this form XObject a **transparency group** (`/Group << /S /Transparency >>`,
/// §11.6.6)?
///
/// The distinction decides what the constant alpha in force at the `Do` means. For an
/// ordinary form the graphics state is simply inherited, and a `gs` inside it legitimately
/// changes the alpha. For a transparency group the alpha at the `Do` applies to the group's
/// **composited result**, and the group's own initial state starts at `ca`/`CA` = 1.0
/// (§11.4.7.2) — so inheriting the alpha into the group instead lets the group's first
/// `gs` overwrite it, silently discarding the caller's transparency.
pub(crate) fn is_transparency_group(access: &dyn DocumentAccess, stream: &lopdf::Stream) -> bool {
    let Some(group) = stream.dict.get(b"Group").ok().and_then(|value| {
        read_resolved(access, value, |o| o.as_dict().ok().cloned()).ok().flatten()
    }) else {
        return false;
    };
    group
        .get(b"S")
        .ok()
        .and_then(|value| read_resolved(access, value, |o| o.as_name().ok().map(<[u8]>::to_vec)).ok().flatten())
        .is_some_and(|name| name == b"Transparency")
}

/// A form XObject's `/BBox` as a page-space clipping rectangle under `ctm`, or `None` when
/// the key is absent or unusable.
///
/// PDF 32000-1 §8.10.2: `/BBox` is expressed in FORM space and **clips the form's content** —
/// ink outside the box does not paint. Nothing in the crate read the key at all, so a
/// producer that reuses one oversized template form and relies on `/BBox` to crop it
/// (matplotlib and several LaTeX figure pipelines do exactly this) had its off-box
/// scaffolding emitted as content, which then fed the figure bbox and the viewBox.
///
/// The corners are NORMALIZED first — the spec fixes no ordering — and the returned rect is
/// the axis-aligned box of the four mapped corners. Under a quarter-turn `ctm` that is exact;
/// under a skewed one it is the conservative reading, which can only ever clip LESS than a
/// conforming reader, never more.
///
/// Lives here, with `is_transparency_group`, because all three walks descend the same forms
/// and must read the key the same way.
pub(crate) fn form_bbox_clip(
    access: &dyn DocumentAccess,
    stream: &lopdf::Stream,
    ctm: Mat,
) -> Option<crate::geom::Rect> {
    let c = read_resolved(access, stream.dict.get(b"BBox").ok()?, |arr| {
        let v = arr.as_array().ok()?;
        if v.len() < 4 {
            return None;
        }
        Some(
            v.iter()
                .map(|o| read_resolved(access, o, num).unwrap_or(f32::NAN))
                .collect::<Vec<_>>(),
        )
    })
    .ok()??;
    if c.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let (bx0, bx1) = (c[0].min(c[2]), c[0].max(c[2]));
    let (by0, by1) = (c[1].min(c[3]), c[1].max(c[3]));
    let mut bb = crate::geom::Rect::EMPTY;
    for (u, w) in [(bx0, by0), (bx1, by0), (bx1, by1), (bx0, by1)] {
        let (px, py) = ctm.apply(u, w);
        bb.include(px, py);
    }
    (bb.width() > 0.0 && bb.height() > 0.0).then_some(bb)
}

/// What a `gs` operator's `/SMask` entry says about the soft mask (§11.6.5.2).
pub(crate) enum SoftMask {
    /// `/SMask /None` — the mask is cleared; whatever was in force stops applying.
    Cleared,
    /// A mask group we are prepared to bound: the `/G` form XObject, to be rendered under
    /// the CTM in force at the `gs` and its painted extent taken as the visible window.
    Group(StreamHandle),
}

/// Read the `/SMask` entry of an `/ExtGState` dictionary (§11.6.5.2 — a *soft mask*: the
/// `/G` form is rendered off-screen and its alpha, or its luminosity, multiplies everything
/// painted while the state is in force).
///
/// `None` means "this `gs` says nothing about the soft mask" — either the key is absent (the
/// state is unchanged) or the mask is one we decline to interpret. The two are folded on
/// purpose: a declined mask must leave the caller doing exactly what it did before, which is
/// not masking at all. Cropping to a mask we have misread is the one outcome worse than
/// ignoring it.
///
/// **What is declined, and why.** Callers approximate the mask by its group's painted
/// *extent* — correct for the hard-edged window this exists to fix, and an over-estimate
/// (clips less than a conforming reader, never more) for a soft one. Two constructions break
/// that direction, so both are refused:
/// - `/S /Luminosity` with a non-black `/BC`. The backdrop fills the group's box *outside*
///   its ink, and a light backdrop is opaque there — so the ink extent is not an upper bound
///   on what shows. Default `/BC` is black (§11.6.5.2), i.e. transparent, which is the case
///   the extent does bound.
/// - a `/TR` transfer function other than `/Identity`, which may map "no ink" to opaque.
pub(crate) fn soft_mask_of(access: &dyn DocumentAccess, gs: &Dictionary) -> Option<SoftMask> {
    read_resolved(access, gs.get(b"SMask").ok()?, |sm| {
        if let Ok(name) = sm.as_name() {
            return (name == b"None").then_some(SoftMask::Cleared);
        }
        let m = sm.as_dict().ok()?;
        let subtype = m.get(b"S").ok().and_then(|value| {
            read_resolved(access, value, |o| o.as_name().ok().map(|name| name.to_vec()))
                .ok()
                .flatten()
        });
        match subtype.as_deref() {
            Some(b"Alpha") => {}
            Some(b"Luminosity") => {
                let non_black = m
                    .get(b"BC")
                    .ok()
                    .and_then(|value| {
                        read_resolved(access, value, |o| {
                            o.as_array().ok().map(|values| {
                                values.iter().any(|component| {
                                    read_resolved(access, component, num).unwrap_or(0.0).abs() > 1e-3
                                })
                            })
                        })
                        .ok()
                        .flatten()
                    })
                    .unwrap_or(false);
                if non_black {
                    return None;
                }
            }
            _ => return None,
        }
        let non_identity = m
            .get(b"TR")
            .ok()
            .and_then(|value| {
                read_resolved(access, value, |o| o.as_name().ok().map(|name| name != b"Identity"))
                    .ok()
                    .flatten()
            })
            .unwrap_or(false);
        if non_identity {
            return None;
        }
        let id = m.get(b"G").ok()?.as_reference().ok()?;
        let stream = access.stream(id).ok()?;
        stream
            .read(|value| has_subtype(value, b"Form"))
            .is_some_and(|is_form| is_form)
            .then_some(SoftMask::Group(stream))
    })
    .ok()?
}

/// The one depth convention: a descent is refused **at** [`crate::MAX_FORM_DEPTH`], so a
/// page's own content (depth 0) may nest that many form levels below it.
///
/// Spelled once here because the four walkers previously spelled it `>=` (extract), `>`
/// (raster, vector) and `<` (text), which made the effective ceiling differ by one between
/// files for no reason anybody chose.
pub(crate) fn too_deep(depth: u32) -> bool {
    depth >= crate::MAX_FORM_DEPTH
}

/// The child resource scope for a form, per `policy`. `None` under
/// [`ScopePolicy::OwnOnly`] when the form has no `/Resources` of its own — there is
/// nothing its names could resolve against, so it is not descended.
pub(crate) fn form_scope(
    access: &dyn DocumentAccess,
    stream: &StreamHandle,
    parent: &XMap,
    policy: ScopePolicy,
) -> Option<FormScope> {
    let resources = stream.dictionary_entry(access, b"Resources").ok();
    match policy {
        ScopePolicy::OverlayParent => {
            let mut xobjects = parent.clone();
            if let Some(fr) = &resources {
                let _ = fr.read(|dictionary| overlay_xobjects(access, dictionary, &mut xobjects));
            }
            Some(FormScope { xobjects, resources })
        }
        ScopePolicy::OwnOnly => {
            let fr = resources?;
            let mut xobjects = XMap::new();
            let _ = fr.read(|dictionary| overlay_xobjects(access, dictionary, &mut xobjects));
            Some(FormScope {
                xobjects,
                resources: Some(fr),
            })
        }
    }
}

/// A form's decoded operator list. `None` when the content stream cannot be parsed.
///
/// Reads the stream through [`crate::pdfobj::content_bytes`], which keeps an **unfiltered**
/// form's bytes — `decompressed_content()` alone errors on a stream with no `/Filter`, and
/// two of these walkers used to hand the decoder zero bytes because of it.
pub(crate) fn form_ops(stream: &lopdf::Stream) -> Option<Vec<Operation>> {
    lopdf::content::Content::decode(&content_bytes(stream)).ok().map(|c| c.operations)
}

/// A form's `/Matrix`, identity when absent or malformed.
///
/// The array and its elements are read through `deref`/`num_deref`: `/Matrix` is a
/// dictionary value, not a content-stream operand, so any part of it may legally be an
/// indirect reference. A direct-only read turned `[1 0 0 1 0 5 0 R]` into a matrix with a
/// zero component — which collapses the whole form onto a point or a line, silently.
pub(crate) fn form_matrix(access: &dyn DocumentAccess, stream: &lopdf::Stream) -> Mat {
    stream
        .dict
        .get(b"Matrix")
        .ok()
        .and_then(|value| {
            read_resolved(access, value, |o| {
                o.as_array().ok().filter(|a| a.len() >= 6).map(|a| Mat {
                    a: read_resolved(access, &a[0], num).unwrap_or(0.0),
                    b: read_resolved(access, &a[1], num).unwrap_or(0.0),
                    c: read_resolved(access, &a[2], num).unwrap_or(0.0),
                    d: read_resolved(access, &a[3], num).unwrap_or(0.0),
                    e: read_resolved(access, &a[4], num).unwrap_or(0.0),
                    f: read_resolved(access, &a[5], num).unwrap_or(0.0),
                })
            })
            .ok()
            .flatten()
        })
        .unwrap_or(Mat::ID)
}

/// Is this annotation one a viewer does not draw? PDF 32000-1 §12.5.3 `/F` bit 2
/// (**Hidden**, value 2): "do not display the annotation … and do not print it". Its ink
/// is not on the page and its resources are not the page's.
///
/// Only Hidden is applied. `NoView` (bit 6) suppresses *screen* display but still prints,
/// so an appearance behind it is real page content; treating it as absent would drop
/// resources a printed page carries.
fn annotation_hidden(annot: &Dictionary) -> bool {
    annot.get(b"F").and_then(|o| o.as_i64()).unwrap_or(0) & 2 != 0
}

/// Push the appearance stream `val` resolves to, if it is one, with the annotation it
/// belongs to (whose `/Rect` places it — see [`appearance_ctm`]).
fn push_appearance(
    access: &dyn DocumentAccess,
    annot: &Dictionary,
    val: &Object,
    out: &mut Vec<Appearance>,
) {
    let Ok(id) = val.as_reference() else {
        return; // a stream is always an indirect object (§7.3.8); nothing else is one
    };
    if let Ok(stream) = access.stream(id) {
        out.push(Appearance { annotation: annot.clone(), id, stream });
    }
}

struct Appearance {
    annotation: Dictionary,
    id: ObjectId,
    stream: StreamHandle,
}

/// The **normal appearance streams** of a page's annotations: `/Annots` → `/AP` → `/N`.
///
/// An appearance stream is a Form XObject that is not reached from the page's content
/// stream or its `/Resources` at all — it hangs off the annotation. Nothing in this crate
/// used to walk it, so an image or a font that lived only inside a stamp's or a widget's
/// appearance was reported by nobody.
///
/// The selection rules, in the order a viewer applies them (§12.5.5):
/// - a **hidden** annotation ([`annotation_hidden`]) contributes nothing;
/// - `/N` may be the appearance stream itself, or a **sub-dictionary keyed by appearance
///   state** (a checkbox's `/Off` and `/Yes`, say). With `/AS` present exactly the named
///   state is current — and if `/AS` names a state the dictionary does not define, nothing
///   is displayed and nothing is returned. Without `/AS` no state is selected, so **every**
///   state is returned: this is a collector's enumeration ("what resources does this page
///   carry"), and a state-keyed appearance with no `/AS` is a malformed annotation whose
///   states are all equally plausible.
///
/// Ordering is `/Annots` array order, then `/N` sub-dictionary order — both are stable
/// document order, so the enumeration is deterministic.
///
/// **The appearance's resources are its own.** §12.5.5 gives the appearance stream the
/// semantics of a Form XObject whose `/Resources` are the ones it declares; it does not
/// inherit the page's. A consumer walking one uses [`ScopePolicy::OwnOnly`].
///
/// **Who consumes this.** `extract::extract_images` does: an image that exists only inside
/// a stamp's appearance is one of the page's images and nobody was reporting it.
/// `extract::extract_fonts` does not (parity verdict recorded at that function). The three
/// **render** walks (`img`, `vector`, `text`) consume [`placed_appearances`] instead — the
/// same enumeration under the `Current` state rule, carrying the §12.5.5 placement matrix an
/// enumerator has no use for.
pub(crate) fn appearance_streams(
    access: &dyn DocumentAccess,
    page_id: ObjectId,
) -> Vec<(ObjectId, StreamHandle)> {
    annot_appearances(access, page_id, StateRule::All)
        .into_iter()
        .map(|appearance| (appearance.id, appearance.stream))
        .collect()
}

/// Which appearance state a walk takes when `/N` is a state dictionary and `/AS` is absent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StateRule {
    /// **Every** state — what a *collector* wants: a state-keyed appearance with no `/AS` is
    /// a malformed annotation whose states are all equally plausible, and the question being
    /// asked is "what resources does this page carry".
    All,
    /// **No** state, i.e. nothing. What a *renderer* must do: §12.5.5 leaves the appearance
    /// undefined when no state is current, and painting all of them stacks a checkbox's
    /// `/Off` on top of its `/Yes` — ink the page does not have.
    Current,
}

/// The `/Annots` → `/AP` → `/N` walk both public enumerators share, with the annotation
/// dictionary each stream came from (the renderer needs its `/Rect`).
fn annot_appearances(
    access: &dyn DocumentAccess,
    page_id: ObjectId,
    rule: StateRule,
) -> Vec<Appearance> {
    let mut out = Vec::new();
    let Ok(page) = access.object(page_id) else {
        return out;
    };
    let _ = page.read(|page| {
        let Ok(page) = page.as_dict() else { return };
        let Ok(annots_value) = page.get(b"Annots") else { return };
        let _ = read_resolved(access, annots_value, |annots| {
            let Ok(annots) = annots.as_array() else { return };
            for annotation_value in annots {
                let _ = read_resolved(access, annotation_value, |annotation| {
                    let Ok(annotation) = annotation.as_dict() else { return };
                    if annotation_hidden(annotation) {
                        return;
                    }
                    let Ok(ap_value) = annotation.get(b"AP") else { return };
                    let _ = read_resolved(access, ap_value, |ap| {
                        let Ok(ap) = ap.as_dict() else { return };
                        let Ok(normal) = ap.get(b"N") else { return };
                        let _ = read_resolved(access, normal, |resolved| match resolved {
                            Object::Stream(_) => {
                                push_appearance(access, annotation, normal, &mut out)
                            }
                            Object::Dictionary(states) => match annotation
                                .get(b"AS")
                                .ok()
                                .and_then(|value| value.as_name().ok())
                            {
                                Some(state) => {
                                    if let Ok(value) = states.get(state) {
                                        push_appearance(access, annotation, value, &mut out);
                                    }
                                }
                                None if rule == StateRule::All => {
                                    for (_, value) in states.iter() {
                                        push_appearance(access, annotation, value, &mut out);
                                    }
                                }
                                None => {}
                            },
                            _ => {}
                        });
                    });
                });
            }
        });
    });
    out
}

/// The appearance streams a page's annotations **paint**, each with the matrix that puts it
/// where a viewer puts it — what the `img`, `vector` and `text` walks consume.
///
/// Same enumeration as [`appearance_streams`] under [`StateRule::Current`], plus the piece
/// that kept the render path out of annotations until now: the §12.5.5 mapping
/// ([`appearance_ctm`]). An appearance whose `/Rect` or `/BBox` cannot be read, or which
/// maps onto nothing, is skipped — ink in the wrong place is a worse answer than no ink.
///
/// The returned matrix is the CTM the appearance's **invocation** sits under, i.e. it is
/// composed with the form's own `/Matrix` by the same `f.matrix.mul(ctm)` every `Do` uses.
pub(crate) fn placed_appearances(
    access: &dyn DocumentAccess,
    page_id: ObjectId,
) -> Vec<(ObjectId, StreamHandle, Mat)> {
    annot_appearances(access, page_id, StateRule::Current)
        .into_iter()
        .filter_map(|appearance| {
            let matrix = appearance
                .stream
                .read(|stream| appearance_ctm(access, &appearance.annotation, stream))??;
            Some((appearance.id, appearance.stream, matrix))
        })
        .collect()
}

/// A page's annotations as `(object id, /Rect)` — the identity a structure tree's `/OBJR`
/// names, with the box it occupies.
///
/// A tagged fillable form files its data cells as `/OBJR` references to widget annotations
/// rather than as `/MCID`s: the cell's *value* is painted by the widget's appearance stream,
/// which has no marked content of its own. Without this mapping every such cell would resolve
/// to nothing and the whole declaration would be rejected — which is every IRS form in the
/// measurement corpus. Hidden annotations are excluded on the same rule as the render walks:
/// a viewer does not draw them, so they are not on the page.
pub(crate) fn annot_rects(
    access: &dyn DocumentAccess,
    page_id: ObjectId,
) -> Vec<(ObjectId, crate::geom::Rect)> {
    let mut out = Vec::new();
    let Ok(page) = access.object(page_id) else { return out };
    let _ = page.read(|page| {
        let Ok(page) = page.as_dict() else { return };
        let Ok(annots_value) = page.get(b"Annots") else { return };
        let _ = read_resolved(access, annots_value, |annots| {
            let Ok(annots) = annots.as_array() else { return };
            for annotation_value in annots {
                let Object::Reference(id) = annotation_value else { continue };
                let _ = read_resolved(access, annotation_value, |annotation| {
                    let Ok(annotation) = annotation.as_dict() else { return };
                    if !annotation_hidden(annotation) {
                        if let Some(rect) = rect_key(access, annotation, b"Rect") {
                            out.push((*id, rect));
                        }
                    }
                });
            }
        });
    });
    out
}

/// A 4-number rectangle from a dictionary key, normalized. Every element is dereferenced:
/// a `/Rect [344.9 456.1 348.9 5 0 R]` is legal and a direct-only read turns it into 0.
fn rect_key(access: &dyn DocumentAccess, d: &Dictionary, key: &[u8]) -> Option<crate::geom::Rect> {
    let v = read_resolved(access, d.get(key).ok()?, |array| {
        let array = array.as_array().ok()?;
        (array.len() >= 4).then(|| {
            array
                .iter()
                .take(4)
                .map(|value| read_resolved(access, value, num).unwrap_or(0.0))
                .collect::<Vec<_>>()
        })
    })
    .ok()??;
    Some(crate::geom::Rect::new(v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])))
}

/// PDF 32000-1 §12.5.5, the **appearance algorithm**: the matrix that maps an annotation's
/// appearance stream onto its `/Rect`.
///
/// This is the mapping whose absence kept the three render walks out of annotations. The
/// corpus stamp that surfaced the gap carries `/BBox [0 0 13.9 17.4]` against `/Rect [344.9
/// 456.1 348.9 461.1]`: painted at the form's own coordinates its image lands ~345 pt from
/// where a viewer puts it.
///
/// The algorithm, verbatim: transform the `/BBox` corners by the form's `/Matrix`; take the
/// axis-aligned box **A** of the result; compute the matrix that maps A onto `/Rect`
/// (translate + non-uniform scale); the appearance is then drawn under that matrix composed
/// with `/Matrix`. Returning the *first* factor alone is deliberate — every caller composes
/// `/Matrix` itself via [`descend_form`], so this must not pre-apply it.
///
/// `None` when either rectangle is missing or degenerate. A zero-extent `/Rect` is an
/// annotation with nowhere to paint, and a zero-extent transformed `/BBox` has no scale that
/// reaches one.
fn appearance_ctm(
    access: &dyn DocumentAccess,
    annot: &Dictionary,
    stream: &lopdf::Stream,
) -> Option<Mat> {
    let rect = rect_key(access, annot, b"Rect")?;
    let bbox = rect_key(access, &stream.dict, b"BBox")?;
    let m = form_matrix(access, stream);
    let mut a = crate::geom::Rect::EMPTY;
    for (u, v) in [(bbox.x0, bbox.y0), (bbox.x1, bbox.y0), (bbox.x1, bbox.y1), (bbox.x0, bbox.y1)] {
        let (x, y) = m.apply(u, v);
        a.include(x, y);
    }
    let (aw, ah) = (a.x1 - a.x0, a.y1 - a.y0);
    let (rw, rh) = (rect.x1 - rect.x0, rect.y1 - rect.y0);
    if aw <= 1e-6 || ah <= 1e-6 || rw <= 1e-6 || rh <= 1e-6 {
        return None;
    }
    let (sx, sy) = (rw / aw, rh / ah);
    Some(Mat { a: sx, b: 0.0, c: 0.0, d: sy, e: rect.x0 - a.x0 * sx, f: rect.y0 - a.y0 * sy })
}

/// Descend into a Form XObject: the whole mechanic, in the one order, with the caps
/// charged before any work is done.
///
/// `parent` is the scope in force at the `Do`, `depth` the current nesting level (a page's
/// own content is 0), and `sibling_cost` the size of any *other* per-scope map the caller
/// is about to clone (`vector`'s ExtGState map, `text`'s font
/// map) — the budget bills the clone the caller will perform, not just the one this
/// function performs.
///
/// The order is load-bearing: the budget is charged first, so a bomb pays for every branch
/// it attempts even when the branch turns out to be a dead end.
pub(crate) fn descend_form(
    access: &dyn DocumentAccess,
    stream: &StreamHandle,
    parent: &XMap,
    policy: ScopePolicy,
    depth: u32,
    budget: &mut crate::WalkBudget,
    sibling_cost: usize,
) -> Descend {
    if !stream.read(|stream| has_subtype(stream, b"Form")).unwrap_or(false) || too_deep(depth) {
        return Descend::Skip;
    }
    // Bill the descent before doing it: cloning the inherited resource maps and decoding
    // the form's content stream dwarf a single operator's cost, and a form bomb pays
    // exactly this per branch — which is what actually bounds the attack.
    if !budget.spend(crate::FORM_DESCENT_COST + parent.len() + sibling_cost) {
        return Descend::Halt;
    }
    let Some(scope) = form_scope(access, stream, parent, policy) else {
        return Descend::Skip;
    };
    let matrix = stream.read(|stream| form_matrix(access, stream)).unwrap_or(Mat::ID);
    match stream.read(form_ops).flatten() {
        Some(ops) => Descend::Into(Box::new(FormDescent { ops, scope, matrix })),
        None => Descend::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::test_adapter;
    use lopdf::{dictionary, Document, Stream};

    /// A one-object document holding a stream with `dict` and `content`.
    fn doc_with_form(dict: Dictionary, content: &[u8]) -> (Document, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let id = doc.add_object(Object::Stream(Stream::new(dict, content.to_vec())));
        (doc, id)
    }

    fn form_dict(extra: Dictionary) -> Dictionary {
        let mut d = dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()]
        };
        for (k, v) in extra.iter() {
            d.set(k.clone(), v.clone());
        }
        d
    }

    fn stream_of(doc: &Document, id: ObjectId) -> &Stream {
        doc.get_object(id).unwrap().as_stream().unwrap()
    }

    fn handle_of(doc: &Document, id: ObjectId) -> StreamHandle {
        test_adapter(doc).stream(id).unwrap()
    }

    #[test]
    fn an_unfiltered_form_stream_still_yields_its_operators() {
        // The defect two walkers shipped: `decompressed_content()` ERRORS on a stream with
        // no `/Filter`, so `unwrap_or_default()` handed the decoder zero bytes.
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"1 0 0 rg 0 0 10 10 re f");
        let ops = form_ops(stream_of(&doc, id)).expect("an unfiltered form decodes");
        assert_eq!(ops.len(), 3, "rg, re, f");
    }

    #[test]
    fn a_missing_or_malformed_matrix_is_the_identity() {
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"");
        let m = form_matrix(&test_adapter(&doc), stream_of(&doc, id));
        assert_eq!((m.a, m.b, m.c, m.d, m.e, m.f), (1.0, 0.0, 0.0, 1.0, 0.0, 0.0));

        let short = form_dict(dictionary! { "Matrix" => vec![2.into(), 0.into()] });
        let (doc2, id2) = doc_with_form(short, b"");
        assert_eq!(form_matrix(&test_adapter(&doc2), stream_of(&doc2, id2)).a, 1.0, "a too-short /Matrix is not read partially");
    }

    #[test]
    fn the_matrix_is_read_in_operand_order() {
        let d = form_dict(dictionary! { "Matrix" => vec![2.into(), 3.into(), 4.into(), 5.into(), 6.into(), 7.into()] });
        let (doc, id) = doc_with_form(d, b"");
        let m = form_matrix(&test_adapter(&doc), stream_of(&doc, id));
        assert_eq!((m.a, m.b, m.c, m.d, m.e, m.f), (2.0, 3.0, 4.0, 5.0, 6.0, 7.0));
    }

    #[test]
    fn the_matrix_resolves_indirect_parts_instead_of_reading_zero() {
        // `/Matrix` is a dictionary value, so any part of it may be a reference. Read
        // directly, `5 0 R` becomes 0.0 — a matrix that collapses the form onto a line.
        let mut doc = Document::with_version("1.5");
        let six = doc.add_object(Object::Real(6.0));
        let d = form_dict(dictionary! {
            "Matrix" => vec![2.into(), 0.into(), 0.into(), 2.into(), Object::Reference(six), 7.into()]
        });
        let id = doc.add_object(Object::Stream(Stream::new(d, b"".to_vec())));
        let m = form_matrix(&test_adapter(&doc), stream_of(&doc, id));
        assert_eq!((m.a, m.d, m.f), (2.0, 2.0, 7.0));
        assert_eq!(m.e, 6.0, "an INDIRECT /Matrix element used to read as 0.0");

        // The whole array written indirectly, which used to yield the identity.
        let mut doc = Document::with_version("1.5");
        let arr = doc.add_object(Object::Array(vec![3.into(), 0.into(), 0.into(), 3.into(), 0.into(), 0.into()]));
        let d = form_dict(dictionary! { "Matrix" => Object::Reference(arr) });
        let id = doc.add_object(Object::Stream(Stream::new(d, b"".to_vec())));
        assert_eq!(form_matrix(&test_adapter(&doc), stream_of(&doc, id)).a, 3.0);
    }

    #[test]
    fn the_depth_cap_is_spelled_once_and_refuses_at_the_limit() {
        assert!(!too_deep(0));
        assert!(!too_deep(crate::MAX_FORM_DEPTH - 1));
        assert!(too_deep(crate::MAX_FORM_DEPTH), "a descent is refused AT the cap, not one past it");
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"0 0 10 10 re f");
        let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
        let at_cap = descend_form(
            &test_adapter(&doc),
            &handle_of(&doc, id),
            &XMap::new(),
            ScopePolicy::OverlayParent,
            crate::MAX_FORM_DEPTH,
            &mut budget,
            0,
        );
        assert!(matches!(at_cap, Descend::Skip));
        let under = descend_form(
            &test_adapter(&doc),
            &handle_of(&doc, id),
            &XMap::new(),
            ScopePolicy::OverlayParent,
            crate::MAX_FORM_DEPTH - 1,
            &mut budget,
            0,
        );
        assert!(matches!(under, Descend::Into(_)), "one level under the cap still descends");
    }

    /// `annot_render.pdf`, page 1 — see `tests/gen_fixtures.py::gen_annot_render`.
    fn annot_render() -> (Document, ObjectId) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/annot_render.pdf");
        let doc = Document::load(path).expect("annot_render.pdf fixture must load");
        let page = *doc.get_pages().get(&1).expect("page 1");
        (doc, page)
    }

    #[test]
    fn an_annotation_appearance_paints_its_raster_vector_ink_and_text_onto_the_page() {
        // §12.5.5's appearance algorithm, which nothing in the crate computed: the render
        // walks ignored `/Annots` outright, so a stamp's content was invisible to `to_html`
        // while `extract_images` reported it. The fixture makes every factor of the mapping
        // different and none of them 1 — `/Matrix [2 0 0 2 0 0]` x `/BBox [0 0 50 50]` onto
        // `/Rect [200 400 400 550]` is a 4x horizontal and 3x VERTICAL scale off (200, 400)
        // — so nothing short of the full algorithm lands the ink where these assertions say.
        let (doc, page) = annot_render();

        // 1. the raster. Form (2,2)+10x10 -> page (208, 406) 40x30.
        let tiles = crate::img::positioned_images(&doc, &test_adapter(&doc), page, true);
        assert_eq!(tiles.len(), 1, "the page's only raster lives in the stamp's appearance");
        let t = &tiles[0];
        for (got, want, what) in [
            (t.x_left, 208.0, "x0"), (t.y_bottom, 406.0, "y0"),
            (t.x_right, 248.0, "x1"), (t.y_top, 436.0, "y1"),
        ] {
            assert!((got - want).abs() < 0.5, "raster {what}: {got} != {want}");
        }

        // 2. the vector ink — the appearance's panel and frame fill the whole `/Rect`.
        let (strong, _) = crate::vector::positioned_vectors(&doc, &test_adapter(&doc), page);
        let fig = strong.iter().find(|f| f.x_left < 205.0 && f.x_right > 395.0).unwrap_or_else(|| {
            panic!("no figure spans the stamp's /Rect; got {:?}", strong.iter().map(|f| (f.x_left, f.x_right)).collect::<Vec<_>>())
        });
        assert!((fig.y_bottom - 400.0).abs() < 2.0 && (fig.y_top - 550.0).abs() < 2.0, "figure y {} .. {}", fig.y_bottom, fig.y_top);

        // 3. the text. Form (2,40) at 4 pt -> page (208, 520) at 12 pt (the vertical scale).
        let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/annot_render.pdf")).expect("readable");
        let spans = crate::text::extract_spans(&doc, &test_adapter(&doc), page, &raw);
        let label = spans.iter().find(|s| s.text.contains("StampInkVisible")).expect("the stamp's text must reach the span set");
        assert!((label.x - 208.0).abs() < 1.0 && (label.y - 520.0).abs() < 1.0, "label at ({}, {})", label.x, label.y);
        assert!((label.size - 12.0).abs() < 0.5, "label size {} — the /Rect's VERTICAL scale", label.size);

        // 4. the selection rules a renderer must apply, and the one it must not borrow from
        // the collector: no `/AS` over a state dictionary means no state is current.
        let all: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(all.contains("SelectedStateOn"), "/AS names the state that paints");
        for absent in ["HiddenStampMustNotPaint", "UnselectedStateOff", "NoStateSelected"] {
            assert!(!all.contains(absent), "{absent} must never reach the page");
        }
    }

    #[test]
    fn a_collector_takes_every_unselected_state_where_a_renderer_takes_none() {
        // The one place the two enumerations disagree, kept honest in both directions:
        // `annot_render.pdf`'s fourth annotation is a state dictionary with no `/AS`.
        let (doc, page) = annot_render();
        let collected = appearance_streams(&test_adapter(&doc), page).len();
        let rendered = placed_appearances(&test_adapter(&doc), page).len();
        assert_eq!(collected, 3, "a collector takes the stamp, the /AS state and the state with no /AS");
        assert_eq!(rendered, 2, "a renderer draws only the stamp and the /AS-selected state");
    }

    #[test]
    fn the_two_scope_policies_disagree_on_purpose() {
        let mut doc = Document::with_version("1.5");
        let own_im = doc.add_object(Object::Null);
        let res = doc.add_object(Object::Dictionary(dictionary! {
            "XObject" => dictionary! { "Own" => Object::Reference(own_im) }
        }));
        let d = form_dict(dictionary! { "Resources" => Object::Reference(res) });
        let id = doc.add_object(Object::Stream(Stream::new(d, b"".to_vec())));
        let mut parent = XMap::new();
        parent.insert(b"Outer".to_vec(), (9, 0));

        let overlay = form_scope(&test_adapter(&doc), &handle_of(&doc, id), &parent, ScopePolicy::OverlayParent).expect("overlay scope");
        assert!(overlay.xobjects.contains_key(b"Outer".as_slice()), "OverlayParent keeps the inherited name");
        assert!(overlay.xobjects.contains_key(b"Own".as_slice()));

        let own = form_scope(&test_adapter(&doc), &handle_of(&doc, id), &parent, ScopePolicy::OwnOnly).expect("own scope");
        assert!(!own.xobjects.contains_key(b"Outer".as_slice()), "OwnOnly must NOT see the invoking scope");
        assert!(own.xobjects.contains_key(b"Own".as_slice()));
    }

    #[test]
    fn a_nearer_scope_shadows_an_outer_one_of_the_same_name() {
        let mut doc = Document::with_version("1.5");
        let inner = doc.add_object(Object::Null);
        let res = doc.add_object(Object::Dictionary(dictionary! {
            "XObject" => dictionary! { "Im0" => Object::Reference(inner) }
        }));
        let d = form_dict(dictionary! { "Resources" => Object::Reference(res) });
        let id = doc.add_object(Object::Stream(Stream::new(d, b"".to_vec())));
        let mut parent = XMap::new();
        parent.insert(b"Im0".to_vec(), (99, 0));
        let scope = form_scope(&test_adapter(&doc), &handle_of(&doc, id), &parent, ScopePolicy::OverlayParent).expect("scope");
        assert_eq!(scope.xobjects[b"Im0".as_slice()], inner, "the form's own /Im0 wins");
    }

    #[test]
    fn own_only_refuses_a_form_that_carries_no_resources() {
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"");
        let parent = XMap::new();
        assert!(form_scope(&test_adapter(&doc), &handle_of(&doc, id), &parent, ScopePolicy::OwnOnly).is_none());
        assert!(
            form_scope(&test_adapter(&doc), &handle_of(&doc, id), &parent, ScopePolicy::OverlayParent).is_some(),
            "OverlayParent still descends: the inherited scope is what its names resolve against"
        );
    }

    #[test]
    fn an_indirect_resources_dictionary_is_followed() {
        // `/Resources 5 0 R` is the common spelling; reading it with `as_dict()` alone
        // loses the whole scope.
        let mut doc = Document::with_version("1.5");
        let im = doc.add_object(Object::Null);
        let res = doc.add_object(Object::Dictionary(dictionary! {
            "XObject" => dictionary! { "Im0" => Object::Reference(im) }
        }));
        let d = form_dict(dictionary! { "Resources" => Object::Reference(res) });
        let id = doc.add_object(Object::Stream(Stream::new(d, b"".to_vec())));
        let scope = form_scope(&test_adapter(&doc), &handle_of(&doc, id), &XMap::new(), ScopePolicy::OwnOnly).expect("scope");
        assert_eq!(scope.xobjects[b"Im0".as_slice()], im);
    }

    #[test]
    fn a_non_form_xobject_is_never_a_descent() {
        let d = dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 4, "Height" => 4 };
        let (doc, id) = doc_with_form(d, b"junk");
        let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
        assert!(matches!(
            descend_form(&test_adapter(&doc), &handle_of(&doc, id), &XMap::new(), ScopePolicy::OverlayParent, 0, &mut budget, 0),
            Descend::Skip
        ));
    }

    #[test]
    fn an_exhausted_budget_halts_rather_than_skips() {
        // The distinction the DoS fix rests on: `Skip` keeps walking, `Halt` must stop the
        // walk. A caller that confuses the two lets a form bomb run again.
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"0 0 10 10 re f");
        let mut budget = crate::WalkBudget::new(10); // less than FORM_DESCENT_COST
        assert!(matches!(
            descend_form(&test_adapter(&doc), &handle_of(&doc, id), &XMap::new(), ScopePolicy::OverlayParent, 0, &mut budget, 0),
            Descend::Halt
        ));
    }

    #[test]
    fn the_descent_is_billed_for_the_maps_the_caller_will_clone() {
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"");
        let mut parent = XMap::new();
        for i in 0..7u32 {
            parent.insert(vec![i as u8], (i, 0));
        }
        // Exactly enough for one descent charged FORM_DESCENT_COST + 7 (scope) + 3 (sibling).
        let mut budget = crate::WalkBudget::new(crate::FORM_DESCENT_COST + 10);
        assert!(matches!(
            descend_form(&test_adapter(&doc), &handle_of(&doc, id), &parent, ScopePolicy::OverlayParent, 0, &mut budget, 3),
            Descend::Into(_)
        ));
        assert!(!budget.spend(1), "the descent must have charged the sibling map too");
    }

    #[test]
    fn a_repeated_form_descends_every_time_it_is_invoked() {
        // The no-dedupe contract, at the helper: a template stamped three times paints
        // three times. `descend_form` holds no visited set, by design.
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"0 0 10 10 re f");
        let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
        for _ in 0..3 {
            assert!(matches!(
                descend_form(&test_adapter(&doc), &handle_of(&doc, id), &XMap::new(), ScopePolicy::OverlayParent, 0, &mut budget, 0),
                Descend::Into(_)
            ));
        }
    }

    /// A page carrying `annots`, whose ids are already in `doc`.
    fn page_with_annots(doc: &mut Document, annots: Vec<Object>) -> ObjectId {
        doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page", "Annots" => Object::Array(annots)
        }))
    }

    /// An appearance stream (a Form XObject) whose content is `content`.
    fn ap_stream(doc: &mut Document, content: &[u8]) -> ObjectId {
        doc.add_object(Object::Stream(Stream::new(form_dict(Dictionary::new()), content.to_vec())))
    }

    #[test]
    fn an_annotations_appearance_stream_is_found() {
        let mut doc = Document::with_version("1.5");
        let ap = ap_stream(&mut doc, b"/Im0 Do");
        let annot = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Stamp",
            "AP" => dictionary! { "N" => Object::Reference(ap) }
        }));
        let page = page_with_annots(&mut doc, vec![Object::Reference(annot)]);
        let found = appearance_streams(&test_adapter(&doc), page);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, ap, "the /AP /N stream nothing used to walk");
    }

    #[test]
    fn a_hidden_annotation_contributes_no_appearance() {
        let mut doc = Document::with_version("1.5");
        let ap = ap_stream(&mut doc, b"/Im0 Do");
        // /F bit 2 = Hidden: not displayed, not printed.
        let annot = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Stamp", "F" => 2,
            "AP" => dictionary! { "N" => Object::Reference(ap) }
        }));
        let page = page_with_annots(&mut doc, vec![Object::Reference(annot)]);
        assert!(appearance_streams(&test_adapter(&doc), page).is_empty());

        // A neighbouring flag (Print, bit 3) must not read as Hidden.
        let annot2 = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Stamp", "F" => 4,
            "AP" => dictionary! { "N" => Object::Reference(ap) }
        }));
        let page2 = page_with_annots(&mut doc, vec![Object::Reference(annot2)]);
        assert_eq!(appearance_streams(&test_adapter(&doc), page2).len(), 1);
    }

    #[test]
    fn a_state_keyed_appearance_follows_slash_as() {
        let mut doc = Document::with_version("1.5");
        let off = ap_stream(&mut doc, b"");
        let on = ap_stream(&mut doc, b"/Im0 Do");
        let states = dictionary! { "Off" => Object::Reference(off), "On" => Object::Reference(on) };

        // /AS names the current state: exactly that one.
        let sel = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "AS" => "On",
            "AP" => dictionary! { "N" => states.clone() }
        }));
        let page = page_with_annots(&mut doc, vec![Object::Reference(sel)]);
        let found = appearance_streams(&test_adapter(&doc), page);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, on);

        // /AS names a state the dictionary does not define: nothing is displayed.
        let missing = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "AS" => "Nope",
            "AP" => dictionary! { "N" => states.clone() }
        }));
        let page = page_with_annots(&mut doc, vec![Object::Reference(missing)]);
        assert!(appearance_streams(&test_adapter(&doc), page).is_empty());

        // No /AS at all: no state is current, so a collector takes every state.
        let none = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "AP" => dictionary! { "N" => states }
        }));
        let page = page_with_annots(&mut doc, vec![Object::Reference(none)]);
        let mut ids: Vec<ObjectId> = appearance_streams(&test_adapter(&doc), page).into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, {
            let mut v = vec![off, on];
            v.sort();
            v
        });
    }

    #[test]
    fn a_page_without_annots_or_appearances_yields_nothing() {
        let mut doc = Document::with_version("1.5");
        let bare = doc.add_object(Object::Dictionary(dictionary! { "Type" => "Page" }));
        assert!(appearance_streams(&test_adapter(&doc), bare).is_empty());
        // An annotation with no /AP, and one whose /AP has no /N.
        let a1 = doc.add_object(Object::Dictionary(dictionary! { "Type" => "Annot", "Subtype" => "Link" }));
        let a2 = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Link", "AP" => dictionary! { "D" => Object::Null }
        }));
        let page = page_with_annots(&mut doc, vec![Object::Reference(a1), Object::Reference(a2)]);
        assert!(appearance_streams(&test_adapter(&doc), page).is_empty());
    }

    #[test]
    fn an_indirect_annots_array_and_annot_dict_are_followed() {
        // `/Annots 5 0 R` with `5 0 obj [6 0 R]` is the common spelling.
        let mut doc = Document::with_version("1.5");
        let ap = ap_stream(&mut doc, b"/Im0 Do");
        let annot = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Annot", "Subtype" => "Stamp",
            "AP" => dictionary! { "N" => Object::Reference(ap) }
        }));
        let arr = doc.add_object(Object::Array(vec![Object::Reference(annot)]));
        let page = doc.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page", "Annots" => Object::Reference(arr)
        }));
        assert_eq!(appearance_streams(&test_adapter(&doc), page).len(), 1);
    }

    #[test]
    fn a_dangling_do_name_resolves_to_nothing() {
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"");
        let mut scope = XMap::new();
        scope.insert(b"F".to_vec(), id);
        assert!(xobject_at(&test_adapter(&doc), &scope, &[Object::Name(b"Missing".to_vec())]).is_none());
        assert!(xobject_at(&test_adapter(&doc), &scope, &[Object::Integer(3)]).is_none(), "a non-name operand names nothing");
        assert!(xobject_at(&test_adapter(&doc), &scope, &[]).is_none());
        let (got_id, _) = xobject_at(&test_adapter(&doc), &scope, &[Object::Name(b"F".to_vec())]).expect("in scope");
        assert_eq!(got_id, id);
    }
}
