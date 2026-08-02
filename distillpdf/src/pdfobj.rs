//! `pdfobj` — the PDF object-model primitives every extraction path needs.
//!
//! Reading a PDF means doing the same five things over and over: follow an indirect
//! reference, read a number, get a stream's bytes, decode a text string, list a stream's
//! filters. Before this module each of those lived as a private copy in whichever file
//! needed it — `deref` in five files, `num` in four, the stream reader in two strong and
//! two weak spellings, the text-string decoder in three — and the copies drifted. That
//! drift has shipped bugs (the CMYK polarity split between extract and render) and is the
//! reason a fourth walker could be written that silently loses uncompressed form streams.
//!
//! **The rule this module encodes:** mechanics and policy live here; interpretation stays
//! with the consumer. Where two consumers legitimately want different behaviour that
//! difference is a *named function* with a documented invariant ([`num`] vs [`num_deref`]),
//! never two copies of one name.
//!
//! Every function here is total: malformed input degrades (a default, `None`, or the raw
//! bytes) and never panics, never fabricates data, and never loops unboundedly.

use lopdf::{Dictionary, Document, Object, ObjectId};
use std::borrow::Cow;

/// The page size assumed when a document states no page box at all: US Letter, in points.
///
/// The one home for the `612`/`792` pair. Four call sites had it open-coded, which is how a
/// "default page" quietly means something different in the OCR planner than in the model.
/// A caller reaching for this is saying "the file told me nothing", never "the file is
/// letter-sized" — [`page_box`] returning `None` is the only thing that should lead here.
pub const DEFAULT_PAGE_PTS: (f32, f32) = (612.0, 792.0);

/// Follow one indirect reference; pass a direct object straight through.
///
/// **Invariants callers may rely on and must not re-check:**
/// - The result is **never** an [`Object::Reference`]. lopdf's `Document::get_object`
///   walks a whole reference chain itself, bounded by its own `DEREF_LIMIT`, so a cyclic
///   or absurdly long chain terminates inside lopdf. Callers must **not** loop on this.
/// - A dangling reference (target not in the document) yields `None`, not a panic.
pub(crate) fn deref<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Object> {
    match o {
        Object::Reference(r) => doc.get_object(*r).ok(),
        other => Some(other),
    }
}

/// A **direct** numeric object as `f32`; `0.0` for anything else.
///
/// This is the operand reader for **content streams**, where PDF 32000-1 §7.8.2 forbids
/// indirect references outright — so an `Object::Reference` here is malformed input and
/// `0.0` (the identity-ish degrade) is the right answer.
///
/// It is the **wrong** reader for a value pulled out of a dictionary or array, where an
/// indirect number is perfectly legal (`/MediaBox [0 0 12 0 R 13 0 R]`): use
/// [`num_deref`] there, or the number silently becomes `0.0`.
pub(crate) fn num(o: &Object) -> f32 {
    match o {
        Object::Integer(i) => *i as f32,
        Object::Real(r) => *r,
        _ => 0.0,
    }
}

/// A numeric object as `f32`, **following an indirect reference** first; `0.0` for a
/// non-numeric or dangling value.
///
/// The reader for dictionary and array values (page boxes, `/Decode`, `/Matrix` stored
/// indirectly). Terminates for the same reason [`deref`] does.
pub(crate) fn num_deref(doc: &Document, o: &Object) -> f32 {
    deref(doc, o).map(num).unwrap_or(0.0)
}

/// A stream's decoded bytes, decompressed when it carries a `/Filter`.
///
/// **Invariant: an unfiltered stream keeps its bytes.** lopdf's `decompressed_content()`
/// returns an *error* for a stream with no `/Filter` key, and some producers store content
/// and Form XObject streams raw — so calling it bare and taking `unwrap_or_default()`
/// decodes those streams as **empty**, silently losing every glyph and path inside them.
/// This reader returns the verbatim content instead. Borrowed when no decode happened, so
/// the common path allocates nothing.
///
/// The `Err` arm covers the filters lopdf does not implement (JBIG2, CCITT, DCT — image
/// codecs that reach here only on a malformed content stream), which likewise degrade to
/// raw bytes. Note what it does **not** cover: lopdf's Flate path swallows a decode error
/// and hands back whatever partial output it managed, so a *truncated* Flate stream comes
/// back short or empty rather than raw. That is lopdf's call, not ours — raw deflate bytes
/// would not parse as a content stream either — and it is stated here so no caller assumes
/// a non-empty result means an intact stream.
pub(crate) fn content_bytes(stream: &lopdf::Stream) -> Cow<'_, [u8]> {
    if stream.dict.get(b"Filter").is_err() {
        return Cow::Borrowed(&stream.content);
    }
    match stream.decompressed_content() {
        Ok(b) => Cow::Owned(b),
        Err(_) => Cow::Borrowed(&stream.content),
    }
}

/// The page's effective box as `[x0, y0, x1, y1]`: `/MediaBox`, else `/CropBox`, inherited
/// from the page tree when the page node itself carries neither.
///
/// **Invariants callers may rely on and must not re-check:**
/// - `/MediaBox` and `/CropBox` are **inheritable** page attributes (PDF 32000-1 §7.7.3.4):
///   a writer may state one once on a `/Pages` node and omit it from every page. The walk
///   climbs `/Parent` until it finds a box, and takes the *nearest* ancestor's.
/// - Extents are read with [`num_deref`], because an array element may legally be an
///   indirect reference (`/MediaBox [0 0 12 0 R 13 0 R]`). Reading them with [`num`] makes
///   such a box measure zero — which is how a real page becomes a guessed US-Letter one.
/// - Termination is bounded twice over: a visited set (a `/Parent` cycle returns `None`)
///   and [`crate::MAX_FORM_DEPTH`] levels. Never loops, never panics.
/// - `None` means the document states no usable box anywhere up the chain. It does **not**
///   mean US Letter — that decision belongs to the caller, which should reach for
///   [`DEFAULT_PAGE_PTS`]. The box is returned as authored: it may be inverted or
///   degenerate, so callers take `.abs()` of the extents they care about.
pub(crate) fn page_box(doc: &Document, page_id: ObjectId) -> Option<[f32; 4]> {
    let mut node = page_id;
    let mut seen: Vec<ObjectId> = Vec::new();
    for _ in 0..crate::MAX_FORM_DEPTH {
        if seen.contains(&node) {
            return None; // cyclic /Parent chain
        }
        seen.push(node);
        let dict = doc.get_object(node).ok()?.as_dict().ok()?;
        let found = dict
            .get(b"MediaBox")
            .ok()
            .or_else(|| dict.get(b"CropBox").ok())
            .and_then(|o| deref(doc, o))
            .and_then(|o| o.as_array().ok())
            .filter(|a| a.len() >= 4);
        if let Some(a) = found {
            return Some([num_deref(doc, &a[0]), num_deref(doc, &a[1]), num_deref(doc, &a[2]), num_deref(doc, &a[3])]);
        }
        node = match dict.get(b"Parent") {
            Ok(Object::Reference(r)) => *r,
            _ => return None,
        };
    }
    None
}

/// The page's `/Rotate` — the degrees a viewer turns the page CLOCKWISE before showing it —
/// normalized to exactly one of `0`, `90`, `180`, `270`.
///
/// **Invariants callers may rely on and must not re-check:**
/// - `/Rotate` is an **inheritable** page attribute (PDF 32000-1 §7.7.3.3), exactly like the
///   page box: a writer may state it once on a `/Pages` node. The walk climbs `/Parent`
///   until a node states one and takes the *nearest*, so [`page_box`] is the model here.
/// - A stated `/Rotate` **stops the climb even when it is unusable** — an explicitly stated
///   attribute shadows its ancestor's, and inheriting a different rotation past a broken one
///   would silently turn a page the file never asked to turn.
/// - The spec requires a multiple of 90 and permits a negative one (`-90` is common):
///   the result is `rem_euclid(360)`, so `-90` is `270`. Anything else — a non-multiple, a
///   non-number, a dangling reference — is **`0`**: a page we cannot turn squarely is left
///   alone rather than skewed.
/// - Never panics, never loops (visited set + [`crate::MAX_FORM_DEPTH`]).
pub(crate) fn page_rotation(doc: &Document, page_id: ObjectId) -> i32 {
    let mut node = page_id;
    let mut seen: Vec<ObjectId> = Vec::new();
    for _ in 0..crate::MAX_FORM_DEPTH {
        if seen.contains(&node) {
            return 0; // cyclic /Parent chain
        }
        seen.push(node);
        let dict = match doc.get_object(node).ok().and_then(|o| o.as_dict().ok()) {
            Some(d) => d,
            None => return 0,
        };
        if let Ok(o) = dict.get(b"Rotate") {
            // A dictionary value may legally be indirect, so `num_deref`, not `num`.
            let deg = num_deref(doc, o).round() as i32;
            return if deg % 90 == 0 { deg.rem_euclid(360) } else { 0 };
        }
        node = match dict.get(b"Parent") {
            Ok(Object::Reference(r)) => *r,
            _ => return 0,
        };
    }
    0
}

/// A single PDFDocEncoding high byte (0x80–0xFF) → char. This is the encoding PDF text
/// strings use when they are not UTF-16BE (PDF spec Annex D.2). NOTE it is NOT cp1252:
/// e.g. 0x85 is EN DASH here (ellipsis in cp1252), 0x84 EM DASH, 0x8D/0x8E curly double
/// quotes. ASCII (<0x80) and Latin-1 (0xA1–0xFF) map to the same code point; 0xA0 = €.
pub(crate) fn pdfdoc_char(c: u8) -> char {
    match c {
        0x80 => '\u{2022}', 0x81 => '\u{2020}', 0x82 => '\u{2021}', 0x83 => '\u{2026}',
        0x84 => '\u{2014}', 0x85 => '\u{2013}', 0x86 => '\u{0192}', 0x87 => '\u{2044}',
        0x88 => '\u{2039}', 0x89 => '\u{203A}', 0x8A => '\u{2212}', 0x8B => '\u{2030}',
        0x8C => '\u{201E}', 0x8D => '\u{201C}', 0x8E => '\u{201D}', 0x8F => '\u{2018}',
        0x90 => '\u{2019}', 0x91 => '\u{201A}', 0x92 => '\u{2122}', 0x93 => '\u{FB01}',
        0x94 => '\u{FB02}', 0x95 => '\u{0141}', 0x96 => '\u{0152}', 0x97 => '\u{0160}',
        0x98 => '\u{0178}', 0x99 => '\u{017D}', 0x9A => '\u{0131}', 0x9B => '\u{0142}',
        0x9C => '\u{0153}', 0x9D => '\u{0161}', 0x9E => '\u{017E}', 0x9F => '\u{FFFD}',
        0xA0 => '\u{20AC}',
        _ => c as char, // ASCII (<0x80) and Latin-1 (0xA1–0xFF) map to the same code point
    }
}

/// Decode a PDF **text string** (§7.9.2.2): outline titles, `/Producer`, page-label
/// prefixes, annotation contents. UTF-16BE when it carries a BE BOM, otherwise
/// PDFDocEncoding via [`pdfdoc_char`].
///
/// `from_utf8_lossy` is wrong for both halves — it turns UTF-16's NUL bytes into
/// NUL-interleaved garbage and replaces every 0x80–0xA0 PDFDocEncoding byte with U+FFFD.
///
/// **Not** for name-tree keys (`/Dests`, `/Names`): those are byte strings that happen to
/// be ASCII by convention, not text strings, and must stay bytes.
pub(crate) fn decode_text_string(b: &[u8]) -> String {
    if b.len() >= 2 && b[0] == 0xFE && b[1] == 0xFF {
        let u16s: Vec<u16> = b[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u16s)
    } else {
        b.iter().map(|&c| pdfdoc_char(c)).collect()
    }
}

/// A dictionary's `/Filter` chain as raw names, in application order. Empty when there is
/// no `/Filter`, when it is neither a name nor an array, or for array entries that are not
/// names — a malformed filter list degrades to "no filters", never to a panic.
///
/// Raw bytes, not `String`: filter names are PDF names and comparing them as bytes avoids
/// a lossy round-trip. Callers that need display strings map over the result.
pub(crate) fn filters_of(dict: &Dictionary) -> Vec<Vec<u8>> {
    match dict.get(b"Filter").ok() {
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Array(a)) => a.iter().filter_map(|o| o.as_name().ok().map(|n| n.to_vec())).collect(),
        _ => vec![],
    }
}

/// A sub-dictionary of `d` that may be written inline or as an indirect reference.
///
/// Every `/Resources` child (`/XObject`, `/Font`, `/ColorSpace`, `/ExtGState`, …) may be
/// either, so reading one with `as_dict()` alone silently misses the indirect half.
pub(crate) fn sub_dict<'a>(doc: &'a Document, d: &'a Dictionary, key: &[u8]) -> Option<&'a Dictionary> {
    d.get(key).ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    /// A document holding the given objects at ids `(1,0), (2,0), …` in order.
    fn doc_with(objs: Vec<Object>) -> (Document, Vec<lopdf::ObjectId>) {
        let mut doc = Document::with_version("1.5");
        let ids = objs.into_iter().map(|o| doc.add_object(o)).collect();
        (doc, ids)
    }

    #[test]
    fn deref_passes_direct_objects_through_and_follows_one_reference() {
        let (doc, ids) = doc_with(vec![Object::Integer(42)]);
        let direct = Object::Real(1.5);
        assert_eq!(deref(&doc, &direct), Some(&Object::Real(1.5)));
        assert_eq!(deref(&doc, &Object::Reference(ids[0])), Some(&Object::Integer(42)));
    }

    #[test]
    fn deref_of_a_dangling_reference_is_none_not_a_panic() {
        // A truncated / hand-edited file routinely points at an object that isn't there.
        let (doc, _) = doc_with(vec![Object::Null]);
        assert_eq!(deref(&doc, &Object::Reference((9999, 0))), None);
    }

    #[test]
    fn deref_terminates_on_a_reference_cycle_and_never_returns_a_reference() {
        // 1 0 obj -> 2 0 R, 2 0 obj -> 1 0 R. lopdf's own DEREF_LIMIT breaks the loop, so
        // callers may treat `deref` as one-shot; if this ever hangs, that assumption died.
        let mut doc = Document::with_version("1.5");
        let a = doc.add_object(Object::Null);
        let b = doc.add_object(Object::Reference(a));
        doc.set_object(a, Object::Reference(b));
        let start = Object::Reference(a);
        let got = deref(&doc, &start);
        assert!(
            !matches!(got, Some(Object::Reference(_))),
            "deref must never hand back a reference — callers do not loop"
        );
    }

    #[test]
    fn num_is_direct_only_and_num_deref_follows_the_reference() {
        // The pin for the split: an indirect number is 0.0 to `num` (content-stream
        // operands may not be indirect) and its real value to `num_deref` (dict/array
        // values may). Reading a dict value with `num` is how an indirect /MediaBox
        // element silently becomes 0.0.
        let (doc, ids) = doc_with(vec![Object::Real(612.0)]);
        let r = Object::Reference(ids[0]);
        assert_eq!(num(&r), 0.0);
        assert_eq!(num_deref(&doc, &r), 612.0);
        for direct in [Object::Integer(-7), Object::Real(2.5)] {
            assert_eq!(num(&direct), num_deref(&doc, &direct));
        }
    }

    #[test]
    fn num_degrades_non_numeric_and_dangling_input_to_zero() {
        let (doc, _) = doc_with(vec![Object::Null]);
        for o in [Object::Null, Object::Boolean(true), Object::Name(b"Foo".to_vec()), Object::Array(vec![])] {
            assert_eq!(num(&o), 0.0);
            assert_eq!(num_deref(&doc, &o), 0.0);
        }
        assert_eq!(num_deref(&doc, &Object::Reference((9999, 0))), 0.0);
    }

    #[test]
    fn content_bytes_keeps_an_unfiltered_stream_instead_of_emptying_it() {
        // THE reason this helper exists: `decompressed_content()` errors on a stream with
        // no /Filter, so `unwrap_or_default()` returns "" and the stream's whole content —
        // text, paths — disappears.
        let raw = b"BT /F1 12 Tf (hi) Tj ET".to_vec();
        let s = Stream::new(dictionary! {}, raw.clone());
        assert!(s.decompressed_content().is_err(), "the premise: lopdf errors without /Filter");
        assert_eq!(content_bytes(&s).as_ref(), &raw[..]);
    }

    #[test]
    fn content_bytes_decompresses_a_filtered_stream() {
        let raw = b"0 0 100 100 re f\n".repeat(8);
        let mut s = Stream::new(dictionary! {}, raw.clone());
        s.compress().expect("flate compress");
        assert_ne!(s.content, raw, "the fixture must really be compressed");
        assert_eq!(content_bytes(&s).as_ref(), &raw[..]);
    }

    #[test]
    fn content_bytes_falls_back_to_raw_on_a_filter_lopdf_cannot_apply() {
        // An image codec named on a stream we are asked to read as content: lopdf returns
        // `Unimplemented`, and the caller gets the bytes on disk — garbage it can reject,
        // never a silent empty page.
        let unknown = Stream::new(dictionary! { "Filter" => "JBIG2Decode" }, b"\x00\x01\x02".to_vec());
        assert_eq!(content_bytes(&unknown).as_ref(), b"\x00\x01\x02");
    }

    #[test]
    fn a_truncated_flate_stream_comes_back_short_not_raw() {
        // Pins the ONE degrade path that is not "raw bytes", so the doc comment stays
        // honest: lopdf's zlib reader swallows its error and returns the partial output,
        // so `decompressed_content` reports Ok and our fallback never sees the corruption.
        let raw = b"0 0 100 100 re f\n".repeat(8);
        let mut s = Stream::new(dictionary! {}, raw.clone());
        s.compress().expect("flate compress");
        let truncated = Stream::new(s.dict.clone(), s.content[..s.content.len() / 2].to_vec());
        assert!(truncated.decompressed_content().is_ok(), "lopdf swallows the zlib error");
        let got = content_bytes(&truncated);
        assert_ne!(got.as_ref(), &truncated.content[..], "not the raw bytes");
        assert!(got.len() < raw.len(), "a truncated stream decodes short, got {} of {}", got.len(), raw.len());
    }

    #[test]
    fn decode_text_string_reads_utf16be_behind_a_bom() {
        // FE FF then "Héllo" in UTF-16BE.
        let b = b"\xfe\xff\x00H\x00\xe9\x00l\x00l\x00o";
        assert_eq!(decode_text_string(b), "Héllo");
        assert_eq!(decode_text_string(b"\xfe\xff"), "", "a bare BOM is the empty string");
    }

    #[test]
    fn decode_text_string_reads_the_pdfdocencoding_high_range() {
        // 0x80-0xA0 is where PDFDocEncoding and every other 8-bit encoding disagree, and
        // where `from_utf8_lossy` produces U+FFFD. Nothing tested this before.
        assert_eq!(decode_text_string(&[0x92]), "™");
        assert_eq!(decode_text_string(&[0x83, 0x84, 0x85, 0xA0]), "…—–€");
        assert_eq!(decode_text_string(&[0x8D, 0x8E, 0x8F, 0x90]), "\u{201C}\u{201D}\u{2018}\u{2019}");
        // ASCII and Latin-1 pass through unchanged.
        assert_eq!(decode_text_string(b"Plain ASCII"), "Plain ASCII");
        assert_eq!(decode_text_string(&[0xE9, 0xFF]), "éÿ");
    }

    #[test]
    fn decode_text_string_survives_odd_lengths_and_lone_surrogates() {
        // An odd trailing byte after the BOM is dropped by `chunks_exact` rather than
        // panicking; an unpaired surrogate becomes U+FFFD rather than an invalid String.
        assert_eq!(decode_text_string(b"\xfe\xff\x00A\x00"), "A");
        assert_eq!(decode_text_string(b"\xfe\xff\xd8\x00"), "\u{FFFD}");
        assert_eq!(decode_text_string(&[]), "");
    }

    /// A page-tree chain of `n` `/Pages` nodes above one `/Page`, with `boxes[i]` merged into
    /// node `i` (0 = the page itself). Returns the doc and the page id.
    fn page_tree(n: usize, boxes: &[(usize, Dictionary)]) -> (Document, lopdf::ObjectId) {
        let mut doc = Document::with_version("1.5");
        let ids: Vec<lopdf::ObjectId> = (0..n).map(|_| doc.add_object(Object::Null)).collect();
        for (i, id) in ids.iter().enumerate() {
            let mut d = dictionary! { "Type" => if i == 0 { "Page" } else { "Pages" } };
            if i + 1 < n {
                d.set("Parent", Object::Reference(ids[i + 1]));
            }
            for (at, extra) in boxes {
                if *at == i {
                    for (k, v) in extra.iter() {
                        d.set(k.clone(), v.clone());
                    }
                }
            }
            doc.set_object(*id, Object::Dictionary(d));
        }
        (doc, ids[0])
    }

    fn bx(v: [i64; 4]) -> Dictionary {
        dictionary! { "MediaBox" => v.iter().map(|n| Object::Integer(*n)).collect::<Vec<_>>() }
    }

    #[test]
    fn page_box_inherits_from_the_nearest_ancestor_that_states_one() {
        // Both ancestors carry a box; the NEAREST wins, not the root.
        let (doc, page) = page_tree(3, &[(1, bx([0, 0, 842, 595])), (2, bx([0, 0, 200, 200]))]);
        assert_eq!(page_box(&doc, page), Some([0.0, 0.0, 842.0, 595.0]));
        // The page's own box beats every ancestor's.
        let (doc, page) = page_tree(2, &[(0, bx([0, 0, 100, 100])), (1, bx([0, 0, 842, 595]))]);
        assert_eq!(page_box(&doc, page), Some([0.0, 0.0, 100.0, 100.0]));
    }

    #[test]
    fn page_box_resolves_indirect_extents() {
        // THE reason this is `num_deref` and not `num`: an array element may legally be an
        // indirect reference, and reading it directly makes the whole page measure zero.
        let mut doc = Document::with_version("1.5");
        let w = doc.add_object(Object::Integer(1008));
        let h = doc.add_object(Object::Real(612.0));
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![Object::Integer(0), Object::Integer(0), Object::Reference(w), Object::Reference(h)],
        });
        assert_eq!(page_box(&doc, page), Some([0.0, 0.0, 1008.0, 612.0]));
    }

    #[test]
    fn page_box_falls_back_to_the_crop_box_but_prefers_the_media_box() {
        let crop = dictionary! { "CropBox" => vec![Object::Integer(0), Object::Integer(0), Object::Integer(400), Object::Integer(650)] };
        let (doc, page) = page_tree(1, &[(0, crop.clone())]);
        assert_eq!(page_box(&doc, page), Some([0.0, 0.0, 400.0, 650.0]));
        // With both present the /MediaBox is the page box; /CropBox is only the fallback.
        let mut both = crop;
        both.set("MediaBox", vec![Object::Integer(0), Object::Integer(0), Object::Integer(612), Object::Integer(792)]);
        let (doc, page) = page_tree(1, &[(0, both)]);
        assert_eq!(page_box(&doc, page), Some([0.0, 0.0, 612.0, 792.0]));
    }

    #[test]
    fn page_box_degrades_a_short_or_malformed_box_to_the_ancestors() {
        // A 2-element box is not a box: keep climbing rather than returning nonsense.
        let short = dictionary! { "MediaBox" => vec![Object::Integer(0), Object::Integer(0)] };
        let (doc, page) = page_tree(2, &[(0, short), (1, bx([0, 0, 842, 595]))]);
        assert_eq!(page_box(&doc, page), Some([0.0, 0.0, 842.0, 595.0]));
        // Non-numeric entries are 0.0 rather than a panic — the caller's sanity filter rejects it.
        let junk = dictionary! { "MediaBox" => vec![Object::Name(b"A".to_vec()), Object::Null, Object::Null, Object::Null] };
        let (doc, page) = page_tree(1, &[(0, junk)]);
        assert_eq!(page_box(&doc, page), Some([0.0; 4]));
    }

    #[test]
    fn page_box_terminates_on_a_cyclic_or_absurdly_deep_parent_chain() {
        // A /Parent cycle: node 0 -> 1 -> 0. Without the visited set this never returns.
        let mut doc = Document::with_version("1.5");
        let a = doc.add_object(Object::Null);
        let b = doc.add_object(dictionary! { "Type" => "Pages", "Parent" => Object::Reference(a) });
        doc.set_object(a, Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => Object::Reference(b) }));
        let t = std::time::Instant::now();
        assert_eq!(page_box(&doc, a), None, "a cyclic /Parent chain has no box");
        assert!(t.elapsed().as_secs() < 5, "the cycle guard is not bounding the walk");
        // Deeper than MAX_FORM_DEPTH: the box exists but is out of reach, and that is a
        // bounded `None`, never a hang.
        let deep = crate::MAX_FORM_DEPTH as usize + 5;
        let (doc, page) = page_tree(deep, &[(deep - 1, bx([0, 0, 842, 595]))]);
        assert_eq!(page_box(&doc, page), None);
        // …and one level inside the cap it is found.
        let ok = crate::MAX_FORM_DEPTH as usize;
        let (doc, page) = page_tree(ok, &[(ok - 1, bx([0, 0, 842, 595]))]);
        assert_eq!(page_box(&doc, page), Some([0.0, 0.0, 842.0, 595.0]));
    }

    #[test]
    fn page_box_of_a_dangling_page_id_is_none_not_a_default() {
        // `None` means "the file said nothing"; choosing US Letter is the caller's business.
        let (doc, _) = doc_with(vec![Object::Null]);
        assert_eq!(page_box(&doc, (9_999, 0)), None);
        assert_eq!(DEFAULT_PAGE_PTS, (612.0, 792.0), "the one home for the letter default");
    }

    fn rot(v: Object) -> Dictionary {
        dictionary! { "Rotate" => v }
    }

    #[test]
    fn page_rotation_inherits_from_the_nearest_ancestor_that_states_one() {
        // `/Rotate` is inheritable like the page box, and a landscape section states it once
        // on its `/Pages` node. Reading only the page dict left every such page upright.
        let (doc, page) = page_tree(3, &[(2, rot(Object::Integer(90)))]);
        assert_eq!(page_rotation(&doc, page), 90);
        // The nearest ancestor wins, and the page's own value beats every ancestor's.
        let (doc, page) = page_tree(3, &[(1, rot(Object::Integer(180))), (2, rot(Object::Integer(90)))]);
        assert_eq!(page_rotation(&doc, page), 180);
        let (doc, page) = page_tree(2, &[(0, rot(Object::Integer(270))), (1, rot(Object::Integer(90)))]);
        assert_eq!(page_rotation(&doc, page), 270);
        // Nothing anywhere: upright.
        let (doc, page) = page_tree(3, &[]);
        assert_eq!(page_rotation(&doc, page), 0);
    }

    #[test]
    fn page_rotation_normalizes_negative_and_over_full_turns() {
        // `-90` is what a great many writers emit, and it is legal; it is 270, not "no
        // rotation" and not a negative index into a 4-way match.
        for (stated, want) in [(-90, 270), (-180, 180), (-270, 90), (360, 0), (450, 90), (720, 0), (0, 0)] {
            let (doc, page) = page_tree(1, &[(0, rot(Object::Integer(stated)))]);
            assert_eq!(page_rotation(&doc, page), want, "/Rotate {stated}");
        }
        // A real is rounded, not truncated to zero.
        let (doc, page) = page_tree(1, &[(0, rot(Object::Real(90.0)))]);
        assert_eq!(page_rotation(&doc, page), 90);
    }

    #[test]
    fn page_rotation_resolves_an_indirect_value() {
        // The same trap `/MediaBox` has: a dictionary value may be written indirectly, and
        // the direct-only `num` reader turns it into 0 — a landscape page silently upright.
        let mut doc = Document::with_version("1.5");
        let r = doc.add_object(Object::Integer(90));
        let page = doc.add_object(dictionary! { "Type" => "Page", "Rotate" => Object::Reference(r) });
        assert_eq!(page_rotation(&doc, page), 90);
    }

    #[test]
    fn page_rotation_degrades_an_unusable_value_to_upright_without_inheriting_past_it() {
        // Not a multiple of 90, not a number, dangling: 0. And a STATED-but-broken value
        // shadows the ancestor's — inheriting past it would turn a page the file did not.
        for junk in [Object::Integer(45), Object::Name(b"Ninety".to_vec()), Object::Null, Object::Reference((9_999, 0))] {
            let (doc, page) = page_tree(2, &[(0, rot(junk.clone())), (1, rot(Object::Integer(90)))]);
            assert_eq!(page_rotation(&doc, page), 0, "junk {junk:?} must not inherit 90");
        }
    }

    #[test]
    fn page_rotation_terminates_on_a_cyclic_or_absurdly_deep_parent_chain() {
        let mut doc = Document::with_version("1.5");
        let a = doc.add_object(Object::Null);
        let b = doc.add_object(dictionary! { "Type" => "Pages", "Parent" => Object::Reference(a) });
        doc.set_object(a, Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => Object::Reference(b) }));
        let t = std::time::Instant::now();
        assert_eq!(page_rotation(&doc, a), 0);
        assert!(t.elapsed().as_secs() < 5, "the cycle guard is not bounding the walk");
        let deep = crate::MAX_FORM_DEPTH as usize + 5;
        let (doc, page) = page_tree(deep, &[(deep - 1, rot(Object::Integer(90)))]);
        assert_eq!(page_rotation(&doc, page), 0, "out of reach is upright, not a hang");
        assert_eq!(page_rotation(&doc, (9_999, 0)), 0, "a dangling page id is upright");
    }

    #[test]
    fn filters_of_reads_both_spellings_and_degrades_malformed_lists() {
        assert_eq!(filters_of(&dictionary! { "Filter" => "FlateDecode" }), vec![b"FlateDecode".to_vec()]);
        let chain = dictionary! { "Filter" => vec![Object::Name(b"ASCII85Decode".to_vec()), Object::Name(b"DCTDecode".to_vec())] };
        assert_eq!(filters_of(&chain), vec![b"ASCII85Decode".to_vec(), b"DCTDecode".to_vec()]);
        assert!(filters_of(&dictionary! {}).is_empty(), "no /Filter -> no filters");
        assert!(filters_of(&dictionary! { "Filter" => 7 }).is_empty(), "a non-name /Filter is not a filter");
        // A mixed array keeps only the names — a junk entry must not shift the chain.
        let mixed = dictionary! { "Filter" => vec![Object::Integer(1), Object::Name(b"FlateDecode".to_vec())] };
        assert_eq!(filters_of(&mixed), vec![b"FlateDecode".to_vec()]);
    }
}
