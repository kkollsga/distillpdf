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

use crate::geom::Mat;
use crate::pdfobj::{content_bytes, deref, num_deref, sub_dict};
use lopdf::content::Operation;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashMap;

/// A resource scope: XObject name → object id, as an unqualified `/Im0` resolves at this
/// point in the walk.
pub(crate) type XMap = HashMap<Vec<u8>, ObjectId>;

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
    pub resources: Option<Dictionary>,
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

/// All XObject entries (images AND forms) of a resources dict: name → object id.
pub(crate) fn xobjects_of(doc: &Document, resources: &Dictionary) -> XMap {
    let mut map = XMap::new();
    overlay_xobjects(doc, resources, &mut map);
    map
}

/// Overlay one resource dictionary's `/XObject` entries onto a name → id map. Later
/// overlays win, so a nearer scope (a form's own resources, the page's own dictionary)
/// shadows an outer one — the precedence a renderer applies.
pub(crate) fn overlay_xobjects(doc: &Document, resources: &Dictionary, map: &mut XMap) {
    let Some(xd) = sub_dict(doc, resources, b"XObject") else {
        return;
    };
    for (name, val) in xd.iter() {
        if let Ok(id) = val.as_reference() {
            map.insert(name.clone(), id);
        }
    }
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
pub(crate) fn page_resource_chain(doc: &Document, page_id: ObjectId) -> Vec<Dictionary> {
    let Ok((own, inherited)) = doc.get_page_resources(page_id) else {
        return Vec::new();
    };
    // `inherited` runs page -> parent -> ...; reverse it so the outermost is applied first.
    let mut out: Vec<Dictionary> = inherited
        .iter()
        .rev()
        .filter_map(|id| doc.get_dictionary(*id).ok().cloned())
        .collect();
    if let Some(d) = own {
        out.push(d.clone());
    }
    out
}

/// The XObjects a page can `Do`, resolved over its whole resource chain.
pub(crate) fn page_xobjects(doc: &Document, page_id: ObjectId) -> XMap {
    let mut map = XMap::new();
    for res in page_resource_chain(doc, page_id) {
        overlay_xobjects(doc, &res, &mut map);
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
pub(crate) fn overlay_resources(doc: &Document, base: &mut Dictionary, inner: &Dictionary) {
    for (key, val) in inner.iter() {
        let Some(kind) = deref(doc, val).and_then(|o| o.as_dict().ok()) else {
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
pub(crate) fn page_resources(doc: &Document, page_id: ObjectId) -> Dictionary {
    let mut out = Dictionary::new();
    for res in page_resource_chain(doc, page_id) {
        overlay_resources(doc, &mut out, &res);
    }
    out
}

/// Resolve a `Do` operand to the XObject stream it names, in the scope in force.
/// `None` when the operand is not a name, the name is not in scope (a dangling reference
/// — nothing to draw), or the object is not a stream.
pub(crate) fn xobject_at<'a>(
    doc: &'a Document,
    scope: &XMap,
    operands: &[Object],
) -> Option<(ObjectId, &'a lopdf::Stream)> {
    let name = operands.first()?.as_name().ok()?;
    let id = *scope.get(name)?;
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    Some((id, stream))
}

/// A stream's `/Subtype`, or `b""` when it has none.
pub(crate) fn subtype_of(stream: &lopdf::Stream) -> &[u8] {
    stream.dict.get(b"Subtype").and_then(|o| o.as_name()).unwrap_or(b"")
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
pub(crate) fn form_scope(doc: &Document, stream: &lopdf::Stream, parent: &XMap, policy: ScopePolicy) -> Option<FormScope> {
    let resources = stream
        .dict
        .get(b"Resources")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
        .cloned();
    match policy {
        ScopePolicy::OverlayParent => {
            let mut xobjects = parent.clone();
            if let Some(fr) = &resources {
                overlay_xobjects(doc, fr, &mut xobjects);
            }
            Some(FormScope { xobjects, resources })
        }
        ScopePolicy::OwnOnly => {
            let fr = resources?;
            Some(FormScope {
                xobjects: xobjects_of(doc, &fr),
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
pub(crate) fn form_matrix(doc: &Document, stream: &lopdf::Stream) -> Mat {
    stream
        .dict
        .get(b"Matrix")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
        .filter(|a| a.len() >= 6)
        .map(|a| Mat {
            a: num_deref(doc, &a[0]),
            b: num_deref(doc, &a[1]),
            c: num_deref(doc, &a[2]),
            d: num_deref(doc, &a[3]),
            e: num_deref(doc, &a[4]),
            f: num_deref(doc, &a[5]),
        })
        .unwrap_or(Mat::ID)
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
    doc: &Document,
    stream: &lopdf::Stream,
    parent: &XMap,
    policy: ScopePolicy,
    depth: u32,
    budget: &mut crate::WalkBudget,
    sibling_cost: usize,
) -> Descend {
    if subtype_of(stream) != b"Form" || too_deep(depth) {
        return Descend::Skip;
    }
    // Bill the descent before doing it: cloning the inherited resource maps and decoding
    // the form's content stream dwarf a single operator's cost, and a form bomb pays
    // exactly this per branch — which is what actually bounds the attack.
    if !budget.spend(crate::FORM_DESCENT_COST + parent.len() + sibling_cost) {
        return Descend::Halt;
    }
    let Some(scope) = form_scope(doc, stream, parent, policy) else {
        return Descend::Skip;
    };
    let matrix = form_matrix(doc, stream);
    match form_ops(stream) {
        Some(ops) => Descend::Into(Box::new(FormDescent { ops, scope, matrix })),
        None => Descend::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

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
        let m = form_matrix(&doc, stream_of(&doc, id));
        assert_eq!((m.a, m.b, m.c, m.d, m.e, m.f), (1.0, 0.0, 0.0, 1.0, 0.0, 0.0));

        let short = form_dict(dictionary! { "Matrix" => vec![2.into(), 0.into()] });
        let (doc2, id2) = doc_with_form(short, b"");
        assert_eq!(form_matrix(&doc2, stream_of(&doc2, id2)).a, 1.0, "a too-short /Matrix is not read partially");
    }

    #[test]
    fn the_matrix_is_read_in_operand_order() {
        let d = form_dict(dictionary! { "Matrix" => vec![2.into(), 3.into(), 4.into(), 5.into(), 6.into(), 7.into()] });
        let (doc, id) = doc_with_form(d, b"");
        let m = form_matrix(&doc, stream_of(&doc, id));
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
        let m = form_matrix(&doc, stream_of(&doc, id));
        assert_eq!((m.a, m.d, m.f), (2.0, 2.0, 7.0));
        assert_eq!(m.e, 6.0, "an INDIRECT /Matrix element used to read as 0.0");

        // The whole array written indirectly, which used to yield the identity.
        let mut doc = Document::with_version("1.5");
        let arr = doc.add_object(Object::Array(vec![3.into(), 0.into(), 0.into(), 3.into(), 0.into(), 0.into()]));
        let d = form_dict(dictionary! { "Matrix" => Object::Reference(arr) });
        let id = doc.add_object(Object::Stream(Stream::new(d, b"".to_vec())));
        assert_eq!(form_matrix(&doc, stream_of(&doc, id)).a, 3.0);
    }

    #[test]
    fn the_depth_cap_is_spelled_once_and_refuses_at_the_limit() {
        assert!(!too_deep(0));
        assert!(!too_deep(crate::MAX_FORM_DEPTH - 1));
        assert!(too_deep(crate::MAX_FORM_DEPTH), "a descent is refused AT the cap, not one past it");
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"0 0 10 10 re f");
        let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
        let at_cap = descend_form(
            &doc,
            stream_of(&doc, id),
            &XMap::new(),
            ScopePolicy::OverlayParent,
            crate::MAX_FORM_DEPTH,
            &mut budget,
            0,
        );
        assert!(matches!(at_cap, Descend::Skip));
        let under = descend_form(
            &doc,
            stream_of(&doc, id),
            &XMap::new(),
            ScopePolicy::OverlayParent,
            crate::MAX_FORM_DEPTH - 1,
            &mut budget,
            0,
        );
        assert!(matches!(under, Descend::Into(_)), "one level under the cap still descends");
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

        let overlay = form_scope(&doc, stream_of(&doc, id), &parent, ScopePolicy::OverlayParent).expect("overlay scope");
        assert!(overlay.xobjects.contains_key(b"Outer".as_slice()), "OverlayParent keeps the inherited name");
        assert!(overlay.xobjects.contains_key(b"Own".as_slice()));

        let own = form_scope(&doc, stream_of(&doc, id), &parent, ScopePolicy::OwnOnly).expect("own scope");
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
        let scope = form_scope(&doc, stream_of(&doc, id), &parent, ScopePolicy::OverlayParent).expect("scope");
        assert_eq!(scope.xobjects[b"Im0".as_slice()], inner, "the form's own /Im0 wins");
    }

    #[test]
    fn own_only_refuses_a_form_that_carries_no_resources() {
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"");
        let parent = XMap::new();
        assert!(form_scope(&doc, stream_of(&doc, id), &parent, ScopePolicy::OwnOnly).is_none());
        assert!(
            form_scope(&doc, stream_of(&doc, id), &parent, ScopePolicy::OverlayParent).is_some(),
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
        let scope = form_scope(&doc, stream_of(&doc, id), &XMap::new(), ScopePolicy::OwnOnly).expect("scope");
        assert_eq!(scope.xobjects[b"Im0".as_slice()], im);
    }

    #[test]
    fn a_non_form_xobject_is_never_a_descent() {
        let d = dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 4, "Height" => 4 };
        let (doc, id) = doc_with_form(d, b"junk");
        let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
        assert!(matches!(
            descend_form(&doc, stream_of(&doc, id), &XMap::new(), ScopePolicy::OverlayParent, 0, &mut budget, 0),
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
            descend_form(&doc, stream_of(&doc, id), &XMap::new(), ScopePolicy::OverlayParent, 0, &mut budget, 0),
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
            descend_form(&doc, stream_of(&doc, id), &parent, ScopePolicy::OverlayParent, 0, &mut budget, 3),
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
                descend_form(&doc, stream_of(&doc, id), &XMap::new(), ScopePolicy::OverlayParent, 0, &mut budget, 0),
                Descend::Into(_)
            ));
        }
    }

    #[test]
    fn a_dangling_do_name_resolves_to_nothing() {
        let (doc, id) = doc_with_form(form_dict(Dictionary::new()), b"");
        let mut scope = XMap::new();
        scope.insert(b"F".to_vec(), id);
        assert!(xobject_at(&doc, &scope, &[Object::Name(b"Missing".to_vec())]).is_none());
        assert!(xobject_at(&doc, &scope, &[Object::Integer(3)]).is_none(), "a non-name operand names nothing");
        assert!(xobject_at(&doc, &scope, &[]).is_none());
        let (got_id, _) = xobject_at(&doc, &scope, &[Object::Name(b"F".to_vec())]).expect("in scope");
        assert_eq!(got_id, id);
    }
}
