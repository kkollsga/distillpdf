//! The raster decode stack: colour-space resolution, sample assembly, and the JPEG /
//! `/Decode` polarity rules.
//!
//! Every function here moved VERBATIM out of `extract.rs`, which held the strong, correct
//! copies of this logic while `img.rs` (the render path) carried weaker ones that lose
//! Indexed palettes, sub-byte depths, `/ImageMask` stencils and `/Decode` arrays. Giving
//! the strong stack a home is what lets those render-path defects be fixed ONCE, at the
//! helper, in the phases that follow — rather than by copying a fifth variant.
//!
//! `img.rs` now consumes the whole stack. The JPEG half ([`decode_dct_rgb`],
//! [`decode_inverts`], [`codec_payload`]) is what makes the render path honour an inverting
//! `/Decode` on a gray or RGB JPEG — its own copies applied that only to CMYK. The sample
//! half ([`decode_samples`], and [`samples_decodable`] for the no-decode placeholder gate)
//! replaced a decoder that read 8 bpc only and *guessed* the channel count from
//! `len(samples) / (w·h)`, so an Indexed image rendered its palette indices as gray levels.
//! Extract and render now decode the same bytes the same way, by construction rather than
//! by two teams keeping two copies in step.
//!
//! **Invariants callers may rely on and must not re-check:**
//! - Nothing here panics or fabricates pixels. A stream that cannot be reduced faithfully
//!   (unknown colour space, unsupported depth, truncated samples, absurd dimensions)
//!   returns `None`, and the caller reports the image honestly rather than emitting a
//!   confidently wrong picture.
//! - Allocation is bounded before it happens: [`MAX_IMAGE_DIM`] per side and
//!   [`MAX_IMAGE_PIXELS`] in total are checked from the DECLARED dimensions, so a
//!   decompression bomb is refused rather than allocated.
//! - Colour-space indirection is bounded by [`MAX_CS_DEPTH`], so a cyclic `/ColorSpace`
//!   chain terminates.

use crate::pdfobj::{content_bytes, deref, filters_of, is_generic_filter, sub_dict};
use lopdf::{Dictionary, Document, Object};
use std::borrow::Cow;

pub(crate) fn filter_to_format(filters: &Option<Vec<String>>) -> &'static str {
    match filters {
        Some(fs) => {
            if fs.iter().any(|f| f == "DCTDecode") {
                "jpeg"
            } else if fs.iter().any(|f| f == "JPXDecode") {
                "jpx"
            } else if fs.iter().any(|f| f == "CCITTFaxDecode") {
                "ccitt"
            } else if fs.iter().any(|f| f == "JBIG2Decode") {
                "jbig2"
            } else {
                "raw" // Flate/LZW/none: plain samples -> assemble_png turns most into "png"
            }
        }
        None => "raw",
    }
}

/// The image codec a stream declares that this crate **declines to decode**, as
/// `(filter name, what a reader would call it)` — or `None` for anything we can render.
///
/// The one list. Four gates on the raster path refuse these filters (`img`'s `decode_rgb`,
/// `decode_smask`, `decodable` and `data_uri`), and until now that refusal was invisible:
/// the image simply did not appear, which a reader cannot tell from a figure we correctly
/// decided not to emit. Naming the codec here lets the render emit a labelled placeholder
/// and `stream_integrity` report the stream, from the same source of truth.
///
/// JPEG 2000 decoding is **parked by decision**, not oversight — there is no mature
/// pure-Rust decoder and OpenJPEG's CVE record is not worth carrying in the automatic path
/// for untrusted files (`dev-docs/plans/composited-figure-raster-gaps.md` §D). Making the
/// gap visible is the part that does not need a codec.
///
/// `ASCIIHexDecode` is deliberately absent: lopdf cannot apply it either, but it is a
/// *generic* layer, and a stream wrapped in one is already reported as `filter-unapplied`.
pub(crate) fn declined_codec(dict: &Dictionary) -> Option<(&'static str, &'static str)> {
    filters_of(dict).into_iter().find_map(|f| match f.as_slice() {
        b"JPXDecode" => Some(("JPXDecode", "JPEG 2000")),
        b"JBIG2Decode" => Some(("JBIG2Decode", "JBIG2")),
        _ => None,
    })
}

/// Cap on colour-space indirection (`/CS0 → [/Indexed [/ICCBased …] …]`, and the cyclic
/// resource dictionary a hostile file can write).
pub(crate) const MAX_CS_DEPTH: u32 = 8;

/// Resolve a `/ColorSpace` value to the object that actually describes the space.
///
/// Two things make the declared value not be that object already, and both were why
/// `color_space` came back `None` for 971 of 2604 corpus images:
///   * it is written as an **indirect reference** (`/ColorSpace 42 0 R`), and
///   * per PDF 32000-1 §8.6.3 an image may name a space *defined in the resource
///     dictionary's `/ColorSpace` sub-dictionary* (`/ColorSpace /CS0`) rather than a
///     device space, so the name has to be looked up in `res` before it means anything.
pub(crate) fn resolve_cs<'a>(doc: &'a Document, res: &'a Dictionary, o: &'a Object, depth: u32) -> Option<&'a Object> {
    if depth > MAX_CS_DEPTH {
        return None;
    }
    match o {
        Object::Reference(r) => resolve_cs(doc, res, doc.get_object(*r).ok()?, depth + 1),
        Object::Name(n) if !is_builtin_cs_name(n) => {
            match sub_dict(doc, res, b"ColorSpace").and_then(|d| d.get(n.as_slice()).ok()) {
                Some(v) => resolve_cs(doc, res, v, depth + 1),
                None => Some(o), // no such resource: report the name verbatim, honestly
            }
        }
        other => Some(other),
    }
}

/// Names that mean a space in themselves (so they are never resource lookups).
fn is_builtin_cs_name(n: &[u8]) -> bool {
    matches!(
        n,
        b"DeviceGray" | b"DeviceRGB" | b"DeviceCMYK" | b"G" | b"RGB" | b"CMYK" | b"Pattern"
    )
}

/// PDF's abbreviated inline-image colour-space names spelled out in full.
fn canonical_cs_name(n: &[u8]) -> String {
    match n {
        b"G" => "DeviceGray".to_string(),
        b"RGB" => "DeviceRGB".to_string(),
        b"CMYK" => "DeviceCMYK".to_string(),
        b"I" => "Indexed".to_string(),
        other => String::from_utf8_lossy(other).into_owned(),
    }
}

/// The image's colour-space **family** name (`DeviceRGB`, `ICCBased`, `Indexed`, …),
/// after [`resolve_cs`]. The family is what pymupdf reports too; the component count an
/// `ICCBased`/`Indexed` space implies is what [`cs_model`] derives for PNG assembly.
pub(crate) fn image_color_space(doc: &Document, res: &Dictionary, dict: &Dictionary) -> Option<String> {
    match resolve_cs(doc, res, dict.get(b"ColorSpace").ok()?, 0)? {
        Object::Name(n) => Some(canonical_cs_name(n)),
        Object::Array(a) => {
            let head = deref(doc, a.first()?)?.as_name().ok()?;
            Some(canonical_cs_name(head))
        }
        _ => None,
    }
}

/// A colour space reduced to what PNG assembly needs: how many samples make a pixel and
/// how those samples become RGB. Spaces we cannot faithfully reduce (`Lab`, `Pattern`, a
/// `Separation` whose tint transform will not evaluate) are deliberately absent — their
/// rows keep `format:"raw"` rather than being handed back as a plausible-looking wrong
/// colour.
pub(crate) enum Cs {
    Gray,
    Rgb,
    Cmyk,
    /// Palette space: one index sample per pixel into `lookup`, `base`-many bytes each.
    Indexed { base: Box<Cs>, lookup: Vec<u8> },
    /// `Separation` / `DeviceN`: `k` **tint** samples per pixel, which mean nothing until
    /// the space's tint transform maps them into `alt`. Reading a tint as an intensity is
    /// the image-path twin of the `scn` defect `vector.rs` fixes — a 10% spot tint decodes
    /// to a near-black pixel where the file meant a pale one.
    Tint { k: usize, tint: Box<crate::function::Function>, alt: Box<Cs> },
}

/// Tint quantisation for [`Cs::Tint`]: the transform is precomputed over an 8-bit grid per
/// colorant, so the per-pixel decode is a table lookup rather than a PDF-function
/// interpreter in the inner loop of a 64 M-pixel image.
const TINT_LEVELS: usize = 256;
/// …and the table is `TINT_LEVELS^k` entries, so only 1- and 2-colorant spaces are served
/// (a `Separation` is 1 by definition; 2 is 65 536 entries). A wider `DeviceN` image keeps
/// `format:"raw"` exactly as it did before, rather than trading a bounded decode for an
/// unbounded one.
const MAX_TINT_COLORANTS: usize = 2;

impl Cs {
    pub(crate) fn components(&self) -> usize {
        match self {
            Cs::Gray => 1,
            Cs::Rgb => 3,
            Cs::Cmyk => 4,
            Cs::Indexed { .. } => 1,
            Cs::Tint { k, .. } => *k,
        }
    }
    /// True when the space is achromatic, so the PNG can be 8-bit grayscale.
    fn is_gray(&self) -> bool {
        match self {
            Cs::Gray => true,
            Cs::Indexed { base, .. } => base.is_gray(),
            _ => false,
        }
    }
}

/// One colour in a *device* space (the only kind a tint transform's alternate may be) as
/// RGB. The same three conversions the sample loop applies, in float — a tint transform
/// hands back floats, not bytes.
fn device_rgb(alt: &Cs, v: &[f32]) -> Option<[u8; 3]> {
    let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    match alt {
        Cs::Gray if !v.is_empty() => Some([q(v[0]); 3]),
        Cs::Rgb if v.len() >= 3 => Some([q(v[0]), q(v[1]), q(v[2])]),
        Cs::Cmyk if v.len() >= 4 => {
            let k = 1.0 - v[3].clamp(0.0, 1.0);
            Some([q((1.0 - v[0].clamp(0.0, 1.0)) * k), q((1.0 - v[1].clamp(0.0, 1.0)) * k), q((1.0 - v[2].clamp(0.0, 1.0)) * k)])
        }
        _ => None,
    }
}

/// The tint transform evaluated over the whole 8-bit tint grid, once per image.
///
/// Index layout is the PDF sample convention — **first colorant varies fastest** — so
/// entry `i` is the tints `(i % 256, (i / 256) % 256, …)`, matching the order the decode
/// loop packs its quantised samples in.
///
/// An individual grid point the transform refuses (an input its `/Domain` excludes, an
/// output arity that disagrees with the alternate space) degrades to **ink coverage**:
/// tint `t` of a colorant laid on white paper reads as luminance `1 - t`, the same
/// fallback `vector.rs` gives an unevaluable `scn`. Pale stays pale; nothing inverts.
fn tint_lut(k: usize, f: &crate::function::Function, alt: &Cs) -> Vec<[u8; 3]> {
    let n = TINT_LEVELS.pow(k as u32);
    let mut lut = Vec::with_capacity(n);
    let mut t = vec![0f32; k];
    for i in 0..n {
        let mut rest = i;
        for slot in t.iter_mut() {
            *slot = (rest % TINT_LEVELS) as f32 / (TINT_LEVELS - 1) as f32;
            rest /= TINT_LEVELS;
        }
        lut.push(f.eval(&t).and_then(|out| device_rgb(alt, &out)).unwrap_or_else(|| {
            let ink = t.iter().copied().fold(0.0f32, f32::max).clamp(0.0, 1.0);
            [((1.0 - ink) * 255.0).round() as u8; 3]
        }));
    }
    lut
}

pub(crate) fn cs_model(doc: &Document, res: &Dictionary, o: &Object, depth: u32) -> Option<Cs> {
    if depth > MAX_CS_DEPTH {
        return None;
    }
    match resolve_cs(doc, res, o, 0)? {
        Object::Name(n) => match n.as_slice() {
            b"DeviceGray" | b"G" | b"CalGray" => Some(Cs::Gray),
            b"DeviceRGB" | b"RGB" | b"CalRGB" => Some(Cs::Rgb),
            b"DeviceCMYK" | b"CMYK" => Some(Cs::Cmyk),
            _ => None,
        },
        Object::Array(a) => match deref(doc, a.first()?)?.as_name().ok()? {
            // An ICC profile's `/N` is the component count — the whole point of reading it.
            b"ICCBased" => match deref(doc, a.get(1)?)?.as_stream().ok()?.dict.get(b"N").ok()?.as_i64().ok()? {
                1 => Some(Cs::Gray),
                3 => Some(Cs::Rgb),
                4 => Some(Cs::Cmyk),
                _ => None,
            },
            b"CalGray" => Some(Cs::Gray),
            b"CalRGB" => Some(Cs::Rgb),
            b"Indexed" | b"I" => {
                let base = cs_model(doc, res, a.get(1)?, depth + 1)?;
                if matches!(base, Cs::Indexed { .. }) {
                    return None; // an Indexed base is illegal (§8.6.6.3); don't guess
                }
                let lookup = match deref(doc, a.get(3)?)? {
                    Object::String(s, _) => s.clone(),
                    Object::Stream(st) => content_bytes(st).into_owned(),
                    _ => return None,
                };
                Some(Cs::Indexed { base: Box::new(base), lookup })
            }
            // A spot space's samples are tints; the tint transform is what makes them a
            // colour. Without an evaluable one (a Type 4 calculator, a malformed function,
            // an alternate we cannot reduce) the image stays `raw` rather than being
            // decoded as if the tints were intensities.
            b"Separation" | b"DeviceN" => {
                let k = if a.first()?.as_name().ok()? == b"Separation" { 1 } else { deref(doc, a.get(1)?)?.as_array().ok()?.len() };
                if k == 0 || k > MAX_TINT_COLORANTS {
                    return None;
                }
                let alt = cs_model(doc, res, a.get(2)?, depth + 1)?;
                if matches!(alt, Cs::Indexed { .. } | Cs::Tint { .. }) {
                    return None; // an Indexed or spot alternate is illegal (§8.6.6.4)
                }
                let tint = crate::function::Function::parse(doc, a.get(3)?)?;
                // A transform whose output arity disagrees with the space it feeds is not
                // one to trust — the same refusal `vector.rs::parse_cs` makes.
                if tint.n_outputs().is_some_and(|n| n != alt.components()) {
                    return None;
                }
                Some(Cs::Tint { k, tint: Box::new(tint), alt: Box::new(alt) })
            }
            _ => None, // Lab / Pattern: not reducible to RGB here
        },
        _ => None,
    }
}

/// Per-dimension and total-pixel ceilings: a malformed or hostile stream can declare an
/// enormous `/Width`×`/Height`, and both the extract and the render path allocate from
/// those numbers. Declared once here; `extract.rs` and `img.rs` had the same two values
/// under two names (`MAX_ASSEMBLE_*` / `MAX_IMAGE_*`) before this.
pub(crate) const MAX_IMAGE_DIM: i64 = 0x1FFFF; // 131071 px per side
pub(crate) const MAX_IMAGE_PIXELS: usize = 64 << 20; // 64 M px

/// Are these declared dimensions safe to allocate from? Refusing them before the raw
/// buffer is reserved is what prevents a decompression-bomb OOM.
pub(crate) fn dims_sane(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w as i64 <= MAX_IMAGE_DIM && h as i64 <= MAX_IMAGE_DIM && (w as usize).saturating_mul(h as usize) <= MAX_IMAGE_PIXELS
}

/// The `/Decode` array as floats, when it has the 2·n entries the sample layout needs.
fn decode_array(doc: &Document, dict: &Dictionary, n: usize) -> Option<Vec<f32>> {
    let a = deref(doc, dict.get(b"Decode").ok()?)?.as_array().ok()?;
    if a.len() != n * 2 {
        return None;
    }
    a.iter()
        .map(|o| match o {
            Object::Integer(i) => Some(*i as f32),
            Object::Real(r) => Some(*r),
            _ => None,
        })
        .collect()
}

/// Everything about a sampled image that is decidable from its *dictionary* alone: the
/// colour model, the bit depth, and the pixel dimensions, all already bounds-checked.
///
/// Split out of [`decode_samples`] so the cheap "could this be decoded?" question and the
/// decode itself answer from ONE gate. `img.rs`'s placeholder mode asked it separately
/// (`bpc == 8` and nothing else), which both over-counted images the decoder then refused
/// and under-counted the sub-byte depths it can in fact render.
struct SamplePlan {
    cs: Cs,
    /// Bits per component; one of 1, 2, 4, 8, 16.
    bpc: usize,
    w: usize,
    h: usize,
}

/// The dictionary-only half of the decode gate — see [`SamplePlan`]. Touches no stream
/// bytes, so a caller may ask it about every image on a page for free.
fn sample_plan(doc: &Document, res: &Dictionary, dict: &Dictionary) -> Option<SamplePlan> {
    let w = deref(doc, dict.get(b"Width").ok()?)?.as_i64().ok()?;
    let h = deref(doc, dict.get(b"Height").ok()?)?.as_i64().ok()?;
    if w <= 0 || h <= 0 || w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
        return None;
    }
    let (wu, hu) = (w as usize, h as usize);
    if wu.checked_mul(hu)? > MAX_IMAGE_PIXELS {
        return None;
    }
    // A stencil mask has no colour space and exactly one 1-bit sample per pixel (§8.9.6.2).
    let is_mask = dict.get(b"ImageMask").and_then(|o| o.as_bool()).unwrap_or(false);
    let (cs, bpc) = if is_mask {
        (Cs::Gray, 1i64)
    } else {
        let cs = cs_model(doc, res, dict.get(b"ColorSpace").ok()?, 0)?;
        let bpc = image_bpc(doc, dict)?;
        (cs, bpc)
    };
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return None;
    }
    Some(SamplePlan { cs, bpc: bpc as usize, w: wu, h: hu })
}

/// Can this image's *samples* be decoded, judged from its dictionary alone (no stream
/// bytes read, no pixels allocated)? The exact gate [`decode_samples`] applies, minus the
/// one thing that needs the bytes: whether the stream is long enough.
///
/// Only ever over-reports (a truncated stream still says `true`), never under-reports —
/// which is the direction a placeholder count can afford to be wrong in.
pub(crate) fn samples_decodable(doc: &Document, res: &Dictionary, dict: &Dictionary) -> bool {
    sample_plan(doc, res, dict).is_some()
}

/// A decoded sample block, in the narrowest form that holds it without loss: an achromatic
/// space (including an `/Indexed` palette whose base is achromatic, and an `/ImageMask`
/// stencil) stays 8-bit gray, everything else is RGB8.
pub(crate) enum Samples {
    Gray(image::GrayImage),
    Rgb(image::RgbImage),
}

impl Samples {
    pub(crate) fn into_dynamic(self) -> image::DynamicImage {
        match self {
            Samples::Gray(g) => image::DynamicImage::ImageLuma8(g),
            Samples::Rgb(r) => image::DynamicImage::ImageRgb8(r),
        }
    }
    /// Widen to RGB8 — what the render path composites and encodes in.
    pub(crate) fn into_rgb8(self) -> image::RgbImage {
        match self {
            Samples::Gray(g) => image::DynamicImage::ImageLuma8(g).to_rgb8(),
            Samples::Rgb(r) => r,
        }
    }
    /// Narrow to 8-bit gray — what a soft mask (`/SMask`) is, by definition (§8.9.5.4).
    pub(crate) fn into_luma8(self) -> image::GrayImage {
        match self {
            Samples::Gray(g) => g,
            Samples::Rgb(r) => image::DynamicImage::ImageRgb8(r).to_luma8(),
        }
    }
}

/// Decode an image XObject's *samples* — the `format:"raw"` case (Flate/LZW/uncompressed),
/// which is 1167 of 2604 corpus rows.
///
/// Handles 1/2/4/8/16 bits per component, the `/Decode` array, `/ImageMask` stencils, and
/// Gray/RGB/CMYK/ICCBased/Indexed colour spaces — including a space *named* in `res`
/// (`/ColorSpace /CS0`). Returns `None` for anything it cannot reduce faithfully, rather
/// than emitting a confidently wrong picture; a caller then reports the image honestly
/// (`format:"raw"` on the extract path, no `<img>` on the render path).
///
/// Reads the stream through [`crate::pdfobj::content_bytes`], so an **unfiltered** raster
/// keeps its bytes — `decompressed_content()` alone errors when a stream has no `/Filter`,
/// which is how the render path's own copy silently lost every uncompressed image.
pub(crate) fn decode_samples(doc: &Document, res: &Dictionary, stream: &lopdf::Stream) -> Option<Samples> {
    let dict = &stream.dict;
    let SamplePlan { cs, bpc, w: wu, h: hu } = sample_plan(doc, res, dict)?;
    let nc = cs.components();

    let samples = content_bytes(stream);
    // Rows are padded to a byte boundary (§8.9.5.1).
    let stride = (wu.checked_mul(nc)?.checked_mul(bpc)?).div_ceil(8);
    if stride == 0 || samples.len() < stride.checked_mul(hu)? {
        return None; // truncated stream: reassembling it would fabricate pixels
    }

    let maxval = ((1u32 << bpc) - 1) as f32;
    let decode = decode_array(doc, dict, nc);
    let gray_out = cs.is_gray();
    let out_ch = if gray_out { 1 } else { 3 };
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(wu.checked_mul(hu)?.checked_mul(out_ch)?).ok()?;

    // A spot space's transform, evaluated once over the tint grid (see [`tint_lut`]) so the
    // per-pixel cost is one indexed read.
    let lut = match &cs {
        Cs::Tint { k, tint, alt } => Some(tint_lut(*k, tint, alt)),
        _ => None,
    };

    // One pixel's colour-space samples, reused per pixel to avoid a per-pixel allocation.
    let mut comp = vec![0u8; nc.max(4)];
    for row in samples.chunks_exact(stride).take(hu) {
        for x in 0..wu {
            for (j, slot) in comp.iter_mut().enumerate().take(nc) {
                let raw = sample_at(row, x * nc + j, bpc);
                *slot = match &cs {
                    // An index is an index: the default /Decode for Indexed is
                    // [0 2^bpc-1], i.e. the raw value, and a non-default one remaps the
                    // index range rather than a colour intensity.
                    Cs::Indexed { .. } => match &decode {
                        Some(d) => (d[0] + raw as f32 * (d[1] - d[0]) / maxval).round().clamp(0.0, 255.0) as u8,
                        None => raw.min(255) as u8,
                    },
                    _ => {
                        let v = match &decode {
                            Some(d) => d[2 * j] + raw as f32 * (d[2 * j + 1] - d[2 * j]) / maxval,
                            None => raw as f32 / maxval,
                        };
                        (v.clamp(0.0, 1.0) * 255.0).round() as u8
                    }
                };
            }
            // A tint pixel is not a colour to be converted but an index into the evaluated
            // transform: pack the quantised tints first-colorant-fastest and look it up.
            if let Some(lut) = &lut {
                let idx = comp[..nc].iter().rev().fold(0usize, |acc, c| acc * TINT_LEVELS + *c as usize);
                out.extend_from_slice(&lut[idx]);
                continue;
            }
            let px: &[u8] = match &cs {
                Cs::Indexed { base, lookup } => {
                    let bn = base.components();
                    let off = (comp[0] as usize) * bn;
                    match lookup.get(off..off + bn) {
                        Some(s) => s,
                        None => &[0u8; 4][..bn], // index past the palette: black, per §8.6.6.3
                    }
                }
                _ => &comp[..nc],
            };
            let base_cs: &Cs = match &cs {
                Cs::Indexed { base, .. } => base,
                other => other,
            };
            match base_cs {
                Cs::Gray => out.push(px[0]),
                Cs::Rgb => out.extend_from_slice(&px[..3]),
                Cs::Cmyk => {
                    let k = 255 - px[3] as u16;
                    for c in &px[..3] {
                        out.push(((255 - *c as u16) * k / 255) as u8);
                    }
                }
                Cs::Indexed { .. } | Cs::Tint { .. } => return None, // both handled above
            }
        }
    }

    let (w, h) = (wu as u32, hu as u32);
    if gray_out {
        Some(Samples::Gray(image::GrayImage::from_raw(w, h, out)?))
    } else {
        Some(Samples::Rgb(image::RgbImage::from_raw(w, h, out)?))
    }
}

/// A real PNG file from an image XObject's samples: [`decode_samples`] plus the encoder.
///
/// The extract path's container for `format:"png"` — 1167 of 2604 corpus rows, none of
/// which opened as an image file before (the caller got back compressed samples with no
/// container at all).
pub(crate) fn assemble_png(doc: &Document, res: &Dictionary, stream: &lopdf::Stream) -> Option<Vec<u8>> {
    png_bytes(decode_samples(doc, res, stream)?.into_dynamic())
}

/// The `i`-th `bpc`-bit sample of a packed row.
fn sample_at(row: &[u8], i: usize, bpc: usize) -> u32 {
    match bpc {
        8 => row[i] as u32,
        16 => ((row[2 * i] as u32) << 8) | row[2 * i + 1] as u32,
        _ => {
            let bit = i * bpc;
            let shift = 8 - bpc - (bit % 8);
            ((row[bit / 8] >> shift) as u32) & ((1u32 << bpc) - 1)
        }
    }
}

/// Encode to a PNG file. Same writer `img.rs` uses for the HTML data URIs — the `image`
/// crate — so an assembled `extract_images()` blob and an embedded `<img>` agree.
pub(crate) fn png_bytes(img: image::DynamicImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// The codec payload of a coded image (`jpeg`/`jpx`/`ccitt`/`jbig2`): the stream bytes
/// with any leading generic compression peeled off, so a Flate-wrapped JPEG is handed
/// back as a JPEG file and not as a blob nothing can open.
///
/// The filter chain is read from the stream's own dict, so no caller can peel against a
/// list that disagrees with the bytes. Borrows when there is nothing to peel (the common
/// single-codec case), so the passthrough path copies no pixels.
///
/// **Not a decode of the generic layers lopdf cannot apply.** `ASCIIHexDecode`/`AHx` are
/// in the set above, but lopdf 0.40 answers `Unimplemented` for them, so a chain that
/// includes one degrades to the verbatim stream (pinned by
/// `an_ascii_hex_wrapper_degrades_to_the_raw_bytes_because_lopdf_cannot_apply_it`). That
/// degradation is *reported*, not silent: `pdfobj::stream_issues` flags exactly this stream
/// as `filter-unapplied`, and a truncated Flate wrapper — which lopdf reports as `Ok` —
/// as `flate-truncated`.
pub(crate) fn codec_payload(stream: &lopdf::Stream) -> Cow<'_, [u8]> {
    let lead: Vec<Object> = filters_of(&stream.dict)
        .iter()
        .take_while(|f| is_generic_filter(f))
        .map(|f| Object::Name(f.clone()))
        .collect();
    if lead.is_empty() {
        return Cow::Borrowed(&stream.content);
    }
    let mut s = stream.clone();
    s.dict.set("Filter", Object::Array(lead));
    s.dict.remove(b"DecodeParms"); // codec parms don't apply to the generic layers
    s.dict.remove(b"DP");
    match s.decompressed_content() {
        Ok(b) => Cow::Owned(b),
        Err(_) => Cow::Borrowed(&stream.content),
    }
}

/// True when the image dict carries an inverting `/Decode` array (`[1 0 …]`).
///
/// Follows an indirect `/Decode`, which the render path's own copy could not see.
pub(crate) fn decode_inverts(doc: &Document, dict: &Dictionary) -> bool {
    match dict.get(b"Decode").ok().and_then(|o| deref(doc, o)) {
        Some(Object::Array(a)) if a.len() >= 2 => {
            let n = |o: &Object| match o {
                Object::Integer(i) => *i as f32,
                Object::Real(r) => *r,
                _ => 0.0,
            };
            n(&a[0]) > n(&a[1])
        }
        _ => false,
    }
}

/// Component count from a JPEG's SOF (start-of-frame) marker, without decoding pixels.
/// `4` => CMYK/YCCK, `3` => RGB/YCbCr, `1` => grayscale.
pub(crate) fn jpeg_components(buf: &[u8]) -> Option<u8> {
    let mut i = 2; // skip SOI
    while i + 4 <= buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = buf[i + 1];
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            i += 2; // standalone marker, no length field
            continue;
        }
        let len = ((buf[i + 2] as usize) << 8) | buf[i + 3] as usize;
        // SOF0/1/2/3, 5/6/7, 9/10/11, 13/14/15 carry the frame header; DHT/DAC/DNL don't.
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) && i + 9 < buf.len() {
            return Some(buf[i + 9]); // marker(2) len(2) precision(1) height(2) width(2) Nf(1)
        }
        if marker == 0xDA {
            break; // SOS
        }
        i += 2 + len;
    }
    None
}

/// Decode a `DCTDecode` payload to RGB8 under **PDF** semantics, which are not the
/// standalone-JPEG-file semantics every general-purpose decoder implements.
///
/// A JPEG file that carries an Adobe APP14 marker and four components is, by the
/// standalone convention, stored with inverted CMYK — so libjpeg-derived decoders (PIL,
/// `jpeg-decoder`) complement all four channels on the way out. PDF says something else:
/// §7.4.8 makes the filter's output the *sample values*, with the APP14 transform byte
/// selecting only the colour transform (0 none, 1 YCbCr, 2 YCCK), and polarity handled
/// explicitly by the image dict's `/Decode` array — which is exactly why Distiller writes
/// `/Decode [1 0 1 0 1 0 1 0]` next to a Photoshop CMYK JPEG.
///
/// So the decoder's complement has to be undone before `/Decode` is applied, and a
/// document that omits `/Decode` (as `med_crispr_sicklecell_pmc.pdf` does) is the case
/// where handing the raw stream to a consumer silently produced the wrong colour: its
/// 873×116 image read as RGB `(110,116,52)` at (0,0) where the page renders `(17,12,75)`,
/// and its 138×54 logo read black on a white corner.
///
/// `jpeg-decoder` complements both of its 4-component paths (`color_convert_line_cmyk`
/// and the `255 - k` in `color_convert_line_ycck`), so one rule covers CMYK and YCCK.
/// The 1- and 3-component paths do not complement, hence the opposite sense there.
pub(crate) fn decode_dct_rgb(buf: &[u8], decode_inv: bool) -> Option<image::RgbImage> {
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(buf));
    // Read the frame header and reject absurd dimensions BEFORE decoding pixels: a hostile
    // JPEG can declare a huge frame and force a giant allocation.
    dec.read_info().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || w > MAX_IMAGE_DIM as usize || h > MAX_IMAGE_DIM as usize || w * h > MAX_IMAGE_PIXELS {
        return None;
    }
    let px = dec.decode().ok()?;
    let (w, h) = (w as u32, h as u32);
    use jpeg_decoder::PixelFormat::*;
    match dec.info()?.pixel_format {
        L8 => {
            let g = image::GrayImage::from_raw(w, h, maybe_invert(px, decode_inv))?;
            Some(image::DynamicImage::ImageLuma8(g).to_rgb8())
        }
        L16 => None,
        RGB24 => image::RgbImage::from_raw(w, h, maybe_invert(px, decode_inv)),
        CMYK32 => {
            let n = (w as usize) * (h as usize);
            let mut rgb = Vec::new();
            rgb.try_reserve_exact(n * 3).ok()?;
            for i in 0..n {
                let c = |j: usize| {
                    let v = px[i * 4 + j];
                    if decode_inv {
                        v
                    } else {
                        255 - v
                    }
                };
                let k = 255 - c(3) as u16;
                for j in 0..3 {
                    rgb.push(((255 - c(j) as u16) * k / 255) as u8);
                }
            }
            image::RgbImage::from_raw(w, h, rgb)
        }
    }
}

fn maybe_invert(mut px: Vec<u8>, invert: bool) -> Vec<u8> {
    if invert {
        for v in &mut px {
            *v = 255 - *v;
        }
    }
    px
}

/// A normalized PNG for a JPEG whose bytes a consumer would decode to the wrong colour:
/// a 4-component (CMYK/YCCK) stream, or any stream with an inverting `/Decode` the JPEG
/// file itself cannot express. Everything else keeps its lossless JPEG passthrough.
pub(crate) fn normalized_jpeg_png(doc: &Document, dict: &Dictionary, jpeg: &[u8]) -> Option<Vec<u8>> {
    let decode_inv = decode_inverts(doc, dict);
    if jpeg_components(jpeg) != Some(4) && !decode_inv {
        return None;
    }
    png_bytes(image::DynamicImage::ImageRgb8(decode_dct_rgb(jpeg, decode_inv)?))
}

/// `/BitsPerComponent`, or the value the spec implies when the key is absent.
pub(crate) fn image_bpc(doc: &Document, dict: &Dictionary) -> Option<i64> {
    dict.get(b"BitsPerComponent")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_i64().ok())
        // A stencil mask is 1-bit by definition and may omit the key (§8.9.6.2).
        .or_else(|| dict.get(b"ImageMask").and_then(|o| o.as_bool()).unwrap_or(false).then_some(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream, StringFormat};

    fn stream(filters: &[&str], content: Vec<u8>) -> Stream {
        let mut d = Dictionary::new();
        if filters.len() == 1 {
            d.set("Filter", Object::Name(filters[0].as_bytes().to_vec()));
        } else if !filters.is_empty() {
            d.set("Filter", Object::Array(filters.iter().map(|f| Object::Name(f.as_bytes().to_vec())).collect()));
        }
        Stream::new(d, content)
    }

    #[test]
    fn filter_to_format_names_the_codec_and_defaults_to_raw() {
        let f = |v: &[&str]| filter_to_format(&Some(v.iter().map(|s| s.to_string()).collect()));
        assert_eq!(f(&["DCTDecode"]), "jpeg");
        assert_eq!(f(&["JPXDecode"]), "jpx");
        assert_eq!(f(&["CCITTFaxDecode"]), "ccitt");
        assert_eq!(f(&["JBIG2Decode"]), "jbig2");
        // A codec wrapped in generic compression is still that codec, whichever order the
        // chain is written in — the format follows the codec, not the outer layer.
        assert_eq!(f(&["FlateDecode", "DCTDecode"]), "jpeg");
        assert_eq!(f(&["ASCII85Decode", "CCITTFaxDecode"]), "ccitt");
        // Plain samples (and no filter at all) are "raw" — `assemble_png`'s input.
        assert_eq!(f(&["FlateDecode"]), "raw");
        assert_eq!(f(&[]), "raw");
        assert_eq!(filter_to_format(&None), "raw");
    }

    #[test]
    fn the_generic_filter_set_includes_the_ascii_hex_spelling() {
        // The set `img.rs` carries is missing ASCIIHexDecode/AHx; that gap is the reason
        // this predicate has one home. All four abbreviations count too.
        for f in [&b"FlateDecode"[..], b"Fl", b"LZWDecode", b"LZW", b"ASCII85Decode", b"A85", b"ASCIIHexDecode", b"AHx"] {
            assert!(is_generic_filter(f), "{} is a generic (non-codec) filter", String::from_utf8_lossy(f));
        }
        for f in [&b"DCTDecode"[..], b"JPXDecode", b"CCITTFaxDecode", b"JBIG2Decode", b"RunLengthDecode"] {
            assert!(!is_generic_filter(f), "{} is a codec, not a generic layer", String::from_utf8_lossy(f));
        }
    }

    #[test]
    fn codec_payload_peels_the_generic_wrapper_off_a_coded_image() {
        // A JPEG stored `[/ASCII85Decode /DCTDecode]` must come back as the JPEG bytes, not
        // as the ASCII blob nothing can open — the shape reportlab writes.
        let jpeg = b"\xff\xd8\xff\xe0hello";
        let s = stream(&["ASCII85Decode", "DCTDecode"], b"s4IA0BOu!rDZ~>".to_vec());
        assert_eq!(codec_payload(&s).as_ref(), jpeg);
    }

    #[test]
    fn an_ascii_hex_wrapper_degrades_to_the_raw_bytes_because_lopdf_cannot_apply_it() {
        // `ASCIIHexDecode`/`AHx` are in the generic set (that is the set difference this
        // module exists to end), but lopdf 0.40 answers `Unimplemented` for them — so the
        // peel DEGRADES to the verbatim stream instead of producing a decoded payload.
        // Pinned here so no later phase claims a decode this stack does not perform.
        let hex = b"ffd8ffe068656c6c6f>".to_vec();
        let s = stream(&["ASCIIHexDecode", "DCTDecode"], hex.clone());
        assert!(s.decompressed_content().is_err(), "the premise: lopdf cannot apply AHx");
        assert_eq!(codec_payload(&s).as_ref(), hex);
    }

    #[test]
    fn codec_payload_hands_back_the_stream_verbatim_when_there_is_nothing_to_peel() {
        // A bare codec stream: no generic layer, so the bytes ARE the payload.
        let s = stream(&["DCTDecode"], b"\xff\xd8raw jpeg".to_vec());
        assert_eq!(codec_payload(&s).as_ref(), b"\xff\xd8raw jpeg");
        // A stream whose declared Flate wrapper is not actually deflate comes back EMPTY,
        // not raw: lopdf's zlib reader swallows its error and returns the partial output it
        // managed (the same lopdf quirk `pdfobj::content_bytes` documents), so the
        // `unwrap_or_else` fallback never fires for Flate. Stated, not assumed.
        let bad = stream(&["FlateDecode", "DCTDecode"], b"not deflate at all".to_vec());
        assert!(codec_payload(&bad).is_empty());
    }

    #[test]
    fn codec_payload_drops_the_codec_decode_parms_before_peeling() {
        // `/DecodeParms` describes the CODEC (e.g. CCITT `/K -1`), not the generic layer;
        // leaving it on the peeling copy makes lopdf misapply it.
        let mut s = stream(&["ASCII85Decode", "CCITTFaxDecode"], b"88/~>".to_vec()); // "Hi"
        s.dict.set("DecodeParms", dictionary! { "K" => -1i64, "Columns" => 1728i64 });
        assert_eq!(codec_payload(&s).as_ref(), b"Hi");
    }

    #[test]
    fn dims_sane_refuses_zero_oversized_and_bomb_sized_images() {
        assert!(dims_sane(1, 1) && dims_sane(4000, 3000));
        assert!(!dims_sane(0, 10) && !dims_sane(10, 0));
        assert!(dims_sane(MAX_IMAGE_DIM as u32, 1));
        assert!(!dims_sane(MAX_IMAGE_DIM as u32 + 1, 1));
        assert!(!dims_sane(1, MAX_IMAGE_DIM as u32 + 1));
        // Each side legal, the product a 4-gigapixel allocation: refused on the total.
        assert!(!dims_sane(65536, 65536));
        // The product is computed saturating, so it cannot wrap into a "sane" answer.
        assert!(!dims_sane(u32::MAX, u32::MAX));
    }

    #[test]
    fn sample_at_unpacks_every_legal_bit_depth() {
        // 8 and 16 bpc are byte- and word-aligned.
        assert_eq!(sample_at(&[0x12, 0x34], 1, 8), 0x34);
        assert_eq!(sample_at(&[0x12, 0x34], 0, 16), 0x1234);
        // Sub-byte samples pack big-endian, most significant first (§8.9.5.1).
        assert_eq!((0..8).map(|i| sample_at(&[0b1011_0010], i, 1)).collect::<Vec<_>>(), vec![1, 0, 1, 1, 0, 0, 1, 0]);
        assert_eq!((0..4).map(|i| sample_at(&[0b11_01_00_10], i, 2)).collect::<Vec<_>>(), vec![3, 1, 0, 2]);
        assert_eq!((0..4).map(|i| sample_at(&[0xAB, 0xCD], i, 4)).collect::<Vec<_>>(), vec![0xA, 0xB, 0xC, 0xD]);
    }

    #[test]
    fn decode_array_reads_the_right_arity_and_rejects_the_wrong_one() {
        let mut doc = Document::with_version("1.5");
        let mut d = Dictionary::new();
        d.set("Decode", vec![1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into()]);
        assert_eq!(decode_array(&doc, &d, 3), Some(vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0]));
        assert_eq!(decode_array(&doc, &d, 4), None, "a 3-channel array is not a 4-channel one");
        assert_eq!(decode_array(&doc, &Dictionary::new(), 3), None);
        // Written indirectly — legal, and the reader must follow it.
        let id = doc.add_object(Object::Array(vec![0.into(), 1.into()]));
        let mut ind = Dictionary::new();
        ind.set("Decode", Object::Reference(id));
        assert_eq!(decode_array(&doc, &ind, 1), Some(vec![0.0, 1.0]));
        // A non-numeric entry makes the whole array unusable rather than half-read.
        let mut bad = Dictionary::new();
        bad.set("Decode", vec![Object::Name(b"nope".to_vec()), 1.into()]);
        assert_eq!(decode_array(&doc, &bad, 1), None);
    }

    #[test]
    fn decode_inverts_reads_the_polarity_through_an_indirect_array() {
        let mut doc = Document::with_version("1.5");
        let mk = |v: Vec<Object>| {
            let mut d = Dictionary::new();
            d.set("Decode", v);
            d
        };
        assert!(decode_inverts(&doc, &mk(vec![1.into(), 0.into()])));
        assert!(!decode_inverts(&doc, &mk(vec![0.into(), 1.into()])));
        assert!(!decode_inverts(&doc, &Dictionary::new()));
        // `img.rs`'s copy cannot see this shape at all — the reason there is one here.
        let id = doc.add_object(Object::Array(vec![1.into(), 0.into()]));
        let mut ind = Dictionary::new();
        ind.set("Decode", Object::Reference(id));
        assert!(decode_inverts(&doc, &ind), "an indirect /Decode array must be followed");
    }

    #[test]
    fn colour_space_names_resolve_through_the_resource_dictionary_and_terminate() {
        let mut doc = Document::with_version("1.5");
        // A name that is NOT a device space means "look me up in /Resources /ColorSpace".
        let icc = doc.add_object(Stream::new(dictionary! { "N" => 3i64 }, vec![]));
        let cs0 = doc.add_object(Object::Array(vec![Object::Name(b"ICCBased".to_vec()), Object::Reference(icc)]));
        let res = dictionary! { "ColorSpace" => dictionary! { "CS0" => Object::Reference(cs0) } };
        let mut img = Dictionary::new();
        img.set("ColorSpace", Object::Name(b"CS0".to_vec()));
        assert_eq!(image_color_space(&doc, &res, &img).as_deref(), Some("ICCBased"));
        assert!(matches!(cs_model(&doc, &res, img.get(b"ColorSpace").unwrap(), 0), Some(Cs::Rgb)));

        // A device name is never a resource lookup, even if the resources define it.
        for n in [&b"DeviceGray"[..], b"DeviceRGB", b"DeviceCMYK", b"G", b"RGB", b"CMYK", b"Pattern"] {
            assert!(is_builtin_cs_name(n));
        }
        assert!(!is_builtin_cs_name(b"CS0"));
        // Inline-image abbreviations are spelled out; anything else is reported verbatim.
        assert_eq!(canonical_cs_name(b"G"), "DeviceGray");
        assert_eq!(canonical_cs_name(b"I"), "Indexed");
        assert_eq!(canonical_cs_name(b"Sep1"), "Sep1");

        // A name pointing at itself is a cycle: bounded by MAX_CS_DEPTH, never a hang.
        let loopy = dictionary! { "ColorSpace" => dictionary! { "A" => Object::Name(b"A".to_vec()) } };
        let a = Object::Name(b"A".to_vec());
        assert_eq!(resolve_cs(&doc, &loopy, &a, 0), None);
        // A name nothing defines is reported honestly rather than guessed at.
        assert_eq!(resolve_cs(&doc, &Dictionary::new(), &a, 0), Some(&a));
    }

    #[test]
    fn assemble_png_refuses_absurd_dimensions_and_truncated_samples() {
        let doc = Document::with_version("1.5");
        let res = Dictionary::new();
        let rgb8 = |w: i64, h: i64, n: usize| {
            let d = dictionary! { "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8i64, "Width" => w, "Height" => h };
            Stream::new(d, vec![0u8; n])
        };
        // 2x2 RGB8 needs 12 bytes and it has them.
        assert!(assemble_png(&doc, &res, &rgb8(2, 2, 12)).is_some());
        // Non-positive and over-cap dimensions never reach an allocation.
        assert!(assemble_png(&doc, &res, &rgb8(0, 2, 12)).is_none());
        assert!(assemble_png(&doc, &res, &rgb8(2, -1, 12)).is_none());
        assert!(assemble_png(&doc, &res, &rgb8(MAX_IMAGE_DIM + 1, 1, 12)).is_none());
        assert!(assemble_png(&doc, &res, &rgb8(65536, 65536, 12)).is_none());
        // Truncated samples: reassembling them would fabricate pixels, so it declines.
        assert!(assemble_png(&doc, &res, &rgb8(4, 4, 12)).is_none());
        // ...but the truncation is invisible from the dictionary alone, so the CHEAP gate
        // still says yes. It may over-report; it must never under-report.
        assert!(samples_decodable(&doc, &res, &rgb8(4, 4, 12).dict));
        // An unsupported bit depth declines too, rather than guessing a layout.
        let d5 = dictionary! { "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 5i64, "Width" => 2, "Height" => 2 };
        assert!(assemble_png(&doc, &res, &Stream::new(d5.clone(), vec![0u8; 64])).is_none());
        assert!(!samples_decodable(&doc, &res, &d5));
        // …and so does a colour space that cannot be reduced to RGB faithfully.
        let sep = dictionary! { "ColorSpace" => "Separation", "BitsPerComponent" => 8i64, "Width" => 2, "Height" => 2 };
        assert!(assemble_png(&doc, &res, &Stream::new(sep.clone(), vec![0u8; 64])).is_none());
        assert!(!samples_decodable(&doc, &res, &sep), "the cheap gate refuses what the decoder refuses");
    }

    #[test]
    fn an_unfiltered_raster_keeps_its_samples() {
        // Through lopdf 0.43 `decompressed_content()` ERRORED on a stream with no `/Filter`,
        // and the render path's own sample reader used `.ok()?` — so an uncompressed image
        // decoded to nothing and simply never appeared in `to_html`
        // (`tests/fixtures_pdf/undrawn_image.pdf`). Reading through `content_bytes` is what
        // makes the shared decoder immune; lopdf 0.44 now returns the raw content itself,
        // so this asserts the new premise and the same end behaviour.
        let doc = Document::with_version("1.5");
        let d = dictionary! { "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8i64, "Width" => 2, "Height" => 1 };
        let samples = vec![10u8, 20, 30, 40, 50, 60];
        let s = Stream::new(d, samples.clone());
        assert_eq!(
            s.decompressed_content().ok().as_deref(),
            Some(&samples[..]),
            "the premise, lopdf 0.44: an unfiltered stream decodes to its raw samples",
        );
        let px = decode_samples(&doc, &Dictionary::new(), &s).expect("unfiltered samples decode").into_rgb8();
        assert_eq!(px.get_pixel(0, 0).0, [10, 20, 30]);
        assert_eq!(px.get_pixel(1, 0).0, [40, 50, 60]);
    }

    #[test]
    fn an_indexed_palette_decodes_to_its_colours_not_its_indices() {
        // The render path's own decoder had no palette support at all: it guessed the
        // channel count from `samples.len() / (w*h)`, which for a 1-byte-per-pixel Indexed
        // image is 1 — so the palette INDICES were rendered as gray levels.
        let doc = Document::with_version("1.5");
        let cs = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            Object::Name(b"DeviceRGB".to_vec()),
            Object::Integer(2),
            Object::String(vec![255, 0, 0, 0, 0, 255], StringFormat::Literal),
        ]);
        let d = dictionary! { "ColorSpace" => cs, "BitsPerComponent" => 8i64, "Width" => 2, "Height" => 1 };
        let px = decode_samples(&doc, &Dictionary::new(), &Stream::new(d, vec![0, 1]))
            .expect("indexed decodes")
            .into_rgb8();
        assert_eq!(px.get_pixel(0, 0).0, [255, 0, 0], "index 0 is the palette's red, not gray level 0");
        assert_eq!(px.get_pixel(1, 0).0, [0, 0, 255], "index 1 is the palette's blue, not gray level 1");
    }

    #[test]
    fn a_colour_space_named_in_the_resource_dictionary_resolves() {
        // `/ColorSpace /CS0` is the spelling 971 of 2604 corpus images use. The name means
        // nothing without `res`, which is why the decoder takes one.
        let doc = Document::with_version("1.5");
        let res = dictionary! { "ColorSpace" => dictionary! { "CS0" => Object::Name(b"DeviceGray".to_vec()) } };
        let d = dictionary! { "ColorSpace" => Object::Name(b"CS0".to_vec()), "BitsPerComponent" => 8i64, "Width" => 2, "Height" => 1 };
        let s = Stream::new(d, vec![7, 200]);
        assert!(matches!(decode_samples(&doc, &res, &s), Some(Samples::Gray(_))));
        assert!(decode_samples(&doc, &Dictionary::new(), &s).is_none(), "without the scope the name means nothing");
    }

    #[test]
    fn a_sub_byte_depth_and_a_stencil_mask_both_decode() {
        let doc = Document::with_version("1.5");
        // 4-bpc gray, two pixels packed into one byte.
        let d4 = dictionary! { "ColorSpace" => "DeviceGray", "BitsPerComponent" => 4i64, "Width" => 2, "Height" => 1 };
        let g = decode_samples(&doc, &Dictionary::new(), &Stream::new(d4, vec![0x0F]))
            .expect("4-bpc decodes")
            .into_luma8();
        assert_eq!(g.get_pixel(0, 0).0, [0], "high nibble 0");
        assert_eq!(g.get_pixel(1, 0).0, [255], "low nibble 15 -> full white");
        // An /ImageMask stencil has no /ColorSpace and no /BitsPerComponent at all.
        let m = dictionary! { "ImageMask" => true, "Width" => 8, "Height" => 1 };
        let s = Stream::new(m, vec![0b1010_1010]);
        assert!(samples_decodable(&doc, &Dictionary::new(), &s.dict));
        let px = decode_samples(&doc, &Dictionary::new(), &s).expect("stencil decodes").into_luma8();
        assert_eq!(px.get_pixel(0, 0).0, [255]);
        assert_eq!(px.get_pixel(1, 0).0, [0]);
    }

    #[test]
    fn image_bpc_falls_back_to_the_stencil_masks_implied_depth() {
        let mut doc = Document::with_version("1.5");
        let mut d = Dictionary::new();
        assert_eq!(image_bpc(&doc, &d), None, "no key and no mask: unknown, not guessed");
        d.set("ImageMask", true);
        assert_eq!(image_bpc(&doc, &d), Some(1), "a stencil mask is 1-bit by definition");
        let id = doc.add_object(Object::Integer(16));
        let mut ind = Dictionary::new();
        ind.set("BitsPerComponent", Object::Reference(id));
        assert_eq!(image_bpc(&doc, &ind), Some(16), "an indirect /BitsPerComponent resolves");
    }

    #[test]
    fn jpeg_components_reads_the_frame_header_without_decoding() {
        // SOI, then a DQT-shaped segment to be skipped, then SOF0 declaring 3 components.
        let mut buf = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0x00, 0x00];
        buf.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x10, 0x00, 0x10, 0x03]);
        assert_eq!(jpeg_components(&buf), Some(3));
        // The 4-component (CMYK/YCCK) case is the one the polarity rule keys on.
        let mut cmyk = vec![0xFF, 0xD8];
        cmyk.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x14, 0x08, 0x00, 0x10, 0x00, 0x10, 0x04]);
        assert_eq!(jpeg_components(&cmyk), Some(4));
        // A Huffman table marker (0xC4) is in the 0xC0..=0xCF range but is NOT a frame
        // header — reading it as one is how a component count gets invented.
        let dht = vec![0xFF, 0xD8, 0xFF, 0xC4, 0x00, 0x0A, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(jpeg_components(&dht), None);
        assert_eq!(jpeg_components(&[]), None);
        assert_eq!(jpeg_components(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn maybe_invert_is_the_identity_unless_asked() {
        assert_eq!(maybe_invert(vec![0, 1, 254, 255], false), vec![0, 1, 254, 255]);
        assert_eq!(maybe_invert(vec![0, 1, 254, 255], true), vec![255, 254, 1, 0]);
    }

    #[test]
    fn png_bytes_writes_a_real_png_file() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(2, 1, vec![1, 2, 3, 4, 5, 6]).unwrap());
        let out = png_bytes(img).expect("encodes");
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n", "must be a PNG signature, not raw samples");
        let back = image::load_from_memory_with_format(&out, image::ImageFormat::Png).unwrap().to_rgb8();
        assert_eq!(back.get_pixel(0, 0).0, [1, 2, 3]);
        assert_eq!(back.get_pixel(1, 0).0, [4, 5, 6]);
    }

    #[test]
    fn a_normalized_jpeg_is_produced_only_for_the_cases_a_consumer_would_get_wrong() {
        // A 3-component JPEG with no inverting /Decode keeps its lossless passthrough:
        // re-encoding it as PNG would be a needless quality and size loss.
        let doc = Document::with_version("1.5");
        let mut rgb = vec![0xFF, 0xD8];
        rgb.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x10, 0x00, 0x10, 0x03]);
        assert!(normalized_jpeg_png(&doc, &Dictionary::new(), &rgb).is_none());
        // With an inverting /Decode the gate opens (the decode itself then fails on this
        // header-only buffer, which is the honest `None` rather than a fabricated image).
        let mut inv = Dictionary::new();
        inv.set("Decode", vec![1.into(), 0.into()]);
        assert!(normalized_jpeg_png(&doc, &inv, &rgb).is_none());
        assert!(decode_dct_rgb(&rgb, true).is_none(), "a truncated JPEG decodes to nothing, never to noise");
    }

    #[test]
    fn a_pdf_string_palette_and_a_stream_palette_are_read_the_same_way() {
        // `/Indexed` lookup tables come both ways in the wild; a reader that handles only
        // one silently loses the palette and renders indices as gray.
        let mut doc = Document::with_version("1.5");
        let pal = vec![255u8, 0, 0, 0, 255, 0];
        let as_string = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            Object::Name(b"DeviceRGB".to_vec()),
            Object::Integer(1),
            Object::String(pal.clone(), StringFormat::Literal),
        ]);
        let st = doc.add_object(Stream::new(Dictionary::new(), pal.clone()));
        let as_stream = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            Object::Name(b"DeviceRGB".to_vec()),
            Object::Integer(1),
            Object::Reference(st),
        ]);
        for o in [&as_string, &as_stream] {
            match cs_model(&doc, &Dictionary::new(), o, 0) {
                Some(Cs::Indexed { base, lookup }) => {
                    assert_eq!(base.components(), 3);
                    assert_eq!(lookup, pal);
                }
                other => panic!("expected an Indexed palette, got {}", other.is_some()),
            }
        }
        // An Indexed space whose base is itself Indexed is illegal (§8.6.6.3) — declined,
        // not guessed at.
        let nested = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            as_string.clone(),
            Object::Integer(1),
            Object::String(pal, StringFormat::Literal),
        ]);
        assert!(cs_model(&doc, &Dictionary::new(), &nested, 0).is_none());
    }

    /// `[/Separation /Spot alt <function>]`, with `alt` and the transform supplied by the caller.
    fn sep_space(doc: &mut Document, alt: Object, func: Object) -> Object {
        let f = doc.add_object(func);
        Object::Array(vec![Object::Name(b"Separation".to_vec()), Object::Name(b"Spot".to_vec()), alt, Object::Reference(f)])
    }

    /// A Type 2 exponential ramp from white to `c1` — the commonest spot transform there is.
    fn ramp_to(c1: [f32; 3]) -> Object {
        Object::Dictionary(dictionary! {
            "FunctionType" => 2i64,
            "Domain" => Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
            "C0" => Object::Array(vec![Object::Real(1.0), Object::Real(1.0), Object::Real(1.0)]),
            "C1" => Object::Array(c1.iter().map(|v| Object::Real(*v)).collect::<Vec<_>>()),
            "N" => 1i64,
        })
    }

    #[test]
    fn a_separation_image_decodes_through_its_tint_transform_not_as_intensities() {
        // THE defect this pins: a `Separation` image's samples are TINTS. Reading them as
        // intensities makes a 10% spot tint a near-black pixel — the image-path twin of the
        // `scn` grey-level misreading. Both images below carried `format:"raw"` before, so
        // extract handed back compressed samples and the render path emitted no `<img>`.
        let mut doc = Document::with_version("1.5");
        let cs = sep_space(&mut doc, Object::Name(b"DeviceRGB".to_vec()), ramp_to([0.776, 0.776, 0.878]));
        let d = dictionary! { "ColorSpace" => cs, "BitsPerComponent" => 8i64, "Width" => 3, "Height" => 1 };
        // Tints 0 / 0.5 / 1 (0x00, 0x80, 0xff).
        let px = decode_samples(&doc, &Dictionary::new(), &Stream::new(d, vec![0x00, 0x80, 0xff]))
            .expect("a Separation image with an evaluable transform decodes")
            .into_rgb8();
        assert_eq!(px.get_pixel(0, 0).0, [255, 255, 255], "tint 0 is no ink at all: white");
        assert_eq!(px.get_pixel(2, 0).0, [198, 198, 224], "tint 1 is the transform's own colour");
        let mid = px.get_pixel(1, 0).0;
        assert!(mid[0] > 210 && mid[0] < 240 && mid[2] > mid[0], "half tint is PALE lavender, got {mid:?}");
        // The intensity misreading, which this exists to exclude.
        assert_ne!(px.get_pixel(1, 0).0, [128, 128, 128]);
        assert_ne!(px.get_pixel(0, 0).0, [0, 0, 0]);
    }

    #[test]
    fn a_devicen_image_reads_its_colorants_first_fastest() {
        // The sample order a 2-colorant grid pins end to end: `(1, 0)` must be red and
        // `(0, 1)` green, never swapped. The same convention `function.rs`'s Type 0
        // interpolator uses, checked here at the image layer where the packing happens.
        let mut doc = Document::with_version("1.5");
        // 2x2x3 grid, first dimension fastest: (0,0) white, (1,0) red, (0,1) green, (1,1) blue.
        let grid = vec![255u8, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255];
        let f = doc.add_object(Stream::new(
            dictionary! {
                "FunctionType" => 0i64,
                "Domain" => Object::Array(vec![Object::Real(0.0), Object::Real(1.0), Object::Real(0.0), Object::Real(1.0)]),
                "Range" => Object::Array((0..3).flat_map(|_| [Object::Real(0.0), Object::Real(1.0)]).collect::<Vec<_>>()),
                "Size" => Object::Array(vec![Object::Integer(2), Object::Integer(2)]),
                "BitsPerSample" => 8i64,
            },
            grid,
        ));
        let cs = Object::Array(vec![
            Object::Name(b"DeviceN".to_vec()),
            Object::Array(vec![Object::Name(b"A".to_vec()), Object::Name(b"B".to_vec())]),
            Object::Name(b"DeviceRGB".to_vec()),
            Object::Reference(f),
        ]);
        let d = dictionary! { "ColorSpace" => cs, "BitsPerComponent" => 8i64, "Width" => 2, "Height" => 1 };
        let px = decode_samples(&doc, &Dictionary::new(), &Stream::new(d, vec![0xff, 0x00, 0x00, 0xff]))
            .expect("a 2-colorant DeviceN image decodes")
            .into_rgb8();
        assert_eq!(px.get_pixel(0, 0).0, [255, 0, 0], "(1, 0) is the FIRST colorant: red");
        assert_eq!(px.get_pixel(1, 0).0, [0, 255, 0], "(0, 1) is the second: green");
    }

    #[test]
    fn a_spot_image_we_cannot_evaluate_stays_raw_instead_of_guessing_pixels() {
        // The module's standing invariant, unchanged by the tint decoder: no evaluable
        // transform, no picture. A Type 4 calculator, an output arity that disagrees with
        // the alternate space, and a `DeviceN` wider than the tint table all decline.
        let mut doc = Document::with_version("1.5");
        let dims = |cs: Object| dictionary! { "ColorSpace" => cs, "BitsPerComponent" => 8i64, "Width" => 2, "Height" => 1 };

        let type4 = Object::Dictionary(dictionary! {
            "FunctionType" => 4i64,
            "Domain" => Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
            "Range" => Object::Array((0..3).flat_map(|_| [Object::Real(0.0), Object::Real(1.0)]).collect::<Vec<_>>()),
        });
        let cs4 = sep_space(&mut doc, Object::Name(b"DeviceRGB".to_vec()), type4);
        assert!(!samples_decodable(&doc, &Dictionary::new(), &dims(cs4)), "a Type 4 tint transform is not evaluated");

        // A 3-output ramp feeding a 1-component alternate: not a transform to trust.
        let mismatched = sep_space(&mut doc, Object::Name(b"DeviceGray".to_vec()), ramp_to([0.5, 0.5, 0.5]));
        assert!(!samples_decodable(&doc, &Dictionary::new(), &dims(mismatched)));

        // A 3-colorant DeviceN is past the tint table's bound — declined, not decoded slowly.
        let wide = doc.add_object(ramp_to([0.2, 0.4, 0.6]));
        let names: Vec<Object> = [b"A", b"B", b"C"].iter().map(|n| Object::Name(n.to_vec())).collect();
        let csw = Object::Array(vec![
            Object::Name(b"DeviceN".to_vec()),
            Object::Array(names),
            Object::Name(b"DeviceRGB".to_vec()),
            Object::Reference(wide),
        ]);
        assert!(!samples_decodable(&doc, &Dictionary::new(), &dims(csw)));
    }

    #[test]
    fn a_spot_image_over_a_cmyk_alternate_converts_once_the_way_every_other_cmyk_does() {
        // The alternate space may be any device space; a CMYK one must go through the SAME
        // conversion the sample loop applies to a DeviceCMYK image, not a second copy of it.
        let mut doc = Document::with_version("1.5");
        let f = Object::Dictionary(dictionary! {
            "FunctionType" => 2i64,
            "Domain" => Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
            "C0" => Object::Array(vec![Object::Real(0.0); 4]),
            "C1" => Object::Array(vec![Object::Real(0.0), Object::Real(1.0), Object::Real(1.0), Object::Real(0.0)]),
            "N" => 1i64,
        });
        let cs = sep_space(&mut doc, Object::Name(b"DeviceCMYK".to_vec()), f);
        let d = dictionary! { "ColorSpace" => cs, "BitsPerComponent" => 8i64, "Width" => 2, "Height" => 1 };
        let px = decode_samples(&doc, &Dictionary::new(), &Stream::new(d, vec![0x00, 0xff]))
            .expect("a CMYK-alternate Separation decodes")
            .into_rgb8();
        assert_eq!(px.get_pixel(0, 0).0, [255, 255, 255], "0 0 0 0 CMYK is white");
        assert_eq!(px.get_pixel(1, 0).0, [255, 0, 0], "0 1 1 0 CMYK is red");
    }
}
