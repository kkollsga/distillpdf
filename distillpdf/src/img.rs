//! Positioned image extraction for HTML: track the content-stream `Do` operator
//! and CTM to locate each image, then emit a base64 data URI (JPEG passthrough;
//! PNG assembly for sampled raster data).
//!
//! **Pixels are decoded by [`crate::raster`], not here.** This module owns *placement* —
//! the CTM walk, tile clustering, grid stitching, soft-mask compositing and the data-URI
//! container choice — and nothing about what a sample means. It used to carry its own,
//! weaker copy of the decode stack, which is how the render path came to drop or corrupt
//! images `extract_images()` handled correctly. The only decoders left here are the two
//! `raster` does not model: CCITT fax (lopdf cannot apply the filter) and the `/SMask`
//! composite.

use base64::Engine;
use crate::geom::{Mat, Rect};
use crate::pdfobj::{filters_of, num};
use crate::raster::{
    codec_payload, decode_dct_rgb, decode_inverts, decode_samples, dims_sane, jpeg_components, png_bytes,
    samples_decodable, MAX_IMAGE_PIXELS,
};
use crate::vector::ClipRect;
use crate::walker::{
    descend_form, has_subtype, page_resources, page_xobjects, soft_mask_of,
    xobject_at, Descend, PaintSeq, ResourceScope,
    ScopePolicy, SoftMask, XMap,
};
use lopdf::{Dictionary, Object, ObjectId};
#[cfg(test)]
use lopdf::Document;
use std::rc::Rc;

/// True if a DCTDecode stream is a 4-component (CMYK) JPEG — these cannot be passed
/// through to the browser as `image/jpeg` (Adobe CMYK renders inverted/black).
fn jpeg_is_cmyk(content: &[u8]) -> bool {
    jpeg_components(content) == Some(4)
}

/// The `/DecodeParms` fields that govern a CCITTFax stream.
struct CcittParms {
    /// Encoding scheme: `< 0` → Group 4 (T.6, pure 2-D); `>= 0` → Group 3 (T.4).
    k: i64,
    /// Pixels per row (PDF default 1728).
    columns: u32,
    /// Image height in rows; `0` when absent (decode runs to the end-of-block marker).
    rows: u32,
    /// When true, 1 bits are black (inverts the default 0=black mapping).
    black_is1: bool,
}

/// Read the CCITTFax `/DecodeParms` (or the abbreviated `/DP`), which may be a single dict
/// or — when the stream has a filter chain — an array of per-filter dicts.
fn ccitt_parms(
    access: &dyn crate::access::DocumentAccess,
    dict: &Dictionary,
) -> CcittParms {
    let defaults = || CcittParms {
        k: 0,
        columns: 1728,
        rows: 0,
        black_is1: false,
    };
    let from_dict = |params: &Dictionary| {
        let geti = |key: &[u8], default: i64| {
            params
                .get(key)
                .ok()
                .and_then(|value| value.as_i64().ok())
                .unwrap_or(default)
        };
        CcittParms {
            k: geti(b"K", 0),
            columns: geti(b"Columns", 1728).max(0) as u32,
            rows: geti(b"Rows", 0).max(0) as u32,
            black_is1: params
                .get(b"BlackIs1")
                .ok()
                .and_then(|value| value.as_bool().ok())
                .unwrap_or(false),
        }
    };
    let Ok(raw) = dict.get(b"DecodeParms").or_else(|_| dict.get(b"DP")) else {
        return defaults();
    };
    crate::access::read_resolved(access, raw, |resolved| match resolved {
        Object::Dictionary(params) => Some(from_dict(params)),
        Object::Array(params) => params.iter().find_map(|value| {
            crate::access::read_resolved(access, value, |value| {
                value.as_dict().ok().map(from_dict)
            })
            .ok()
            .flatten()
        }),
        _ => None,
    })
    .ok()
    .flatten()
    .unwrap_or_else(defaults)
}

/// Decode a CCITT Group 3/4 fax image (the encoding of most black-and-white PDF scans) to
/// grayscale. lopdf cannot apply this filter, so the raw (encoded) stream bytes are decoded
/// here via the pure-Rust `fax` crate. Honors `/DecodeParms` (`K`, `Columns`, `Rows`,
/// `BlackIs1`) and an inverting `/Decode` array. `content` must be the raw CCITT bitstream.
fn decode_ccitt(
    access: &dyn crate::access::DocumentAccess,
    dict: &Dictionary,
    content: &[u8],
) -> Option<image::GrayImage> {
    use fax::decoder::{decode_g3, decode_g4, pels};
    use fax::Color;

    let parms = ccitt_parms(access, dict);
    let cols = parms.columns;
    let img_h = dict.get(b"Height").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0).max(0) as u32;
    let rows = if parms.rows > 0 { parms.rows } else { img_h };
    if cols == 0 || !dims_sane(cols, rows.max(1)) {
        return None;
    }
    let width = cols.min(u16::MAX as u32) as u16;

    // Default (BlackIs1=false): a fax-"black" pel is a black pixel (0). A `/Decode [1 0]`
    // array flips the mapping; the two inversions compose.
    let invert = parms.black_is1 ^ decode_inverts(access, dict);
    let (black, white) = if invert { (255u8, 0u8) } else { (0u8, 255u8) };

    let mut buf: Vec<u8> = Vec::new();
    let cap = (cols as usize).saturating_mul(rows.max(1) as usize).min(MAX_IMAGE_PIXELS);
    buf.try_reserve(cap).ok()?;
    let mut emit = |line: &[u16]| {
        for c in pels(line, width) {
            buf.push(match c {
                Color::Black => black,
                Color::White => white,
            });
        }
    };
    // On a truncated/corrupt stream the decoder stops early; keep the rows it produced
    // (a partial scan still OCRs) rather than discarding the page.
    if parms.k < 0 {
        // Group 4: known height lets the decoder pad omitted trailing white rows.
        let max_rows = if rows > 0 { Some(rows.min(u16::MAX as u32) as u16) } else { None };
        let _ = decode_g4(content.iter().copied(), width, max_rows, &mut emit);
    } else {
        // Group 3 (rare in PDFs): rows are delimited in-band.
        let _ = decode_g3(content.iter().copied(), &mut emit);
    }

    let stride = cols as usize;
    if stride == 0 || buf.len() < stride {
        return None;
    }
    let h = (buf.len() / stride) as u32;
    buf.truncate(stride * h as usize);
    image::GrayImage::from_raw(cols, h, buf)
}

/// Decode an image stream to RGB8: JPEG (DCTDecode) and CCITT fax through their codecs,
/// everything else through the shared sample decoder ([`decode_samples`]). Returns `None`
/// for a format we cannot reduce faithfully — the image is then reported by nobody rather
/// than rendered wrong.
///
/// `res` is the resource scope the image was drawn in; a `/ColorSpace /CS0` names an entry
/// there and describes nothing without it (§8.6.3).
fn decode_rgb(
    access: &dyn crate::access::DocumentAccess,
    res: &ResourceScope,
    id: ObjectId,
) -> Option<image::RgbImage> {
    access.stream(id).ok()?.read(|stream| {
        let dict = &stream.dict;
        let filters = filters_of(dict);
        if filters.iter().any(|f| f == b"DCTDecode") {
            return decode_dct_rgb(&codec_payload(stream), decode_inverts(access, dict));
        }
        if filters.iter().any(|f| f == b"CCITTFaxDecode") {
            // Fax bitstreams are 1-bpc gray; lopdf can't apply the filter, so decode the codec
            // payload here (peeling any Flate wrapper first), then widen gray → RGB.
            return decode_ccitt(access, dict, &codec_payload(stream))
                .map(|g| image::DynamicImage::ImageLuma8(g).to_rgb8());
        }
        if filters.iter().any(|f| f == b"JPXDecode" || f == b"JBIG2Decode") {
            return None;
        }
        // Plain samples. This used to be a private, weaker decoder: 8 bpc only, no palette, no
        // `/Decode`, no stencil, and — when the colour space was anything it did not model — a
        // channel count GUESSED from `raw.len() / (w*h)`, which renders an Indexed image's
        // palette indices as gray levels. It also read the bytes with `decompressed_content()`,
        // which errors on an unfiltered stream, so every uncompressed raster vanished.
        Some(decode_samples(access, res, stream)?.into_rgb8())
    })?
}

/// Build a base64 PNG data URI for an inline image's already-parsed samples.
///
/// Deliberately narrower than [`data_uri`]: an inline image can carry no `/SMask` and the
/// parser hands it over unfiltered, so the whole decode is the shared sample path —
/// stencils (`/IM true`), sub-byte depths and `/Decode` included.
fn inline_data_uri(
    access: &dyn crate::access::DocumentAccess,
    res: &ResourceScope,
    stream: &lopdf::Stream,
    window: Option<(f32, f32, f32, f32)>,
    turn: i32,
) -> Option<String> {
    let rgb = turn_pixels(crop_window(decode_samples(access, res, stream)?.into_rgb8(), window), turn);
    let png = png_bytes(image::DynamicImage::ImageRgb8(rgb))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Some(format!("data:image/png;base64,{}", b64.encode(&png)))
}

/// Decode the soft mask (`/SMask`) of an image to a grayscale alpha channel.
fn decode_smask(
    access: &dyn crate::access::DocumentAccess,
    res: &ResourceScope,
    dict: &Dictionary,
) -> Option<image::GrayImage> {
    let sid = dict.get(b"SMask").ok().and_then(|o| o.as_reference().ok())?;
    access.stream(sid).ok()?.read(|stream| {
        let sd = &stream.dict;
        let filters = filters_of(sd);
        if filters.iter().any(|f| f == b"DCTDecode") {
            let mut g = image::load_from_memory_with_format(&codec_payload(stream), image::ImageFormat::Jpeg)
                .ok()?
                .to_luma8();
            // A soft mask carries its own `/Decode`, and `[1 0]` flips the alpha ramp
            // (§8.9.5.2) — a polarity the JPEG file cannot express, so it is applied here.
            if decode_inverts(access, sd) {
                for v in g.iter_mut() {
                    *v = 255 - *v;
                }
            }
            return Some(g);
        }
        if filters.iter().any(|f| f == b"JPXDecode" || f == b"CCITTFaxDecode" || f == b"JBIG2Decode") {
            return None;
        }
        // The shared decoder applies the mask's own `/Decode` on this path too, and handles the
        // sub-byte depths a soft mask is often written at (a 1-bpc knockout mask).
        Some(decode_samples(access, res, stream)?.into_luma8())
    })?
}

/// Cheap (no-decode) test that an image XObject is a format we can render — used in
/// placeholder mode so a `<image N>` stands in only for an image that inline mode would
/// actually emit.
///
/// The sample half is [`samples_decodable`], the very gate the decoder applies minus the
/// one thing that needs the bytes (whether the stream is long enough). It used to be an
/// independently-written `bpc == 8` guess that disagreed with the decoder in both
/// directions.
fn decodable(
    access: &dyn crate::access::DocumentAccess,
    res: &ResourceScope,
    id: ObjectId,
) -> bool {
    access.stream(id).ok().and_then(|stream| stream.read(|stream| {
        let dict = &stream.dict;
        let filters = filters_of(dict);
        if filters.iter().any(|f| f == b"JPXDecode" || f == b"JBIG2Decode") {
            return false;
        }
        if filters.iter().any(|f| f == b"DCTDecode" || f == b"CCITTFaxDecode") {
            return true; // JPEG / CCITT fax: renderable (fax via decode_ccitt)
        }
        samples_decodable(access, res, dict)
    })).unwrap_or(false)
}

/// Build a base64 data URI for an image stream, or None if unsupported.
///
/// Images with a soft mask (`/SMask`) are alpha-composited so transparency is
/// preserved — without this, masked figures (whose visible content lives in the
/// mask, over a flat-colour/black base) render as solid black boxes.
fn data_uri(
    access: &dyn crate::access::DocumentAccess,
    res: &ResourceScope,
    id: ObjectId,
    window: Option<(f32, f32, f32, f32)>,
    turn: i32,
) -> Option<String> {
    access.stream(id).ok()?.read(|stream| {
    let dict = &stream.dict;
    let b64 = base64::engine::general_purpose::STANDARD;
    let has_smask = dict.get(b"SMask").and_then(|o| o.as_reference()).is_ok();
    let filters = filters_of(dict);

    // No soft mask: keep JPEG passthrough (cheap, lossless), assemble PNG for
    // Flate raster, bail on formats we can't decode.
    if !has_smask {
        if filters.iter().any(|f| f == b"DCTDecode") {
            // Peel any generic wrapper (e.g. [FlateDecode, DCTDecode]) to get the raw JPEG.
            let jpeg = codec_payload(stream);
            // A JPEG passes straight through (cheap, lossless) only when its own bytes
            // decode to the authored colour. Two kinds do not, and both are re-encoded:
            //   * CMYK — browsers apply the Adobe complement and render it inverted/black;
            //   * ANY stream whose image dict carries an inverting `/Decode` — a polarity
            //     the JPEG file cannot express, so a passthrough renders the NEGATIVE of
            //     the authored image. This is the same gate `raster::normalized_jpeg_png`
            //     applies on the extract path; the render path used to apply the `/Decode`
            //     half to CMYK only, so a gray or RGB JPEG with `/Decode [1 0 …]` came out
            //     inverted in `to_html`.
            // A clip is the third reason the bytes cannot pass straight through: the
            // passthrough hands over the WHOLE authored image, and the page shows a window of it.
            let inv = decode_inverts(access, dict);
            if inv || window.is_some() || turn != 0 || jpeg_is_cmyk(&jpeg) {
                let rgb = decode_dct_rgb(&jpeg, inv)?;
                return jpeg_uri(turn_pixels(crop_window(rgb, window), turn));
            }
            return Some(format!("data:image/jpeg;base64,{}", b64.encode(&jpeg)));
        }
        if filters.iter().any(|f| f == b"JPXDecode" || f == b"JBIG2Decode") {
            return None;
        }
        // CCITTFax falls through to decode_rgb (which decodes it via decode_ccitt) → PNG.
        let rgb = turn_pixels(crop_window(decode_rgb(access, res, id)?, window), turn);
        let png = png_bytes(image::DynamicImage::ImageRgb8(rgb))?;
        return Some(format!("data:image/png;base64,{}", b64.encode(&png)));
    }

    // Soft mask present: decode base + mask, composite to RGBA, emit PNG.
    let base = decode_rgb(access, res, id)?;
    let (w, h) = (base.width(), base.height());
    let mask = decode_smask(access, res, dict);
    let mut rgba = image::RgbaImage::new(w, h);
    let resized;
    let mask_ref = match &mask {
        Some(m) if m.width() == w && m.height() == h => Some(m),
        Some(m) => {
            resized = image::imageops::resize(m, w, h, image::imageops::FilterType::Triangle);
            Some(&resized)
        }
        None => None,
    };
    // Composite over raw buffers (base RGB → RGBA + mask alpha) instead of per-pixel
    // get_pixel/put_pixel, which bounds-check every access. Linear indexing over the
    // contiguous samples is markedly faster on the large rasters this path handles.
    let mut max_a = 0u8;
    let (mut cmin, mut cmax) = ([255u8; 3], [0u8; 3]);
    let base_raw: &[u8] = &base;
    let mask_raw: Option<&[u8]> = mask_ref.map(|m| -> &[u8] { m });
    let out_raw: &mut [u8] = &mut rgba;
    let n = (w as usize) * (h as usize);
    for i in 0..n {
        let (cr, cg, cb) = (base_raw[i * 3], base_raw[i * 3 + 1], base_raw[i * 3 + 2]);
        let a = mask_raw.map(|m| m[i]).unwrap_or(255);
        if a > max_a {
            max_a = a;
        }
        if a > 0 {
            let c = [cr, cg, cb];
            for k in 0..3 {
                if c[k] < cmin[k] {
                    cmin[k] = c[k];
                }
                if c[k] > cmax[k] {
                    cmax[k] = c[k];
                }
            }
        }
        let o = i * 4;
        out_raw[o] = cr;
        out_raw[o + 1] = cg;
        out_raw[o + 2] = cb;
        out_raw[o + 3] = a;
    }
    // Drop content-free overlays rather than emit them:
    //   * fully faint (max alpha < 40) — an invisible anti-alias / shadow layer;
    //   * a flat single-colour panel that never reaches strong opacity — a tint /
    //     drop-shadow fill (real line-art/photos hit full opacity somewhere).
    let spread = (0..3).map(|k| cmax[k].saturating_sub(cmin[k])).max().unwrap_or(0);
    if max_a < 40 || (spread < 8 && max_a < 160) {
        return None;
    }
    // Match the source format: a JPEG base with a trivial (all-opaque) mask becomes a
    // compact JPEG; a mask with real transparency stays a lossless PNG.
    rgba_uri(turn_pixels(crop_window(rgba, window), turn), filters.iter().any(|f| f == b"DCTDecode"))
    })?
}

/// Turn a decoded raster CLOCKWISE by a page's `/Rotate`, so the emitted pixels are the ones a
/// reader displays.
///
/// P1 turned the rasters *composited into* a figure's `<svg>`; one that is not inside a vector
/// figure goes out as a plain `<img>`, which applies no transform of any kind — so a photo on a
/// `/Rotate 90` page came out sideways. Turning the samples, exactly as [`crop_window`] cuts
/// them, is what makes the answer the same in `<img>`, in an `<svg>` `<image>`, and in any
/// consumer that lifts the data URI out of the HTML. `rot == 0` returns the buffer untouched, so
/// an upright page is byte-identical.
fn turn_pixels<P>(img: image::ImageBuffer<P, Vec<P::Subpixel>>, rot: i32) -> image::ImageBuffer<P, Vec<P::Subpixel>>
where
    P: image::Pixel + 'static,
{
    match rot {
        90 => image::imageops::rotate90(&img),
        180 => image::imageops::rotate180(&img),
        270 => image::imageops::rotate270(&img),
        _ => img,
    }
}

/// The unit-square remap a raster's placement matrix needs once [`turn_pixels`] has turned its
/// samples: with the pixels already in display orientation, the matrix must map the *turned*
/// unit square to the same page-space rect, so every consumer sees the same pixel land in the
/// same place as before.
///
/// A PDF image's unit square has `(0, 0)` bottom-left with its FIRST pixel row at the top
/// (`v = 1`). Turning the displayed image clockwise by 90° sends the sample at old `(u, v)` to
/// new `(v, 1 - u)`, i.e. old `(u, v) = (1 - v', u')` — the matrix below. 180° and 270° follow
/// the same substitution.
fn turn_unit_square(rot: i32) -> Option<Mat> {
    match rot {
        90 => Some(Mat { a: 0.0, b: 1.0, c: -1.0, d: 0.0, e: 1.0, f: 0.0 }),
        180 => Some(Mat { a: -1.0, b: 0.0, c: 0.0, d: -1.0, e: 1.0, f: 1.0 }),
        270 => Some(Mat { a: 0.0, b: -1.0, c: 1.0, d: 0.0, e: 0.0, f: 1.0 }),
        _ => None,
    }
}

/// The placement matrix for a raster whose pixels [`turn_pixels`] has turned by `rot`, given the
/// matrix (or, absent one, the axis-aligned rect) that placed the untouched samples.
fn turned_placement(ctm: Option<[f32; 6]>, rect: (f32, f32, f32, f32), rot: i32) -> Option<[f32; 6]> {
    let turn = match turn_unit_square(rot) {
        Some(t) => t,
        None => return ctm, // upright: exactly what the walk recorded, association included
    };
    // Without a placement matrix the image's own is the implicit `[w 0 0 h x0 y0]` that
    // stretches its unit square over the placement rect.
    let (x0, y0, x1, y1) = rect;
    let base = match ctm {
        Some([a, b, c, d, e, f]) => Mat { a, b, c, d, e, f },
        None => Mat { a: x1 - x0, b: 0.0, c: 0.0, d: y1 - y0, e: x0, f: y0 },
    };
    let m = turn.mul(base);
    Some([m.a, m.b, m.c, m.d, m.e, m.f])
}

/// Cut a decoded raster down to the sub-rectangle its clip leaves visible
/// ([`RawTile::window`], in unit-square fractions), or hand it back untouched when nothing was
/// cropped.
///
/// Cropping the SAMPLES, rather than masking the element, is what makes the answer the same
/// everywhere the URI is used: a plain `<img>` on the page has nowhere to put a `clip-path`, and
/// a consumer that lifts the data URI out of the HTML has nowhere at all. An axis-aligned
/// placement is the only case this can serve — a page-space clip on a ROTATED placement is not a
/// pixel rectangle, and those keep the SVG mask ([`Placed::clip`]).
fn crop_window<P>(img: image::ImageBuffer<P, Vec<P::Subpixel>>, window: Option<(f32, f32, f32, f32)>) -> image::ImageBuffer<P, Vec<P::Subpixel>>
where
    P: image::Pixel + 'static,
{
    let (u0, v0, u1, v1) = match window {
        Some(w) => w,
        None => return img,
    };
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x0 = (u0 * w).round().clamp(0.0, w) as u32;
    let x1 = (u1 * w).round().clamp(0.0, w) as u32;
    // Image row 0 is the TOP row, i.e. `v = 1` — so the vertical span inverts.
    let y0 = ((1.0 - v1) * h).round().clamp(0.0, h) as u32;
    let y1 = ((1.0 - v0) * h).round().clamp(0.0, h) as u32;
    if x1 <= x0 || y1 <= y0 {
        return img; // a sub-pixel window: keep the image rather than emit nothing
    }
    let mut img = img;
    image::imageops::crop(&mut img, x0, y0, x1 - x0, y1 - y0).to_image()
}

/// True if the image XObject is DCTDecode (a JPEG) at source.
fn jpeg_source(access: &dyn crate::access::DocumentAccess, id: ObjectId) -> bool {
    access.stream(id)
        .ok()
        .and_then(|stream| stream.read(|stream| filters_of(&stream.dict).iter().any(|f| f == b"DCTDecode")))
        .unwrap_or(false)
}

/// Base64 JPEG data URI (quality 85) for an RGB image.
fn jpeg_uri(rgb: image::RgbImage) -> Option<String> {
    let mut out = std::io::Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    image::DynamicImage::ImageRgb8(rgb).write_with_encoder(enc).ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(out.into_inner())))
}

/// True when a composited raster's alpha carries no information — every pixel is opaque
/// (within JPEG/resample noise), so flattening it onto any background is a no-op.
///
/// This is the ONLY condition under which a soft-masked raster may be flattened. The test
/// this replaced was "is the source a JPEG", which discards the mask outright; a test on
/// whether the mask is *soft* would be just as wrong — the corpus case
/// (`cs_CV_2606_02580` p9, `/Im1` = obj 715) is a cut-out mask that is 85% fully
/// transparent, 14% fully opaque and 0.7% anything else, i.e. as BINARY as a mask gets, and
/// it is precisely those 85% that must stay transparent for the panel behind them to show.
fn alpha_is_opaque(img: &image::RgbaImage) -> bool {
    let raw: &[u8] = img.as_raw();
    // 250, not 255: a mask decoded from a DCT stream or resampled to the base's dimensions
    // carries a few LSBs of noise, and re-encoding a photograph as PNG to honour that is
    // pure size for no visible transparency.
    !raw.chunks_exact(4).any(|p| p[3] < 250)
}

/// Encode a (possibly composited) RGBA raster to a data URI, matching the SOURCE format.
/// JPEG-sourced content whose mask leaves it fully opaque is flattened and re-encoded as
/// JPEG — far smaller than lossless PNG for photographic content, and it adds no new
/// quality loss over the already-lossy source. Anything with real transparency keeps its
/// alpha in a lossless PNG, whatever the source format.
///
/// The white-flatten this replaced was justified by "the HTML render background, so any
/// edge/feather transparency from a soft mask looks identical". That premise holds for a
/// raster laid on the page and fails inside a figure's `<svg>`, where what sits behind the
/// raster is the figure's own coloured panel — and the flattening paints over it. The
/// transparency is the *authored* compositing, so it is kept in both places rather than
/// decided per emission site: the two paths must not disagree about the same image.
fn rgba_uri(img: image::RgbaImage, jpeg_src: bool) -> Option<String> {
    if jpeg_src && alpha_is_opaque(&img) {
        let (w, h) = (img.width(), img.height());
        let mut rgb = image::RgbImage::new(w, h);
        let src: &[u8] = img.as_raw();
        let dst: &mut [u8] = &mut rgb;
        for i in 0..(w as usize * h as usize) {
            let a = src[i * 4 + 3] as u32;
            for k in 0..3 {
                let c = src[i * 4 + k] as u32;
                dst[i * 3 + k] = ((c * a + 255 * (255 - a)) / 255) as u8; // over white
            }
        }
        return jpeg_uri(rgb);
    }
    let png = png_bytes(image::DynamicImage::ImageRgba8(img))?;
    Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&png)))
}

/// A raster image placed on the page: its bounding box (top/bottom for reading order,
/// left/right for detecting a vector overlay that annotates it) and data URI. A merged
/// grid carries the union bbox and one stitched image.
pub struct Placed {
    pub y_top: f32,
    pub y_bottom: f32,
    pub x_left: f32,
    pub x_right: f32,
    pub uri: String,
    /// The image's full placement matrix `[a,b,c,d,e,f]` (PDF page space, y up) when it is
    /// ROTATED (non-axis-aligned CTM) — e.g. a "Temp (Celsius)" axis label flattened to a
    /// raster and placed sideways. `None` for the common axis-aligned case (then the bbox
    /// alone places it). Used to emit a matching SVG transform instead of stretching the
    /// pixels into the axis-aligned box.
    pub ctm: Option<[f32; 6]>,
    /// Where this raster was painted in the page's content tree ([`PaintSeq`]). Carried so
    /// that a figure compositing this raster with vector ink can interleave the two by
    /// paint order instead of assuming rasters always go behind — an opaque panel the
    /// stream paints *after* an image covers it, and the reverse is just as common.
    /// A stitched grid takes the address of its EARLIEST tile: the mosaic occupies the
    /// span from its first paint onward, so anything painted before all of it is behind it.
    pub(crate) seq: PaintSeq,
    /// The page-space clipping rectangle in force where this raster was `Do`ne, kept ONLY
    /// when it actually cropped the placement. The bbox above is then the CROPPED extent —
    /// what the page shows, which is what reading order, figure absorption and the composite
    /// viewBox must measure — while `ctm` still describes the FULL placement, because the
    /// pixels have not moved. (`vector::finish` sets the precedent: crop the stored bbox,
    /// keep the full geometry, mask it at render time.)
    pub(crate) clip: Option<ClipRect>,
}

/// Where a tile's pixels come from.
enum TileSrc {
    /// A named image XObject — resolvable through the document by id.
    Object(ObjectId),
    /// An inline image (`BI…ID…EI`, §8.9.7), its samples already parsed out of the content
    /// stream (dict normalized to full XObject keys — see
    /// [`crate::raster::normalize_inline_image`]). `None` when the parser could not take
    /// the samples (a filtered inline image): the placement is still known, so the page
    /// shows an honest placeholder instead of silently losing the raster.
    Inline(Option<Rc<lopdf::Stream>>),
}

/// One placed image before clustering: its source, placed bbox (page points),
/// and source pixel WIDTH (for the stitch resolution). Collected by `walk`, then grouped
/// by `finalize`.
struct RawTile {
    src: TileSrc,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    pw: u32,
    ctm: Option<[f32; 6]>, // placement matrix when rotated (see Placed::ctm)
    /// The resource scope in force where this tile was drawn — the page's merged
    /// dictionary, or a form's own `/Resources` overlaid on it. Decoding needs it because
    /// an image may name its colour space (`/ColorSpace /CS0`) rather than declare one
    /// (§8.6.3), and the name resolves only here. Shared, not cloned, per tile.
    res: Rc<ResourceScope>,
    /// Paint position of this tile's `Do` in the page's content tree (see [`Placed::seq`]).
    seq: PaintSeq,
    /// The page-space clip in force at the `Do`, when it actually cropped this tile. The bbox
    /// above is then already the CROPPED one.
    clip: Option<ClipRect>,
    /// The UNCROPPED placement bbox — where the tile's pixels actually land. Equal to the bbox
    /// above unless a clip cropped it, and the frame the crop is expressed as a fraction of.
    full: (f32, f32, f32, f32),
}

/// A resolved image placement: the cropped bbox, the crop that produced it (when one did),
/// the rotation matrix for a non-axis-aligned CTM, and the uncropped frame. `None` when the
/// placement is wholly clipped away or below [`MIN_DIM`] (a diagram tile / rule / icon).
/// Shared by the `Do` and `BI` arms of [`walk`], which place their unit squares identically.
struct ImgPlacement {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    crop: Option<ClipRect>,
    ctm: Option<[f32; 6]>,
    full: (f32, f32, f32, f32),
}

fn image_placement(ctm: Mat, clip: Option<ClipRect>, smask: Option<ClipRect>) -> Option<ImgPlacement> {
    // Placed bbox = image unit square [0,1]^2 through the CTM.
    let corners = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
    let mut bb = Rect::EMPTY;
    for (u, v) in corners {
        let (px, py) = ctm.apply(u, v);
        bb.include(px, py);
    }
    let Rect { mut x0, mut y0, mut x1, mut y1 } = bb;
    // What the page actually shows is the clip AND the soft-mask window; the
    // two are independent restrictions on the same paint, so they intersect.
    let visible = match smask {
        Some(w) => Some(crate::vector::intersect_clip(clip, w)),
        None => clip,
    };
    // Honour the clip in force, on `vector::finish`'s discipline: keep it only
    // when it actually crops (so the ubiquitous full-page `re W n` costs
    // nothing), crop the recorded extent to what shows, and drop outright a
    // raster that lies wholly outside its clip — the page never showed it.
    let mut crop = None;
    if let Some((cx0, cy0, cx1, cy1)) = visible {
        if cx0 > x0 + 0.5 || cy0 > y0 + 0.5 || cx1 < x1 - 0.5 || cy1 < y1 - 0.5 {
            let n = Rect::new(x0, y0, x1, y1).intersect(Rect::new(cx0, cy0, cx1, cy1));
            if n.x1 <= n.x0 || n.y1 <= n.y0 {
                return None;
            }
            x0 = n.x0;
            y0 = n.y0;
            x1 = n.x1;
            y1 = n.y1;
            crop = visible;
        }
    }
    let (w, h) = (x1 - x0, y1 - y0);
    if w < MIN_DIM || h < MIN_DIM {
        return None; // diagram tile / rule / icon — not a figure
    }
    // A ROTATED placement (non-axis-aligned CTM) would render mangled if we
    // just stretched the pixels into this axis-aligned bbox — keep the matrix
    // so the emitter can rotate it. Axis-aligned (the common case) → None.
    let scale = ctm.a.abs().max(ctm.b.abs()).max(ctm.c.abs()).max(ctm.d.abs()).max(1e-6);
    let rot_ctm = if ctm.b.abs() > 0.01 * scale || ctm.c.abs() > 0.01 * scale {
        Some([ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f])
    } else {
        None
    };
    Some(ImgPlacement { x0, y0, x1, y1, crop, ctm: rot_ctm, full: (bb.x0, bb.y0, bb.x1, bb.y1) })
}

impl RawTile {
    /// This tile's document object id — `None` for an inline image, whose samples live in
    /// the content stream rather than behind a resolvable id.
    fn oid(&self) -> Option<ObjectId> {
        match self.src {
            TileSrc::Object(id) => Some(id),
            TileSrc::Inline(_) => None,
        }
    }

    /// The sub-rectangle of this tile's PIXELS that its clip leaves visible, as fractions of
    /// the image's unit square `(u0, v0, u1, v1)` — `None` when nothing was cropped.
    ///
    /// A PDF image's unit square has `(0, 0)` bottom-left with its FIRST pixel row at the top
    /// (`v = 1`), so the caller converts `v` to a row index as `(1 - v) * height`.
    fn window(&self) -> Option<(f32, f32, f32, f32)> {
        self.clip?;
        let (fx0, fy0, fx1, fy1) = self.full;
        let (w, h) = (fx1 - fx0, fy1 - fy0);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        Some((((self.x0 - fx0) / w).clamp(0.0, 1.0), ((self.y0 - fy0) / h).clamp(0.0, 1.0),
              ((self.x1 - fx0) / w).clamp(0.0, 1.0), ((self.y1 - fy0) / h).clamp(0.0, 1.0)))
    }
}

/// Images smaller than this (rendered, points) are diagram tiles / rules /
/// icons rather than figures — dropped so they don't flood the output with
/// hundreds of fragments (a single figure is often a mosaic of tiny tiles).
const MIN_DIM: f32 = 24.0;

/// Minimum tiles in a cluster for it to be treated as a mergeable image grid.
const MIN_GRID_TILES: usize = 4;

/// Positioned images on a page, top-to-bottom. Recurses into Form XObjects
/// (which carry their own content + resources + /Matrix). Tiny tiles are
/// filtered out (see [`MIN_DIM`]).
///
/// `want_uris`: when false (placeholder mode), the image is located but NOT decoded
/// or base64-encoded — `uri` is left empty. Decoding/encoding the raster is by far the
/// dominant cost on image-heavy PDFs, so this makes `images=False` near-free.
pub fn positioned_images(
    access: &dyn crate::access::DocumentAccess,
    page_id: ObjectId,
    want_uris: bool,
) -> Vec<Placed> {
    let content = match access
        .page_content(page_id)
        .ok()
        .and_then(|bytes| lopdf::content::Content::decode(&bytes).ok())
    {
        Some(content) => content,
        None => return Vec::new(),
    };
    // The page's XObjects over its WHOLE resource chain, not just the nearest dictionary
    // (see `walker::page_resource_chain`) — a raster a producer left on an outer node of
    // the page tree used to resolve to nothing and simply not appear.
    let xmap = page_xobjects(access, page_id);
    // The same chain, folded into one dictionary: what a `/ColorSpace /CS0` on any of this
    // page's images has to be looked up in.
    let res = Rc::new(page_resources(access, page_id));
    let mut raws: Vec<RawTile> = Vec::new();
    let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
    walk(access, &content.operations, &xmap, &res, Mat::ID, None, None, &mut raws, 0, &mut budget, &[]);
    // §12.5.5: an annotation's appearance stream is page content a viewer paints ON TOP of
    // the content stream — and it is reachable from neither that stream nor the page's
    // `/Resources`. Addressed past the last page operation so paint order puts it there.
    for (k, (_, ap, ctm)) in crate::walker::placed_appearances(access, page_id).into_iter().enumerate() {
        // The appearance's resources are its OWN (§12.5.5), so the scope it descends from is
        // empty — this walk's `OverlayParent` against nothing, which is `OwnOnly` except it
        // still runs an appearance that declares no `/Resources` (path ink needs none).
        let outcome = ap.read(|ap_stream| {
            let f = match descend_form(access, &ap, &XMap::new(), ScopePolicy::OverlayParent, 0, &mut budget, 0) {
                Descend::Into(f) => f,
                Descend::Skip => return false,
                Descend::Halt => return true,
            };
            let sub_ctm = f.matrix.mul(ctm);
            let clip = crate::walker::form_bbox_clip(access, ap_stream, sub_ctm)
                .map(|bb| (bb.x0, bb.y0, bb.x1, bb.y1));
            let ares = Rc::new(
                f.scope
                    .resources
                    .clone()
                    .map(ResourceScope::own)
                    .unwrap_or_default(),
            );
            let here = PaintSeq::at(&[], content.operations.len() + k);
            walk(access, &f.ops, &f.scope.xobjects, &ares, sub_ctm, clip, None, &mut raws, 1, &mut budget, here.as_slice());
            false
        });
        if outcome == Some(true) {
            break;
        }
    }
    // The page's `/Rotate` reaches only the emitted PIXELS and the matrix that places them
    // (see `turn_pixels`): every bbox this module hands out stays in page space, because every
    // cross-subsystem comparison in `html.rs` — captions, containment, reading order — is
    // page-space, exactly as `vector::positioned_vectors_capped` reasons.
    finalize(access, raws, want_uris, crate::pdfobj::page_rotation(access, page_id))
}

/// Grow `out` by the part of `bb` its clip leaves visible. A rectangle entirely outside the
/// clip contributes nothing — it is ink the group paints and the reader never sees.
fn add_ink(bb: Rect, clip: Option<ClipRect>, out: &mut Rect) {
    if !bb.is_valid() {
        return;
    }
    let r = match clip {
        Some((cx0, cy0, cx1, cy1)) => bb.intersect(Rect::new(cx0, cy0, cx1, cy1)),
        None => bb,
    };
    if r.x1 > r.x0 && r.y1 > r.y0 {
        out.include(r.x0, r.y0);
        out.include(r.x1, r.y1);
    }
}

/// Page-space extent of the ink a **soft-mask group** paints, grown into `out`.
///
/// Returns `false` for a group this cannot bound — then the caller must apply no mask at
/// all. That is the load-bearing direction: an unbounded reading leaves today's behaviour
/// (paint whole), a wrong one deletes content. `sh` and the text-showing operators bail for
/// exactly that reason — a shading fills the clip region, which may be the whole page, and
/// a glyph's extent is `text.rs`'s business, not this walk's.
///
/// Only extents matter, so this is [`walk`]'s path/`Do` bookkeeping without the placement:
/// same `q`/`Q` stack, same deferred `W`/`W*`, same `/BBox` clip on a descended form.
#[allow(clippy::too_many_arguments)]
fn mask_extent(
    access: &dyn crate::access::DocumentAccess,
    ops: &[lopdf::content::Operation],
    xmap: &XMap,
    base: Mat,
    base_clip: Option<ClipRect>,
    depth: u32,
    budget: &mut crate::WalkBudget,
    out: &mut Rect,
) -> bool {
    let mut ctm = base;
    let mut clip = base_clip;
    let mut stack: Vec<(Mat, Option<ClipRect>)> = Vec::new();
    let mut pending_clip = false;
    let mut cur = Rect::EMPTY;
    for op in ops {
        if !budget.spend(1) {
            return false;
        }
        let o = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push((ctm, clip)),
            "Q" => {
                if let Some((m, c)) = stack.pop() {
                    ctm = m;
                    clip = c;
                }
            }
            "cm" if o.len() >= 6 => {
                let m = Mat { a: num(&o[0]), b: num(&o[1]), c: num(&o[2]), d: num(&o[3]), e: num(&o[4]), f: num(&o[5]) };
                ctm = m.mul(ctm);
            }
            "m" | "l" if o.len() >= 2 => {
                let (x, y) = ctm.apply(num(&o[0]), num(&o[1]));
                cur.include(x, y);
            }
            "c" if o.len() >= 6 => {
                for k in [0, 2, 4] {
                    let (x, y) = ctm.apply(num(&o[k]), num(&o[k + 1]));
                    cur.include(x, y);
                }
            }
            "v" | "y" if o.len() >= 4 => {
                for k in [0, 2] {
                    let (x, y) = ctm.apply(num(&o[k]), num(&o[k + 1]));
                    cur.include(x, y);
                }
            }
            "re" if o.len() >= 4 => {
                let (x, y, w, h) = (num(&o[0]), num(&o[1]), num(&o[2]), num(&o[3]));
                for (u, v) in [(x, y), (x + w, y), (x + w, y + h), (x, y + h)] {
                    let (px, py) = ctm.apply(u, v);
                    cur.include(px, py);
                }
            }
            "W" | "W*" => pending_clip = true,
            "f" | "F" | "f*" | "S" | "s" | "B" | "B*" | "b" | "b*" | "n" => {
                if pending_clip {
                    if cur.is_valid() {
                        clip = Some(crate::vector::intersect_clip(clip, (cur.x0, cur.y0, cur.x1, cur.y1)));
                    }
                    pending_clip = false;
                }
                if op.operator.as_str() != "n" {
                    add_ink(cur, clip, out); // "n" is a clip-only path — no ink
                }
                cur = Rect::EMPTY;
            }
            // A shading or a glyph run is ink this walk cannot bound; see the doc comment.
            "sh" | "Tj" | "TJ" | "'" | "\"" | "BI" => return false,
            "Do" => {
                let Some((_, stream)) = xobject_at(access, xmap, o) else {
                    continue;
                };
                let completed = stream.read(|value| {
                if has_subtype(value, b"Image") {
                    let mut bb = Rect::EMPTY;
                    for (u, v) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
                        let (px, py) = ctm.apply(u, v);
                        bb.include(px, py);
                    }
                    add_ink(bb, clip, out);
                } else {
                    match descend_form(access, &stream, xmap, ScopePolicy::OverlayParent, depth, budget, 0) {
                        Descend::Into(f) => {
                            let sub_ctm = f.matrix.mul(ctm);
                            let sub_clip = match crate::walker::form_bbox_clip(access, value, sub_ctm) {
                                Some(bb) => Some(crate::vector::intersect_clip(clip, (bb.x0, bb.y0, bb.x1, bb.y1))),
                                None => clip,
                            };
                            if !mask_extent(access, &f.ops, &f.scope.xobjects, sub_ctm, sub_clip, depth + 1, budget, out) {
                                return false;
                            }
                        }
                        Descend::Skip => return true,
                        Descend::Halt => return false,
                    }
                }
                true
                });
                if completed != Some(true) {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

/// The visible window an `/ExtGState /SMask` group leaves, in page space — `None` when this
/// mask must not crop anything.
///
/// PDF 32000-1 §11.6.5.2: the `/G` form is rendered off-screen **under the CTM in force at
/// the `gs`**, and its alpha (or luminosity) multiplies everything painted while the state
/// holds. Nothing here renders it: the group's painted extent is taken as the window, which
/// is exact for the hard-edged crop-to-window idiom this exists to fix — a producer masking
/// a 512x512 render down to a thumbnail — and an over-estimate for a soft one, i.e. wrong in
/// the same direction every other approximation in these walks is (clip less, never more).
///
/// A group whose ink cannot be bounded ([`mask_extent`]), or which paints nothing at all,
/// yields `None`. The empty case is deliberately *not* read as "alpha 0 everywhere, so
/// nothing paints": an empty mask group is far more often a producer quirk or a form this
/// walk failed to follow than an authored erasure, and "we could not read this" must cost
/// nothing rather than delete a figure.
fn mask_window(
    access: &dyn crate::access::DocumentAccess,
    form: &crate::access::StreamHandle,
    xmap: &XMap,
    ctm: Mat,
    depth: u32,
    budget: &mut crate::WalkBudget,
) -> Option<ClipRect> {
    let Descend::Into(f) = descend_form(access, form, xmap, ScopePolicy::OverlayParent, depth, budget, 0) else {
        return None;
    };
    let sub_ctm = f.matrix.mul(ctm);
    // §8.10.2 applies to a mask group like any other form: its `/BBox` bounds its ink.
    let sub_clip = form
        .read(|form| crate::walker::form_bbox_clip(access, form, sub_ctm))
        .flatten()
        .map(|bb| (bb.x0, bb.y0, bb.x1, bb.y1));
    let mut ink = Rect::EMPTY;
    if !mask_extent(access, &f.ops, &f.scope.xobjects, sub_ctm, sub_clip, depth + 1, budget, &mut ink) {
        return None;
    }
    (ink.is_valid() && ink.x1 > ink.x0 && ink.y1 > ink.y0).then_some((ink.x0, ink.y0, ink.x1, ink.y1))
}

// The interpreter state is a flat argument list here exactly as it is in `vector::walk` and
// `text::decode_spans`: bundling it into a struct would hide which parts the recursion
// REPLACES per form (scope, matrix, depth) and which it threads through (the output vector,
// the shared budget).
#[allow(clippy::too_many_arguments)]
fn walk(
    access: &dyn crate::access::DocumentAccess,
    ops: &[lopdf::content::Operation],
    xmap: &XMap,
    res: &Rc<ResourceScope>,
    base: Mat,
    // The clipping rectangle in force where this stream is invoked (page space, y up).
    base_clip: Option<ClipRect>,
    // The `/ExtGState /SMask` window in force where this stream is invoked, if any — the
    // graphics state is inherited by a form's content exactly as the clip is.
    base_smask: Option<ClipRect>,
    out: &mut Vec<RawTile>,
    depth: u32,
    budget: &mut crate::WalkBudget,
    // Address of the stream being walked (empty for the page's own content) — each
    // operation's index is appended to it to stamp a raster's `PaintSeq`.
    here: &[u32],
) {
    let mut ctm = base;
    // Active clip in PAGE space, the intersection of every `W`/`W*` seen so far on the q/Q
    // stack — the same state `vector::GState.clip` keeps, for the same reason. Without it a
    // raster the stream crops to a small window rendered at its full placement rect, and
    // since rasters paint in true paint order that means covering the ink drawn after it.
    let mut clip = base_clip;
    // The window an `/ExtGState /SMask` group leaves visible (§11.6.5.2, see [`mask_window`]).
    // Kept BESIDE the clip rather than folded into it because `gs` *replaces* the soft mask
    // — `/SMask /None` clears it — and an intersection cannot be undone. Like the clip it is
    // graphics state, so `q`/`Q` scopes it and a descended form inherits it.
    let mut smask = base_smask;
    let mut stack: Vec<(Mat, Option<ClipRect>, Option<ClipRect>)> = Vec::new();
    // `W`/`W*` mark the current path as a clip, but it takes effect only after the path's
    // painting operator (`vector::walk` defers it the same way).
    let mut pending_clip = false;
    // Page-space extent of the path under construction. Only its BOUNDS matter here — this
    // walk paints nothing — so a `Rect` accumulator says everything `vector`'s segment list
    // would, control points included (which over-estimates a curved clip in exactly the same
    // direction: it can only ever clip less than a conforming reader, never more).
    let mut cur = Rect::EMPTY;
    for (opi, op) in ops.iter().enumerate() {
        // Total-work budget (see `crate::WalkBudget`): the depth cap alone lets a
        // self-referential form branch 2x per level. Out of budget → stop and keep the
        // tiles found so far; a partial page beats a hang and beats an empty one.
        if !budget.spend(1) {
            return;
        }
        let o = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push((ctm, clip, smask)),
            "Q" => {
                if let Some((m, c, s)) = stack.pop() {
                    ctm = m;
                    clip = c;
                    smask = s;
                }
            }
            "cm" if o.len() >= 6 => {
                let m = Mat { a: num(&o[0]), b: num(&o[1]), c: num(&o[2]), d: num(&o[3]), e: num(&o[4]), f: num(&o[5]) };
                ctm = m.mul(ctm);
            }
            // §11.6.5.2's soft mask, the *other* way a producer says "show this part of that
            // image". A `gs` that names no `/SMask` leaves the one in force alone; `/None`
            // clears it; a group we can bound becomes a window (see [`mask_window`]).
            "gs" if !o.is_empty() => {
                let Some(mask) = o[0].as_name().ok().and_then(|name| {
                    res.read_named(access, b"ExtGState", name, |value| {
                        crate::access::read_resolved(access, value, |value| {
                            value
                                .as_dict()
                                .ok()
                                .and_then(|dictionary| soft_mask_of(access, dictionary))
                        })
                        .ok()
                        .flatten()
                    })
                    .flatten()
                })
                else {
                    continue;
                };
                match mask {
                    SoftMask::Cleared => smask = None,
                    SoftMask::Group(group) => {
                        smask = mask_window(access, &group, xmap, ctm, depth, budget)
                    }
                }
            }
            // The path operators, tracked for their EXTENT alone — a clip path is just a path
            // followed by `W`/`W*`, and this walk needs no other use for them.
            "m" | "l" if o.len() >= 2 => {
                let (x, y) = ctm.apply(num(&o[0]), num(&o[1]));
                cur.include(x, y);
            }
            "c" if o.len() >= 6 => {
                for k in [0, 2, 4] {
                    let (x, y) = ctm.apply(num(&o[k]), num(&o[k + 1]));
                    cur.include(x, y);
                }
            }
            "v" | "y" if o.len() >= 4 => {
                for k in [0, 2] {
                    let (x, y) = ctm.apply(num(&o[k]), num(&o[k + 1]));
                    cur.include(x, y);
                }
            }
            "re" if o.len() >= 4 => {
                let (x, y, w, h) = (num(&o[0]), num(&o[1]), num(&o[2]), num(&o[3]));
                for (u, v) in [(x, y), (x + w, y), (x + w, y + h), (x, y + h)] {
                    let (px, py) = ctm.apply(u, v);
                    cur.include(px, py);
                }
            }
            "W" | "W*" => pending_clip = true,
            "f" | "F" | "f*" | "S" | "s" | "B" | "B*" | "b" | "b*" | "n" => {
                // A pending `W`/`W*` applies from after this paint op; `q`/`Q` scopes it.
                if pending_clip {
                    if cur.is_valid() {
                        clip = Some(crate::vector::intersect_clip(clip, (cur.x0, cur.y0, cur.x1, cur.y1)));
                    }
                    pending_clip = false;
                }
                cur = Rect::EMPTY;
            }
            "Do" => {
                let Some((id, stream)) = xobject_at(access, xmap, o) else {
                    continue;
                };
                let action = stream.read(|value| {
                if has_subtype(value, b"Image") {
                    let Some(p) = image_placement(ctm, clip, smask) else {
                        return false;
                    };
                    // Record geometry + pixel dims; uri building / grid stitching happens
                    // in finalize() once the whole page's tiles are known.
                    let pw = value.dict.get(b"Width").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0) as u32;
                    out.push(RawTile { src: TileSrc::Object(id), x0: p.x0, x1: p.x1, y0: p.y0, y1: p.y1, pw, ctm: p.ctm, res: Rc::clone(res), seq: PaintSeq::at(here, opi), clip: p.crop, full: p.full });
                    false
                } else {
                    // A form is descended with the page's XObject scope still in force
                    // (`OverlayParent`): a raster the page defines and the form invokes by
                    // an unqualified name must still be found.
                    match descend_form(access, &stream, xmap, ScopePolicy::OverlayParent, depth, budget, 0) {
                        Descend::Into(f) => {
                            // The colour-space scope follows the same `OverlayParent` rule
                            // as the XObject scope: the form's own resources shadow the
                            // inherited ones name by name. A form that declares none draws
                            // in the scope it was invoked from, and shares it — no clone.
                            let child = match &f.scope.resources {
                                Some(fr) => Rc::new(res.overlay(fr)),
                                None => Rc::clone(res),
                            };
                            let sub_ctm = f.matrix.mul(ctm);
                            // §8.10.2: a form's `/BBox` clips its content — a raster the form
                            // places outside its own box does not paint. Same reader, same
                            // intersect as `vector::walk`'s `Do` arm.
                            let sub_clip = match crate::walker::form_bbox_clip(access, value, sub_ctm) {
                                Some(bb) => Some(crate::vector::intersect_clip(clip, (bb.x0, bb.y0, bb.x1, bb.y1))),
                                None => clip,
                            };
                            walk(access, &f.ops, &f.scope.xobjects, &child, sub_ctm, sub_clip, smask, out, depth + 1, budget, PaintSeq::at(here, opi).as_slice());
                            false
                        }
                        Descend::Skip => false,
                        Descend::Halt => true,
                    }
                }
                });
                if action == Some(true) {
                    return;
                }
            }
            // Inline image (§8.9.7). The fork's content parser hands the samples over as a
            // Stream operand — always unfiltered, exact length taken from the header. A
            // filtered/unparseable inline image arrives with NO operand: the placement is
            // still real ink, so it becomes an honest placeholder rather than vanishing.
            "BI" => {
                let Some(p) = image_placement(ctm, clip, smask) else {
                    continue;
                };
                let inline = match op.operands.first() {
                    Some(lopdf::Object::Stream(s)) => Some(Rc::new(crate::raster::normalize_inline_image(s))),
                    _ => None,
                };
                let pw = inline
                    .as_ref()
                    .and_then(|s| s.dict.get(b"Width").ok().and_then(|o| o.as_i64().ok()))
                    .unwrap_or(0) as u32;
                out.push(RawTile { src: TileSrc::Inline(inline), x0: p.x0, x1: p.x1, y0: p.y0, y1: p.y1, pw, ctm: p.ctm, res: Rc::clone(res), seq: PaintSeq::at(here, opi), clip: p.crop, full: p.full });
            }
            _ => {}
        }
    }
}

/// A labelled stand-in for an image whose codec this crate declines to decode, as a data URI
/// sized to the placement it replaces.
///
/// **Why this exists at all.** An undecodable image left a *blank frame* — and a blank frame
/// is indistinguishable from a figure we correctly decided not to emit. A reader cannot tell
/// "nothing here" from "we cannot read this", and every expensive defect in this effort was
/// of the second kind: silently wrong or silently absent output that looked plausible. The
/// placeholder converts an unknown into a known, and `stream_integrity` reports the same
/// stream so a program can find it without looking at pixels.
///
/// **SVG, and no new dependency.** The only in-tree raster encoders are PNG and JPEG, and
/// neither can draw text without a font rasteriser. An SVG is text, renders identically in
/// an `<img src>` on the page and in an `<image href>` inside a composited figure, and
/// scales to whatever box the placement gives it. Base64 so no consumer has to worry about
/// what needs percent-escaping in a `data:` URI.
fn placeholder_uri(filter: &str, human: &str, w: f32, h: f32) -> String {
    let (w, h) = (w.max(8.0), h.max(8.0));
    let label = format!("{human} image");
    let sub = format!("not decoded ({filter})");
    // Fit the longer line: ~0.55 em per character for a sans-serif at these sizes, with the
    // height as the other bound so a wide, short placement stays legible rather than bold.
    let fit = |s: &str| w * 0.86 / (s.chars().count() as f32 * 0.55).max(1.0);
    let fs = fit(&label).min(fit(&sub)).min(h * 0.22).clamp(3.0, 15.0);
    let svg = format!(
        concat!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"##,
            r##"<rect x="0.75" y="0.75" width="{iw}" height="{ih}" fill="#f4f4f5" stroke="#a1a1aa" stroke-width="1.5" stroke-dasharray="6 4"/>"##,
            r##"<text x="{cx}" y="{y1}" text-anchor="middle" font-family="sans-serif" font-size="{fs}" fill="#52525b">{label}</text>"##,
            r##"<text x="{cx}" y="{y2}" text-anchor="middle" font-family="sans-serif" font-size="{fs2}" fill="#71717a">{sub}</text>"##,
            "</svg>"
        ),
        w = round2(w),
        h = round2(h),
        iw = round2(w - 1.5),
        ih = round2(h - 1.5),
        cx = round2(w * 0.5),
        y1 = round2(h * 0.5 - fs * 0.15),
        y2 = round2(h * 0.5 + fs * 1.05),
        fs = round2(fs),
        fs2 = round2(fs * 0.85),
        label = crate::textutil::esc_text(&label),
        sub = crate::textutil::esc_text(&sub),
    );
    format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
}

/// Two decimals, without a trailing `.0` — the placeholder SVG is part of the render's
/// byte-for-byte determinism, so its numbers are formatted, never defaulted.
fn round2(v: f32) -> String {
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Turn a page's raw tiles into placed images: detect mergeable image GRIDS (clusters
/// of ≥4 spatially-adjacent tiles spanning ≥2 columns and ≥2 rows — maps/diagrams that
/// authoring software exports as a tile mosaic) and stitch each into ONE image; every
/// other image is emitted on its own. In placeholder mode (`!want_uris`) the same
/// grouping applies with no pixel decode — a grid becomes one empty-uri slot.
fn finalize(
    access: &dyn crate::access::DocumentAccess,
    raws: Vec<RawTile>,
    want_uris: bool,
    rot: i32,
) -> Vec<Placed> {
    let mut out = Vec::new();
    for g in cluster(&raws) {
        let tiles: Vec<&RawTile> = g.iter().map(|&i| &raws[i]).collect();
        let (x0, x1, y0, y1) = union_bbox(&tiles);
        // A merged mosaic paints where its FIRST tile did (see `Placed::seq`).
        let grid_seq = || tiles.iter().map(|t| &t.seq).min().cloned().unwrap_or_default();
        // Grid stitching reads tiles back through their document ids; an inline tile has
        // none, so a cluster containing one falls through to per-tile emission.
        if tiles.len() >= MIN_GRID_TILES && is_grid(&tiles) && tiles.iter().all(|t| t.oid().is_some()) {
            // A stitched grid is composed axis-aligned, so it carries no rotation.
            if want_uris {
                if let Some(uri) = stitch_grid(access, &tiles, (x0, x1, y0, y1), rot) {
                    let ctm = turned_placement(None, (x0, y0, x1, y1), rot);
                    out.push(Placed { y_top: y1, y_bottom: y0, x_left: x0, x_right: x1, uri, ctm, seq: grid_seq(), clip: None });
                    continue;
                }
                // stitch failed → fall through to per-tile emission
            } else {
                if tiles.iter().any(|t| t.oid().is_some_and(|id| decodable(access, &t.res, id))) {
                    out.push(Placed { y_top: y1, y_bottom: y0, x_left: x0, x_right: x1, uri: String::new(), ctm: turned_placement(None, (x0, y0, x1, y1), rot), seq: grid_seq(), clip: None });
                }
                continue;
            }
        }
        // Not a grid (or stitch failed): emit each tile individually (prior behaviour),
        // carrying its rotation matrix if any.
        for t in tiles {
            // Two ways to honour a clip, and exactly one applies. An axis-aligned placement
            // crops its SAMPLES (so `<img>`, `<image>` and any consumer of the URI agree), and
            // then carries no mask; a ROTATED one cannot — a page-space rectangle is not a
            // pixel rectangle there — so it keeps the full pixels and an SVG `clip-path`.
            let (window, mask) = if t.ctm.is_none() { (t.window(), None) } else { (None, t.clip) };
            // The placement matrix describes the TURNED unit square, because the samples the
            // URI carries are turned — the two must agree or a composite double-turns.
            let ctm = turned_placement(t.ctm, (t.x0, t.y0, t.x1, t.y1), rot);
            if want_uris {
                // A codec we decline leaves a LABELLED placeholder, not a blank frame — see
                // [`placeholder_uri`]. The gate is `data_uri` having already said no, so this
                // can never fire for an image we can decode: a decodable stream never reaches
                // it, and `raster::declined_codec` is the same list the four decline points
                // on this path read. An inline image gets the same honesty: samples that
                // fail to decode — or that the parser could not take at all — leave a
                // labelled placeholder, never a blank frame.
                let uri = match &t.src {
                    TileSrc::Object(id) => match data_uri(access, &t.res, *id, window, rot) {
                        Some(uri) => Some(uri),
                        None => access
                            .stream(*id)
                            .ok()
                            .and_then(|stream| stream.read(|stream| crate::raster::declined_codec(&stream.dict)))
                            .flatten()
                            .map(|(filter, human)| placeholder_uri(filter, human, t.x1 - t.x0, t.y1 - t.y0)),
                    },
                    TileSrc::Inline(stream) => stream
                        .as_ref()
                        .and_then(|s| inline_data_uri(access, &t.res, s, window, rot))
                        .or_else(|| Some(placeholder_uri("inline image", "inline", t.x1 - t.x0, t.y1 - t.y0))),
                };
                if let Some(uri) = uri {
                    out.push(Placed { y_top: t.y1, y_bottom: t.y0, x_left: t.x0, x_right: t.x1, uri, ctm, seq: t.seq.clone(), clip: mask });
                }
            } else {
                let renderable = match &t.src {
                    TileSrc::Object(id) => decodable(access, &t.res, *id),
                    // Inline mode always emits SOMETHING here (pixels or the labelled
                    // placeholder), so placeholder mode reports the placement too.
                    TileSrc::Inline(_) => true,
                };
                if renderable {
                    out.push(Placed { y_top: t.y1, y_bottom: t.y0, x_left: t.x0, x_right: t.x1, uri: String::new(), ctm, seq: t.seq.clone(), clip: mask });
                }
            }
        }
    }
    out
}

/// Union-find grouping of tiles whose placed bounding boxes touch/overlap (within a
/// small tolerance). Returns index groups; isolated images form singleton groups.
///
/// **Invariant: the returned order is deterministic** — groups come out in the paint
/// order of their first (lowest-index) tile, and each group's members ascend. Callers
/// downstream (`finalize` → `positioned_images` → the HTML emitter) preserve this order
/// for tiles that do not sort apart, so a nondeterministic order here reaches the
/// rendered page. See the comment at the grouping loop for why that is not free.
fn cluster(tiles: &[RawTile]) -> Vec<Vec<usize>> {
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    const TOL: f32 = 2.0;
    let n = tiles.len();
    let mut parent: Vec<usize> = (0..n).collect();
    for i in 0..n {
        for j in (i + 1)..n {
            let (a, b) = (&tiles[i], &tiles[j]);
            let xover = a.x0 < b.x1 + TOL && b.x0 < a.x1 + TOL;
            let yover = a.y0 < b.y1 + TOL && b.y0 < a.y1 + TOL;
            if xover && yover {
                let (ra, rb) = (find(&mut parent, i), find(&mut parent, j));
                if ra != rb {
                    parent[ra] = rb;
                }
            }
        }
    }
    // Collect each root's members in FIRST-APPEARANCE order. This used to be a
    // `HashMap<usize, Vec<usize>>` drained with `into_values()`, but `RandomState` seeds
    // every map instance separately, so the group order varied between two calls in the
    // SAME process — and it reaches output: `finalize` emits `Placed` images in this
    // order, and the emitter keeps it for images the reading-order sort leaves adjacent.
    // The observable effect was a page whose composited figure picked a different tile
    // cluster run to run (one real corpus document rendered 20 distinct HTML outputs in
    // 40 renders, differing by ~37 KB of embedded image). The map below is only a
    // root -> slot lookup; the ORDER lives in `out`, indexed by first appearance.
    let mut slot_of: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut out: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        let slot = *slot_of.entry(r).or_insert_with(|| {
            out.push(Vec::new());
            out.len() - 1
        });
        out[slot].push(i);
    }
    out
}

/// Enclosing box of a tile cluster, in this module's `(x0, x1, y0, y1)` order.
fn union_bbox(tiles: &[&RawTile]) -> (f32, f32, f32, f32) {
    let bb = tiles
        .iter()
        .fold(Rect::EMPTY, |acc, t| acc.union(Rect::new(t.x0, t.y0, t.x1, t.y1)));
    (bb.x0, bb.x1, bb.y0, bb.y1)
}

/// Count distinct cluster positions: sorted centers separated by more than `gap` start a
/// new column/row. Used to require a real 2-D grid (≥2 columns AND ≥2 rows).
fn distinct_positions(mut centers: Vec<f32>, gap: f32) -> usize {
    centers.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut n = 0;
    let mut last = f32::NEG_INFINITY;
    for c in centers {
        if c - last > gap.max(1.0) {
            n += 1;
            last = c;
        }
    }
    n
}

/// A cluster is a mergeable grid when its tiles lay out in ≥2 columns and ≥2 rows (a
/// single row/column stack is left as separate panels — it may be independent images).
fn is_grid(tiles: &[&RawTile]) -> bool {
    let mut ws: Vec<f32> = tiles.iter().map(|t| t.x1 - t.x0).collect();
    let mut hs: Vec<f32> = tiles.iter().map(|t| t.y1 - t.y0).collect();
    ws.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    hs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (mw, mh) = (ws[ws.len() / 2], hs[hs.len() / 2]);
    let cols = distinct_positions(tiles.iter().map(|t| (t.x0 + t.x1) * 0.5).collect(), mw * 0.5);
    let rows = distinct_positions(tiles.iter().map(|t| (t.y0 + t.y1) * 0.5).collect(), mh * 0.5);
    cols >= 2 && rows >= 2
}

/// Decode an image XObject to RGBA, compositing its soft mask (`/SMask`) into the alpha
/// channel when present. Used by the grid stitcher.
fn decode_rgba(
    access: &dyn crate::access::DocumentAccess,
    res: &ResourceScope,
    id: ObjectId,
) -> Option<image::RgbaImage> {
    let base = decode_rgb(access, res, id)?;
    let (w, h) = (base.width(), base.height());
    let mask = access
        .stream(id)
        .ok()?
        .read(|stream| decode_smask(access, res, &stream.dict))?;
    match mask {
        Some(mask) => {
            let resized;
            let m = if mask.width() == w && mask.height() == h {
                &mask
            } else {
                resized = image::imageops::resize(&mask, w, h, image::imageops::FilterType::Triangle);
                &resized
            };
            let mut out = image::RgbaImage::new(w, h);
            let base_raw: &[u8] = base.as_raw();
            let m_raw: &[u8] = m.as_raw();
            let o: &mut [u8] = &mut out;
            for i in 0..(w as usize * h as usize) {
                o[i * 4] = base_raw[i * 3];
                o[i * 4 + 1] = base_raw[i * 3 + 1];
                o[i * 4 + 2] = base_raw[i * 3 + 2];
                o[i * 4 + 3] = m_raw[i];
            }
            Some(out)
        }
        None => Some(image::DynamicImage::ImageRgb8(base).to_rgba8()),
    }
}

/// Composite a tile grid into a single data URI (JPEG when the tiles are JPEG-sourced —
/// the usual map/photo mosaic — else PNG). Each tile is decoded, resized to its placed
/// size, and pasted at its grid position (PDF y-up → image y-down). The canvas starts
/// opaque white so any uncovered seams stay opaque (and JPEG-encodable). None if nothing
/// decodes.
fn stitch_grid(
    access: &dyn crate::access::DocumentAccess,
    tiles: &[&RawTile],
    bbox: (f32, f32, f32, f32),
    turn: i32,
) -> Option<String> {
    let (x0, x1, y0, y1) = bbox;
    let (pw, ph) = (x1 - x0, y1 - y0);
    if pw <= 0.0 || ph <= 0.0 {
        return None;
    }
    // Canvas resolution = the DOMINANT tile's native DPI (pixels per point), i.e. the tile
    // with the most pixels across its placed width. A median/min would let a cluster of
    // small low-res satellites (axis strips, a colour bar) shrink the canvas and downsample
    // the main high-res image (a scatter plot) — the figure must keep the source resolution.
    let dominant = tiles
        .iter()
        .filter(|t| t.x1 - t.x0 > 0.1 && t.pw > 0)
        .max_by_key(|t| t.pw)?;
    let scale = (dominant.pw as f32 / (dominant.x1 - dominant.x0)).clamp(0.5, 20.0);
    let cw = ((pw * scale).round() as u32).clamp(1, 5000);
    let ch = ((ph * scale).round() as u32).clamp(1, 5000);
    let mut canvas = image::RgbaImage::from_pixel(cw, ch, image::Rgba([255, 255, 255, 255]));
    let mut placed_any = false;
    for t in tiles {
        let tile = match t.oid().and_then(|id| decode_rgba(access, &t.res, id)).map(|im| crop_window(im, t.window())) {
            Some(im) => im,
            None => continue,
        };
        // `decode_rgba` has already cut a clipped tile down to its window, so the recorded
        // rect is where these pixels go — no special case.
        let tw = (((t.x1 - t.x0) * scale).round() as u32).max(1);
        let th = (((t.y1 - t.y0) * scale).round() as u32).max(1);
        let resized = image::imageops::resize(&tile, tw, th, image::imageops::FilterType::Triangle);
        let ox = ((t.x0 - x0) * scale).round() as i64;
        let oy = ((y1 - t.y1) * scale).round() as i64; // top edge → canvas top
        image::imageops::overlay(&mut canvas, &resized, ox, oy);
        placed_any = true;
    }
    if !placed_any {
        return None;
    }
    rgba_uri(turn_pixels(canvas, turn), tiles.iter().all(|t| t.oid().is_some_and(|id| jpeg_source(access, id))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::test_adapter;

    #[test]
    fn the_same_page_renders_byte_identical_html_every_time() {
        // `img::cluster` returned its groups out of a `HashMap`, and `RandomState` seeds
        // every map INSTANCE separately — so two renders in the SAME process saw a page's
        // images in different orders, and the order reaches output (the emitter's
        // raster/vector absorption is first-match-wins, and images sharing a y_top get
        // identical sort boxes so nothing re-sorts them). One real 9-page document produced
        // 20 distinct HTML outputs in 40 renders, two of them differing by a whole 37 KB
        // embedded image. `tests/gen_fixtures.py::gen_image_order` is the owned repro: six
        // separate single-tile clusters on one row, one top edge, six distinct colours.
        // Pre-fix this failed on the 2nd or 3rd iteration; 25 renders over 720 possible
        // orderings cannot pass by luck.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/image_order.pdf");
        let doc = Document::load(path).expect("image_order.pdf fixture must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let first = crate::html::to_html(&crate::access::test_adapter_with_source(&doc, &raw), crate::html::Mode::Page, true, true);
        assert_eq!(first.matches("<img").count(), 6, "the fixture must place six separate rasters");
        for i in 1..25 {
            let again = crate::html::to_html(&crate::access::test_adapter_with_source(&doc, &raw), crate::html::Mode::Page, true, true);
            assert!(
                again == first,
                "render {i} differs from render 0 ({} vs {} bytes) — to_html is not deterministic",
                again.len(),
                first.len()
            );
        }
    }

    /// An adversarial fixture (`tests/gen_fixtures.py::gen_form_bomb`) and its first page.
    fn adversarial(name: &str) -> (Document, ObjectId) {
        let path = format!("{}/../tests/fixtures_pdf/adversarial/{name}", env!("CARGO_MANIFEST_DIR"));
        let doc = Document::load(&path).unwrap_or_else(|e| panic!("{name} fixture must load: {e}"));
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        (doc, page_id)
    }

    #[test]
    fn a_self_referential_form_cannot_hang_the_image_walk() {
        // `form_bomb.pdf`: form /X invokes /X twice, so the walk branches 2x per level and
        // `MAX_FORM_DEPTH` alone allowed ~2^40 descents. This call never returned.
        let (doc, page_id) = adversarial("form_bomb.pdf");
        let t = std::time::Instant::now();
        let placed = positioned_images(&test_adapter(&doc), page_id, true);
        assert!(t.elapsed().as_secs() < 10, "form bomb ran for {:?} — the budget is not bounding it", t.elapsed());
        assert!(placed.is_empty(), "the bomb draws no image, so none may be invented for it");
    }

    #[test]
    fn a_form_drawn_three_times_places_three_tiles() {
        // The control, and the reason this fix is a BUDGET and not a visited set: one form
        // holding one image, invoked at three offsets, is three placed rasters. An
        // `ObjectId` dedupe would return 1 and silently drop two real figures.
        let (doc, page_id) = adversarial("form_repeat.pdf");
        let placed = positioned_images(&test_adapter(&doc), page_id, false);
        assert_eq!(placed.len(), 3, "a repeated form must place one tile per invocation");
        let mut ys: Vec<i32> = placed.iter().map(|p| p.y_top.round() as i32).collect();
        ys.sort_unstable();
        ys.dedup();
        assert_eq!(ys.len(), 3, "the three occurrences must land at three offsets, got {ys:?}");
    }

    #[test]
    fn an_exhausted_work_budget_degrades_a_repeated_form_instead_of_emptying_it() {
        // Degrade, don't vanish: a walk that runs out mid-page keeps the tiles it found.
        let (doc, page_id) = adversarial("form_repeat.pdf");
        let content = doc.get_and_decode_page_content(page_id).expect("fixture page has content");
        let xmap = page_xobjects(&test_adapter(&doc), page_id);
        let mut raws = Vec::new();
        let mut budget = crate::WalkBudget::new(700);
        let res = Rc::new(page_resources(&test_adapter(&doc), page_id));
        walk(&test_adapter(&doc), &content.operations, &xmap, &res, Mat::ID, None, None, &mut raws, 0, &mut budget, &[]);
        assert!(!raws.is_empty(), "a tripped budget must not empty the page");
        assert!(raws.len() < 3, "the budget must really bite, got {} tiles", raws.len());
    }

    /// Decode the base64 payload of a `data:` URI into an RGB image.
    fn uri_rgb(uri: &str) -> image::RgbImage {
        let b64 = uri.split_once(";base64,").expect("data URI must be base64").1;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).expect("valid base64");
        image::load_from_memory(&bytes).expect("the embedded payload must decode").to_rgb8()
    }

    #[test]
    fn the_render_path_embeds_a_cmyk_jpeg_in_the_authored_colour() {
        // `tests/gen_fixtures.py::gen_cmyk_jpeg` — three flat CMYK bands behind an Adobe
        // APP14 marker (transform 0) and `/Decode [1 0 1 0 1 0 1 0]`. Keying the polarity
        // on `transform == 2` inverted every non-YCCK CMYK JPEG, so `to_html`'s embedded
        // image came out SOLID BLACK for all three bands. K is 0 throughout, so the
        // expected RGB is just 255 - ink.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/cmyk_jpeg.pdf");
        let doc = Document::load(path).expect("cmyk_jpeg.pdf fixture must load");
        let page_id = *doc.get_pages().values().next().expect("fixture has a page");
        let placed = positioned_images(&test_adapter(&doc), page_id, true);
        assert_eq!(placed.len(), 1, "the fixture places exactly one image");
        let img = uri_rgb(&placed[0].uri);
        assert_eq!(img.dimensions(), (96, 48));
        for (x, want) in [(15u32, [255u8, 255, 255]), (47, [0, 255, 255]), (79, [255, 75, 255])] {
            let got = img.get_pixel(x, 24).0;
            let d = (0..3).map(|i| got[i].abs_diff(want[i])).max().unwrap();
            assert!(d <= 8, "band at x={x}: expected ~{want:?}, got {got:?}");
        }
    }

    /// A flat single-colour JPEG of the given pixel format, encoded in memory.
    fn flat_jpeg(img: image::DynamicImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 100);
        img.write_with_encoder(enc).expect("encode");
        out.into_inner()
    }

    #[test]
    fn the_gray_and_rgb_jpeg_paths_are_untouched_by_the_cmyk_polarity_rule() {
        // `decode_dct_rgb` is shared: the 1- and 3-component paths must keep round-tripping
        // their colour whatever the CMYK rule does (no corpus fixture carries a non-CMYK
        // DCTDecode raster, so the JPEGs are encoded here).
        let rgb = flat_jpeg(image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([200, 30, 90]),
        )));
        let got = decode_dct_rgb(&rgb, false).expect("RGB JPEG decodes").get_pixel(4, 4).0;
        for (g, w) in got.iter().zip([200u8, 30, 90]) {
            assert!(g.abs_diff(w) <= 4, "RGB JPEG round-trip: got {got:?}");
        }

        let gray = flat_jpeg(image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
            8,
            8,
            image::Luma([160]),
        )));
        let got = decode_dct_rgb(&gray, false).expect("gray JPEG decodes").get_pixel(4, 4).0;
        assert!(got.iter().all(|v| v.abs_diff(160) <= 4), "gray JPEG round-trip: got {got:?}");
    }

    #[test]
    fn an_inverting_decode_array_flips_the_gray_and_rgb_jpeg_paths_too() {
        // The defect this pins: `decode_invert` was honoured on the CMYK path ONLY, and the
        // suite never passed `true` on the other two — so a gray/RGB JPEG carrying
        // `/Decode [1 0 …]` decoded to the NEGATIVE of the authored image with no test
        // objecting. (Verified failing on the pre-fix decoder.)
        let rgb = flat_jpeg(image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([200, 30, 90]),
        )));
        let got = decode_dct_rgb(&rgb, true).expect("RGB JPEG decodes").get_pixel(4, 4).0;
        for (g, w) in got.iter().zip([55u8, 225, 165]) {
            assert!(g.abs_diff(w) <= 4, "inverted RGB JPEG: got {got:?}, want ~[55, 225, 165]");
        }

        let gray = flat_jpeg(image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
            8,
            8,
            image::Luma([160]),
        )));
        let got = decode_dct_rgb(&gray, true).expect("gray JPEG decodes").get_pixel(4, 4).0;
        assert!(got.iter().all(|v| v.abs_diff(95) <= 4), "inverted gray JPEG: got {got:?}, want ~95");
    }

    #[test]
    fn a_raster_defined_two_ancestors_up_is_still_placed_and_lands_where_the_matrix_says() {
        // `tests/gen_fixtures.py::gen_form_inherit`. The page has NO /Resources; its parent
        // defines the form it draws, its GRANDPARENT defines the image that form paints.
        // Reading only the nearest inherited dictionary (what this walker did) left `/Im`
        // unresolved and the raster simply absent. The form's /Matrix is INDIRECT, which a
        // direct-only read degraded to the identity — placing everything 100 pt left of
        // where the page puts it. Both are asserted at once: 0 images before, 1 at x=172.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_inherit.pdf");
        let doc = Document::load(path).expect("form_inherit.pdf fixture must load");
        let page_id = *doc.get_pages().values().next().expect("fixture has a page");
        let placed = positioned_images(&test_adapter(&doc), page_id, true);
        assert_eq!(placed.len(), 1, "the grandparent's image must resolve and be placed");
        let p = &placed[0];
        assert!((p.x_left - 172.0).abs() < 0.5, "x_left {} (72 means the indirect /Matrix was lost)", p.x_left);
        assert!((p.y_bottom - 560.0).abs() < 0.5, "y_bottom {}", p.y_bottom);
        assert!((p.x_right - 292.0).abs() < 0.5, "x_right {}", p.x_right);
    }

    #[test]
    fn the_render_path_honours_an_inverting_decode_on_a_gray_and_an_rgb_jpeg() {
        // `tests/gen_fixtures.py::gen_decode_jpeg` — a gray and an RGB DCTDecode image, each
        // with an inverting `/Decode` (the RGB one written as an INDIRECT array, which
        // `img.rs`'s own `decode_inverts` could not follow). Before the fix `to_html`
        // embedded both by passing the JPEG bytes straight through, so both rendered as the
        // negative of the authored colour.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/decode_jpeg.pdf");
        let doc = Document::load(path).expect("decode_jpeg.pdf fixture must load");
        let page_id = *doc.get_pages().values().next().expect("fixture has a page");
        let placed = positioned_images(&test_adapter(&doc), page_id, true);
        assert_eq!(placed.len(), 2, "the fixture places two images");
        // Top-to-bottom: the gray image sits above the RGB one.
        let mut order: Vec<&Placed> = placed.iter().collect();
        order.sort_by(|a, b| b.y_top.partial_cmp(&a.y_top).unwrap());
        // Raw samples are gray 40 and RGB (200, 30, 90); `/Decode [1 0 …]` inverts both.
        for (p, want) in [(order[0], [215u8, 215, 215]), (order[1], [55, 225, 165])] {
            let img = uri_rgb(&p.uri);
            let got = img.get_pixel(img.width() / 2, img.height() / 2).0;
            let d = (0..3).map(|i| got[i].abs_diff(want[i])).max().unwrap();
            assert!(d <= 8, "expected ~{want:?}, got {got:?}");
        }
    }

    /// The fixture's page, and its placed images sorted top-to-bottom.
    fn placed_top_down(name: &str, page: u32) -> Vec<Placed> {
        let path = format!("{}/../tests/fixtures_pdf/{name}", env!("CARGO_MANIFEST_DIR"));
        let doc = Document::load(&path).unwrap_or_else(|e| panic!("{name} fixture must load: {e}"));
        let page_id = *doc.get_pages().get(&page).expect("fixture has that page");
        let mut placed = positioned_images(&test_adapter(&doc), page_id, true);
        placed.sort_by(|a, b| b.y_top.partial_cmp(&a.y_top).expect("finite"));
        placed
    }

    #[test]
    fn an_indexed_raster_renders_its_palette_colours_and_a_4bpc_one_renders_at_all() {
        // `tests/gen_fixtures.py::gen_render_samples`. The render path's own sample decoder
        // guessed the channel count from `samples.len() / (w*h)` whenever the colour space
        // was not a device name or ICCBased — which for a 1-byte-per-pixel Indexed image is
        // 1, so the palette INDICES were rendered as gray levels and the authored red/blue
        // came out (0,0,0)/(1,1,1). Sub-byte depths it refused outright, so the 4-bpc gray
        // image was simply absent. Verified failing before the fix: reverting `decode_rgb`'s
        // sample branch to the old body gives [0,0,0]/[1,1,1] for the first image and drops
        // the second entirely (`placed.len() == 1`).
        let placed = placed_top_down("render_samples.pdf", 1);
        assert_eq!(placed.len(), 2, "both sampled rasters must be placed");
        let idx = uri_rgb(&placed[0].uri);
        assert_eq!(idx.dimensions(), (2, 1));
        assert_eq!(idx.get_pixel(0, 0).0, [255, 0, 0], "palette entry 0 is red, not gray level 0");
        assert_eq!(idx.get_pixel(1, 0).0, [0, 0, 255], "palette entry 1 is blue, not gray level 1");
        let g4 = uri_rgb(&placed[1].uri);
        assert_eq!(g4.dimensions(), (2, 1));
        assert_eq!(g4.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(g4.get_pixel(1, 0).0, [255, 255, 255], "4-bpc nibble 15 is full white");
    }

    #[test]
    fn an_unfiltered_raster_is_placed_instead_of_vanishing() {
        // The filed bug, on a fixture that has been committed the whole time:
        // `undrawn_image.pdf` stores its rasters with NO `/Filter` at all, and
        // `decompressed_content()` ERRORS on such a stream — so `extract_images()` returned
        // two valid PNGs while `to_html` emitted zero `<img>`. Reading through
        // `pdfobj::content_bytes` (which `raster::decode_samples` does) is what fixes it.
        for (page, want_w) in [(1u32, 40u32), (2, 42)] {
            let placed = placed_top_down("undrawn_image.pdf", page);
            assert_eq!(placed.len(), 1, "page {page} paints exactly one raster");
            assert_eq!(uri_rgb(&placed[0].uri).width(), want_w);
        }
    }

    #[test]
    fn a_jpeg_cut_out_keeps_the_alpha_that_lets_the_panel_behind_it_show() {
        // `tests/gen_fixtures.py::gen_smask_panel`: a flat crimson DCTDecode raster whose
        // `/SMask` is opaque over its left half and transparent over its right, drawn over a
        // teal panel inside a vector figure. `rgba_uri` flattened any JPEG-sourced RGBA onto
        // WHITE and re-encoded as JPEG, so the composited `<image>` came out fully opaque and
        // the panel it was supposed to sit on was erased. Verified failing before the fix:
        // the emitted href is `data:image/jpeg` and every pixel of the right half is white.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/smask_panel.pdf");
        let doc = Document::load(path).expect("smask_panel.pdf fixture must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let html = crate::html::to_html(&crate::access::test_adapter_with_source(&doc, &raw), crate::html::Mode::Page, true, true);
        let href = html
            .split("<image href=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("the figure must composite the raster as an <svg> <image>");
        assert!(href.starts_with("data:image/png;base64,"), "composited raster is not lossless: {}", &href[..30.min(href.len())]);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(href.split_once(";base64,").expect("base64 payload").1)
            .expect("valid base64");
        let im = image::load_from_memory(&bytes).expect("payload decodes").to_rgba8();
        let (w, h) = im.dimensions();
        let left = im.get_pixel(w / 4, h / 2).0;
        let right = im.get_pixel(w * 3 / 4, h / 2).0;
        assert_eq!(left[3], 255, "the masked-in half must stay opaque");
        for (g, want) in left[..3].iter().zip([220u8, 30, 40]) {
            assert!(g.abs_diff(want) <= 8, "masked-in colour {left:?}");
        }
        assert_eq!(right[3], 0, "the masked-out half must stay transparent, got {right:?}");
        // And the panel is still painted, so there is something for that transparency to reveal.
        assert!(html.contains("#339999"), "the teal panel must survive alongside the raster");
    }

    #[test]
    fn a_clipped_raster_shows_its_window_and_nothing_else() {
        // `tests/gen_fixtures.py::gen_clipped_raster`: one raster placed 160x120 in three
        // identical figures — whole, clipped, and clipped under a ROTATED placement.
        // `img::walk` tracked no clip at all, so all three came back at the full placement and
        // carried every pixel: the crop the page applied was invisible to the extractor.
        let mut placed = placed_top_down("clipped_raster.pdf", 1);
        assert_eq!(placed.len(), 3, "all three rasters must be placed");
        placed.sort_by(|a, b| b.y_top.partial_cmp(&a.y_top).expect("finite"));
        let (whole, cropped, turned) = (&placed[0], &placed[1], &placed[2]);

        assert!(whole.clip.is_none() && whole.ctm.is_none(), "the control is unclipped and axis-aligned");
        assert!((whole.x_right - whole.x_left - 160.0).abs() < 0.5, "control width {}", whole.x_right - whole.x_left);
        assert_eq!(uri_rgb(&whole.uri).dimensions(), (2, 2), "the control keeps every sample");

        // Axis-aligned: the recorded extent is the WINDOW (what reading order, figure
        // absorption and the viewBox must measure)...
        assert!((cropped.x_right - cropped.x_left - 80.0).abs() < 0.5, "cropped width {}", cropped.x_right - cropped.x_left);
        assert!((cropped.y_top - cropped.y_bottom - 60.0).abs() < 0.5, "cropped height {}", cropped.y_top - cropped.y_bottom);
        // ...and so are the SAMPLES, which is the only answer that also works for the plain
        // `<img>` this raster becomes when no figure absorbs it. Bottom-left quarter = blue.
        let win = uri_rgb(&cropped.uri);
        assert_eq!(win.dimensions(), (1, 1), "the window is a quarter of a 2x2 raster");
        assert_eq!(win.get_pixel(0, 0).0, [40, 60, 210], "the surviving quarter is the blue one");
        assert!(cropped.clip.is_none(), "cropped samples need no mask on top of them");

        // Rotated: a page-space rectangle is not a pixel rectangle, so the pixels are kept
        // whole and the crop travels as an SVG mask instead.
        assert!(turned.ctm.is_some(), "the rotated placement keeps its matrix");
        let c = turned.clip.expect("a rotated clipped raster carries the mask");
        for (got, want) in [(c.0, 140.0), (c.1, 110.0), (c.2, 220.0), (c.3, 170.0)] {
            assert!((got - want).abs() < 0.5, "clip {c:?}");
        }
        assert_eq!(uri_rgb(&turned.uri).dimensions(), (2, 2), "a masked raster keeps every sample");
    }

    #[test]
    fn a_soft_masked_raster_shows_only_the_window_its_mask_group_paints() {
        // `tests/gen_fixtures.py::gen_smask_group_raster`: the same 160x120 raster in three
        // identical figures — no `gs`, an `/S /Alpha` mask group painting one opaque 80x60
        // rect, and an `/S /Luminosity` group with a WHITE `/BC`. The `gs` arm read `/ca`
        // and `/CA` only, so all three came back at the full placement: §11.6.5.2's soft
        // mask, the other way a PDF says "show this part of that image", was invisible here.
        let mut placed = placed_top_down("smask_group_raster.pdf", 1);
        assert_eq!(placed.len(), 3, "all three rasters must be placed");
        placed.sort_by(|a, b| b.y_top.partial_cmp(&a.y_top).expect("finite"));
        let (whole, masked, backdrop) = (&placed[0], &placed[1], &placed[2]);

        assert!(whole.clip.is_none(), "the control carries no mask");
        assert!((whole.x_right - whole.x_left - 160.0).abs() < 0.5, "control width {}", whole.x_right - whole.x_left);

        // The alpha group's window is its painted rect, and — the placement being
        // axis-aligned — it lands in the SAMPLES exactly as a path clip's does.
        assert!((masked.x_right - masked.x_left - 80.0).abs() < 0.5, "masked width {}", masked.x_right - masked.x_left);
        assert!((masked.y_top - masked.y_bottom - 60.0).abs() < 0.5, "masked height {}", masked.y_top - masked.y_bottom);
        let win = uri_rgb(&masked.uri);
        assert_eq!(win.dimensions(), (1, 1), "the window is a quarter of a 2x2 raster");
        assert_eq!(win.get_pixel(0, 0).0, [40, 60, 210], "the surviving quarter is the blue one");

        // A `/Luminosity` mask over a non-black backdrop is opaque everywhere its group
        // paints nothing, so the group's extent bounds nothing — this raster must come back
        // WHOLE. The approximation may only ever clip less than a conforming reader.
        assert!(
            (backdrop.x_right - backdrop.x_left - 160.0).abs() < 0.5,
            "a mask we cannot bound must not crop: width {}",
            backdrop.x_right - backdrop.x_left
        );
        assert_eq!(uri_rgb(&backdrop.uri).dimensions(), (2, 2), "and it keeps every sample");
    }

    #[test]
    fn a_codec_we_decline_leaves_a_labelled_placeholder_and_a_decodable_one_never_does() {
        // `tests/gen_fixtures.py::gen_undecodable_codec`: a `/JPXDecode` image beside an
        // ordinary Flate raster of the same size. JPEG 2000 *decoding* stays parked (no
        // mature pure-Rust decoder; OpenJPEG's CVE record is not worth carrying in the
        // automatic path for untrusted files) — but the refusal used to be INVISIBLE, and a
        // blank frame is indistinguishable from a figure we correctly chose not to emit.
        //
        // The control is the regression that matters: the placeholder must never fire for an
        // image we can decode.
        let placed = placed_top_down("undecodable_codec.pdf", 1);
        assert_eq!(placed.len(), 2, "both placements are emitted, got {}", placed.len());
        let mut by_x = placed;
        by_x.sort_by(|a, b| a.x_left.partial_cmp(&b.x_left).expect("finite"));
        let (declined, control) = (&by_x[0], &by_x[1]);

        assert!(control.uri.starts_with("data:image/png;base64,"), "the control decodes normally: {}", &control.uri[..30.min(control.uri.len())]);
        assert!(!control.uri.contains("svg"), "and never gets a placeholder");

        assert!(declined.uri.starts_with("data:image/svg+xml;base64,"), "placeholder: {}", &declined.uri[..40.min(declined.uri.len())]);
        let svg = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(declined.uri.split_once(";base64,").expect("base64 payload").1)
                .expect("valid base64"),
        )
        .expect("the placeholder is text");
        assert!(svg.contains("JPEG 2000 image"), "the codec must be NAMED: {svg}");
        assert!(svg.contains("not decoded (JPXDecode)"), "and the filter with it: {svg}");
        // Sized to the placement it stands in for, so it neither stretches nor shrinks.
        assert!(svg.contains(r#"width="200" height="150""#), "placeholder geometry: {svg}");

        // And a program can find it without looking at pixels — P12's report is the home for
        // "this stream is not what it looks like".
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/undecodable_codec.pdf");
        let doc = Document::load(path).expect("fixture must load");
        let issues = crate::pdfobj::stream_issues(&crate::access::test_adapter(&doc));
        let hit = issues.iter().find(|i| i.object.0 == 5).expect("the declined stream is reported");
        assert_eq!((hit.kind, hit.filter.as_str(), hit.recovered), ("codec-unsupported", "JPXDecode", 0));
        assert!(issues.iter().all(|i| i.object.0 != 6), "the decodable raster is NOT reported: {issues:?}");
    }

    #[test]
    fn a_masked_composite_image_wraps_the_clip_on_a_group_not_on_the_image() {
        // The emission half. `transform` establishes a new user space and a `clip-path` on the
        // SAME element resolves in it — so a mask on the transformed `<image>` would be scaled
        // by the placement matrix and hide the raster outright. It goes on a wrapping `<g>`.
        // (Verified by rendering: on the `<image>` the figure came out blank.)
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/clipped_raster.pdf");
        let doc = Document::load(path).expect("clipped_raster.pdf fixture must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let html = crate::html::to_html(&crate::access::test_adapter_with_source(&doc, &raw), crate::html::Mode::Page, true, true);
        assert_eq!(html.matches("<image ").count(), 3, "all three figures composite their raster");
        // Only the rotated one needs a mask; the axis-aligned crop is in the samples.
        assert_eq!(html.matches("clip-path=").count(), 1, "exactly one mask");
        let g = html.split("<g clip-path=").nth(1).expect("the mask must sit on a wrapping <g>");
        assert!(g.starts_with("\"url(#clip_"), "clip-path value: {}", &g[..40.min(g.len())]);
        let el = g.split("<image ").nth(1).map(|s| s.split("/>").next().unwrap_or("")).expect("the <g> wraps the <image>");
        assert!(el.contains("transform=\"matrix("), "a masked raster is placed by matrix: {el}");
        // The mask is the 80x60 window: the figure box is 300x180 at (72, 80), so the crop's
        // local origin is x 68, y 90 (y measured down from the figure's top).
        let id = g.split("url(#").nth(1).and_then(|s| s.split(')').next()).expect("a clip-path url");
        assert!(html.contains(&format!("<clipPath id=\"{id}\"><rect x=\"68\" y=\"90\" width=\"80\" height=\"60\"/></clipPath>")), "clipPath {id} missing or wrong");
    }

    #[test]
    fn a_standalone_raster_on_a_rotate_page_is_emitted_the_way_a_reader_sees_it() {
        // `tests/gen_fixtures.py::gen_rotated_raster`: four pages, ONE content stream, only
        // `/Rotate` differs, each drawing a 2x1 red|blue image with no vector ink anywhere —
        // so it goes out as a plain `<img>`, which applies no transform at all. The four pages
        // used to emit a BYTE-IDENTICAL data URI: the figure path had learned the page turn
        // and the raster path had not.
        //
        // A quarter turn also transposes the pixel grid, so the dimensions prove it as well as
        // the colours; red-first vs blue-first separates 90 from 270.
        const RED: [u8; 3] = [255, 0, 0];
        const BLUE: [u8; 3] = [0, 0, 255];
        let mut uris = Vec::new();
        for (page, dims, first, second) in [
            (1u32, (2u32, 1u32), RED, BLUE),   // upright: red LEFT
            (2, (1, 2), RED, BLUE),            // 90 cw: red on TOP
            (3, (2, 1), BLUE, RED),            // 180: blue left
            (4, (1, 2), BLUE, RED),            // 270: blue on top
        ] {
            let placed = placed_top_down("rotated_raster.pdf", page);
            assert_eq!(placed.len(), 1, "page {page} paints exactly one raster");
            let p = &placed[0];
            // The placement rect stays in PAGE space on every page — `html.rs` compares
            // captions, containment and reading order in that space and must not move.
            for (got, want, what) in [(p.x_left, 100.0), (p.y_bottom, 250.0), (p.x_right, 300.0), (p.y_top, 350.0)]
                .iter()
                .zip(["x_left", "y_bottom", "x_right", "y_top"])
                .map(|((g, w), n)| (*g, *w, n))
            {
                assert!((got - want).abs() < 0.5, "page {page} {what} {got} (want {want})");
            }
            let img = uri_rgb(&p.uri);
            assert_eq!(img.dimensions(), dims, "page {page} pixel grid");
            assert_eq!(img.get_pixel(0, 0).0, first, "page {page} first sample");
            assert_eq!(img.get_pixel(dims.0 - 1, dims.1 - 1).0, second, "page {page} last sample");
            uris.push(p.uri.clone());
        }
        assert_eq!(uris[0], uris[0], "sanity");
        for i in 1..4 {
            assert_ne!(uris[i], uris[0], "page {} must not re-emit the upright pixels", i + 1);
        }
    }

    #[test]
    fn a_raster_a_form_places_outside_its_own_bbox_is_not_placed() {
        // `tests/gen_fixtures.py::gen_form_bbox`: one form body, two `/BBox`es. The raster sits
        // at form (220,140)-(260,180), outside `[0 0 200 120]` and inside `[0 0 400 300]`, and
        // both forms are invoked through `/Matrix [1.5 0 0 1.5 0 0]`. §8.10.2 makes `/BBox` a
        // clip on the form's content, and nothing in the crate read the key — `grep -c b"BBox"`
        // over the walkers was 0 — so the same raster was placed twice.
        let placed = placed_top_down("form_bbox.pdf", 1);
        assert_eq!(placed.len(), 1, "only the form whose BBox contains the raster may place it");
        let p = &placed[0];
        for (got, want, what) in [(p.x_left, 402.0, "x_left"), (p.y_bottom, 270.0, "y_bottom"), (p.x_right, 462.0, "x_right"), (p.y_top, 330.0, "y_top")] {
            assert!((got - want).abs() < 0.5, "{what} {got} (want {want}) — the /Matrix must reach the BBox too");
        }
    }

    #[test]
    fn an_opaque_mask_leaves_a_jpeg_a_jpeg() {
        // The size half of the same rule: `alpha_is_opaque` is what keeps the 792 embedded
        // JPEGs of the corpus from all becoming PNG. A mask that masks nothing must not cost
        // a re-encode — `cmyk_jpeg.pdf` carries no `/SMask` at all, so it takes the
        // passthrough/re-encode branch and must stay `image/jpeg`.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/cmyk_jpeg.pdf");
        let doc = Document::load(path).expect("cmyk_jpeg.pdf fixture must load");
        let page_id = *doc.get_pages().values().next().expect("fixture has a page");
        let placed = positioned_images(&test_adapter(&doc), page_id, true);
        assert!(placed[0].uri.starts_with("data:image/jpeg;"), "an unmasked JPEG must not become a PNG");
        // Unit-level: the predicate itself, at the 250 noise floor it is written to.
        let opaque = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        assert!(alpha_is_opaque(&opaque));
        let noisy = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 251]));
        assert!(alpha_is_opaque(&noisy), "resample/DCT noise in the mask is not transparency");
        let mut holed = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        holed.put_pixel(0, 0, image::Rgba([1, 2, 3, 0]));
        assert!(!alpha_is_opaque(&holed), "one genuinely transparent pixel is transparency");
    }

    #[test]
    fn the_placeholder_gate_agrees_with_the_decoder_image_for_image() {
        // `decodable` used to be an independently-written `bpc == 8` guess, so placeholder
        // mode disagreed with inline mode in BOTH directions on the same page:
        // `colorspace_images.pdf` emitted 3 `<image N>` placeholders for 2 embedded images.
        // Rederiving it from `raster::samples_decodable` makes the two agree by construction.
        for name in ["colorspace_images.pdf", "render_samples.pdf", "undrawn_image.pdf"] {
            let path = format!("{}/../tests/fixtures_pdf/{name}", env!("CARGO_MANIFEST_DIR"));
            let doc = Document::load(&path).unwrap_or_else(|e| panic!("{name} must load: {e}"));
            for page_id in doc.get_pages().values() {
                let inline = positioned_images(&test_adapter(&doc), *page_id, true).len();
                let placeholders = positioned_images(&test_adapter(&doc), *page_id, false).len();
                assert_eq!(placeholders, inline, "{name}: placeholder count must match the embedded count");
            }
        }
    }
}
