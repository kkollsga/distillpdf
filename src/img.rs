//! Positioned image extraction for HTML: track the content-stream `Do` operator
//! and CTM to locate each image, then emit a base64 data URI (JPEG passthrough;
//! PNG assembly for Flate-encoded raster samples).

use base64::Engine;
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
}

fn num(o: &Object) -> f32 {
    match o {
        Object::Integer(i) => *i as f32,
        Object::Real(r) => *r,
        _ => 0.0,
    }
}

fn deref<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Object> {
    match o {
        Object::Reference(r) => doc.get_object(*r).ok(),
        other => Some(other),
    }
}

/// All XObject entries (images AND forms) in a resources dict: name -> object id.
fn xobjects_of(doc: &Document, resources: &Dictionary) -> HashMap<Vec<u8>, ObjectId> {
    let mut map = HashMap::new();
    let xdict = resources
        .get(b"XObject")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok());
    if let Some(xd) = xdict {
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

fn cs_channels(dict: &Dictionary, doc: &Document) -> Option<usize> {
    let cs = dict.get(b"ColorSpace").ok().and_then(|o| deref(doc, o))?;
    match cs {
        Object::Name(n) => match n.as_slice() {
            b"DeviceRGB" | b"RGB" | b"CalRGB" => Some(3),
            b"DeviceGray" | b"G" | b"CalGray" => Some(1),
            b"DeviceCMYK" | b"CMYK" => Some(4),
            _ => None,
        },
        Object::Array(a) => {
            // ICCBased -> /N components
            if a.first().and_then(|o| o.as_name().ok()) == Some(&b"ICCBased"[..]) {
                a.get(1)
                    .and_then(|o| deref(doc, o))
                    .and_then(|o| o.as_stream().ok())
                    .and_then(|s| s.dict.get(b"N").ok())
                    .and_then(|o| o.as_i64().ok())
                    .map(|n| n as usize)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn filters_of(dict: &Dictionary) -> Vec<Vec<u8>> {
    match dict.get(b"Filter").ok() {
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Array(a)) => a.iter().filter_map(|o| o.as_name().ok().map(|n| n.to_vec())).collect(),
        _ => vec![],
    }
}

/// The Adobe APP14 `transform` byte of a JPEG, if the marker is present.
/// `2` = YCCK (Photoshop/Adobe CMYK, stored with inverted polarity).
fn adobe_transform(buf: &[u8]) -> Option<u8> {
    let mut i = 2; // skip SOI (FF D8)
    while i + 4 <= buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = buf[i + 1];
        // standalone markers (no length) / start of scan
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if marker == 0xDA {
            break; // SOS — compressed data follows
        }
        let len = ((buf[i + 2] as usize) << 8) | buf[i + 3] as usize;
        if marker == 0xEE {
            // APP14: "Adobe" + version(2) + flags0(2) + flags1(2) + transform(1)
            let seg = &buf[i + 4..(i + 2 + len).min(buf.len())];
            if seg.starts_with(b"Adobe") && seg.len() >= 12 {
                return Some(seg[11]);
            }
        }
        i += 2 + len;
    }
    None
}

/// Component count from the JPEG SOF (start-of-frame) marker, without a full decode.
/// 4 => CMYK; 3 => RGB/YCbCr; 1 => grayscale.
fn jpeg_components(buf: &[u8]) -> Option<u8> {
    let mut i = 2;
    while i + 4 <= buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = buf[i + 1];
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = ((buf[i + 2] as usize) << 8) | buf[i + 3] as usize;
        // SOF0/1/2/3, 5/6/7, 9/10/11, 13/14/15 carry the frame header; skip DHT/DAC tables.
        let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof && i + 9 < buf.len() {
            // marker(2) len(2) precision(1) height(2) width(2) Nf(1)
            return Some(buf[i + 9]);
        }
        if marker == 0xDA {
            break;
        }
        i += 2 + len;
    }
    None
}

/// True if a DCTDecode stream is a 4-component (CMYK) JPEG — these cannot be passed
/// through to the browser as `image/jpeg` (Adobe CMYK renders inverted/black).
fn jpeg_is_cmyk(content: &[u8]) -> bool {
    jpeg_components(content) == Some(4)
}

/// Decode a JPEG to RGB8 via `jpeg-decoder` (the `image`/zune path mis-renders Adobe
/// CMYK as black). Handles grayscale, RGB, and CMYK; for CMYK it mirrors MuPDF: invert
/// the Adobe YCCK polarity (transform==2), optionally XOR a PDF `/Decode` inversion,
/// then map true CMYK → RGB.
fn decode_jpeg_rgb(content: &[u8], decode_invert: bool) -> Option<image::RgbImage> {
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(content));
    // Read the header first and reject absurd dimensions BEFORE decoding pixels (a hostile
    // JPEG could declare a huge frame and force a giant allocation).
    dec.read_info().ok()?;
    if let Some(info) = dec.info() {
        if !dims_sane(info.width as u32, info.height as u32) {
            return None;
        }
    }
    let px = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    use jpeg_decoder::PixelFormat::*;
    match info.pixel_format {
        L8 => {
            let g = image::GrayImage::from_raw(w, h, px)?;
            Some(image::DynamicImage::ImageLuma8(g).to_rgb8())
        }
        L16 => None,
        RGB24 => image::RgbImage::from_raw(w, h, px),
        CMYK32 => {
            let net_invert = (adobe_transform(content) == Some(2)) ^ decode_invert;
            let n = (w as usize) * (h as usize);
            let mut rgb = vec![0u8; n * 3];
            for i in 0..n {
                let (mut c, mut m, mut y, mut k) =
                    (px[i * 4], px[i * 4 + 1], px[i * 4 + 2], px[i * 4 + 3]);
                if net_invert {
                    c = 255 - c;
                    m = 255 - m;
                    y = 255 - y;
                    k = 255 - k;
                }
                let kk = 255 - k as u16;
                rgb[i * 3] = ((255 - c as u16) * kk / 255) as u8;
                rgb[i * 3 + 1] = ((255 - m as u16) * kk / 255) as u8;
                rgb[i * 3 + 2] = ((255 - y as u16) * kk / 255) as u8;
            }
            image::RgbImage::from_raw(w, h, rgb)
        }
    }
}

/// True if the image dict carries an inverting `/Decode` array (first channel `[1 0 …]`).
fn decode_inverts(dict: &Dictionary) -> bool {
    match dict.get(b"Decode").ok() {
        Some(Object::Array(a)) if a.len() >= 2 => num(&a[0]) > num(&a[1]),
        _ => false,
    }
}

/// Decode an image stream to RGB8, handling JPEG (DCTDecode) and Flate/raw
/// samples (gray/rgb). Returns None for formats we don't assemble.
/// Per-dimension sanity cap and a total-pixel ceiling. A malformed/hostile stream can declare
/// enormous `/Width`×`/Height`; refusing them before allocating the raw buffer prevents a
/// decompression-bomb OOM.
const MAX_IMAGE_DIM: u32 = 0x1FFFF; // 131071 px per side
const MAX_IMAGE_PIXELS: usize = 64 << 20; // 64 M px

fn dims_sane(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w <= MAX_IMAGE_DIM && h <= MAX_IMAGE_DIM && (w as usize).saturating_mul(h as usize) <= MAX_IMAGE_PIXELS
}

fn decode_rgb(doc: &Document, id: ObjectId) -> Option<image::RgbImage> {
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let dict = &stream.dict;
    let filters = filters_of(dict);
    if filters.iter().any(|f| f == b"DCTDecode") {
        return decode_jpeg_rgb(&stream.content, decode_inverts(dict));
    }
    if filters.iter().any(|f| f == b"JPXDecode" || f == b"CCITTFaxDecode" || f == b"JBIG2Decode") {
        return None;
    }
    let w = dict.get(b"Width").ok().and_then(|o| o.as_i64().ok())? as u32;
    let h = dict.get(b"Height").ok().and_then(|o| o.as_i64().ok())? as u32;
    let bpc = dict.get(b"BitsPerComponent").ok().and_then(|o| o.as_i64().ok()).unwrap_or(8);
    if bpc != 8 || !dims_sane(w, h) {
        return None;
    }
    let raw = stream.decompressed_content().ok()?;
    let total = (w as usize) * (h as usize);
    let ch = cs_channels(dict, doc).unwrap_or_else(|| if total > 0 && raw.len() % total == 0 { raw.len() / total } else { 0 });
    match ch {
        1 if raw.len() >= total => {
            let g = image::GrayImage::from_raw(w, h, raw[..total].to_vec())?;
            Some(image::DynamicImage::ImageLuma8(g).to_rgb8())
        }
        3 if raw.len() >= total * 3 => image::RgbImage::from_raw(w, h, raw[..total * 3].to_vec()),
        _ => None,
    }
}

/// Decode the soft mask (`/SMask`) of an image to a grayscale alpha channel.
fn decode_smask(doc: &Document, dict: &Dictionary) -> Option<image::GrayImage> {
    let sid = dict.get(b"SMask").ok().and_then(|o| o.as_reference().ok())?;
    let stream = doc.get_object(sid).ok()?.as_stream().ok()?;
    let sd = &stream.dict;
    let filters = filters_of(sd);
    if filters.iter().any(|f| f == b"DCTDecode") {
        return image::load_from_memory_with_format(&stream.content, image::ImageFormat::Jpeg)
            .ok()
            .map(|d| d.to_luma8());
    }
    if filters.iter().any(|f| f == b"JPXDecode" || f == b"CCITTFaxDecode" || f == b"JBIG2Decode") {
        return None;
    }
    let w = sd.get(b"Width").ok().and_then(|o| o.as_i64().ok())? as u32;
    let h = sd.get(b"Height").ok().and_then(|o| o.as_i64().ok())? as u32;
    let bpc = sd.get(b"BitsPerComponent").ok().and_then(|o| o.as_i64().ok()).unwrap_or(8);
    if bpc != 8 || !dims_sane(w, h) {
        return None;
    }
    let raw = stream.decompressed_content().ok()?;
    let total = (w as usize) * (h as usize);
    if raw.len() < total {
        return None;
    }
    image::GrayImage::from_raw(w, h, raw[..total].to_vec())
}

/// Cheap (no-decode) test that an image XObject is a format we can render — used in
/// placeholder mode so a `<image N>` stands in only for an image that inline mode would
/// actually emit. Mirrors `data_uri`'s format gate without decoding pixels (so it can
/// slightly over-count the rare SMask content-free overlay that inline mode drops).
fn decodable(doc: &Document, id: ObjectId) -> bool {
    let stream = match doc.get_object(id).ok().and_then(|o| o.as_stream().ok()) {
        Some(s) => s,
        None => return false,
    };
    let dict = &stream.dict;
    let filters = filters_of(dict);
    if filters.iter().any(|f| f == b"JPXDecode" || f == b"CCITTFaxDecode" || f == b"JBIG2Decode") {
        return false;
    }
    if filters.iter().any(|f| f == b"DCTDecode") {
        return true; // JPEG: always renderable
    }
    let w = dict.get(b"Width").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0);
    let h = dict.get(b"Height").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0);
    let bpc = dict.get(b"BitsPerComponent").ok().and_then(|o| o.as_i64().ok()).unwrap_or(8);
    bpc == 8 && w > 0 && h > 0 && dims_sane(w as u32, h as u32)
}

/// Build a base64 data URI for an image stream, or None if unsupported.
///
/// Images with a soft mask (`/SMask`) are alpha-composited so transparency is
/// preserved — without this, masked figures (whose visible content lives in the
/// mask, over a flat-colour/black base) render as solid black boxes.
fn data_uri(doc: &Document, id: ObjectId) -> Option<String> {
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let dict = stream.dict.clone();
    let b64 = base64::engine::general_purpose::STANDARD;
    let has_smask = dict.get(b"SMask").and_then(|o| o.as_reference()).is_ok();
    let filters = filters_of(&dict);

    // No soft mask: keep JPEG passthrough (cheap, lossless), assemble PNG for
    // Flate raster, bail on formats we can't decode.
    if !has_smask {
        if filters.iter().any(|f| f == b"DCTDecode") {
            // Gray/RGB JPEGs pass straight through (cheap, lossless). CMYK JPEGs can't:
            // browsers render Adobe CMYK inverted/black, so decode → RGB and re-encode.
            if jpeg_is_cmyk(&stream.content) {
                let rgb = decode_jpeg_rgb(&stream.content, decode_inverts(&dict))?;
                return jpeg_uri(rgb);
            }
            return Some(format!("data:image/jpeg;base64,{}", b64.encode(&stream.content)));
        }
        if filters.iter().any(|f| f == b"JPXDecode" || f == b"CCITTFaxDecode" || f == b"JBIG2Decode") {
            return None;
        }
        let rgb = decode_rgb(doc, id)?;
        let png = png_bytes(image::DynamicImage::ImageRgb8(rgb))?;
        return Some(format!("data:image/png;base64,{}", b64.encode(&png)));
    }

    // Soft mask present: decode base + mask, composite to RGBA, emit PNG.
    let base = decode_rgb(doc, id)?;
    let (w, h) = (base.width(), base.height());
    let mask = decode_smask(doc, &dict);
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
    rgba_uri(rgba, filters.iter().any(|f| f == b"DCTDecode"))
}

fn png_bytes(img: image::DynamicImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// True if the image XObject is DCTDecode (a JPEG) at source.
fn jpeg_source(doc: &Document, id: ObjectId) -> bool {
    doc.get_object(id)
        .ok()
        .and_then(|o| o.as_stream().ok())
        .map(|s| filters_of(&s.dict).iter().any(|f| f == b"DCTDecode"))
        .unwrap_or(false)
}

/// Base64 JPEG data URI (quality 85) for an RGB image.
fn jpeg_uri(rgb: image::RgbImage) -> Option<String> {
    let mut out = std::io::Cursor::new(Vec::new());
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    image::DynamicImage::ImageRgb8(rgb).write_with_encoder(enc).ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(out.into_inner())))
}

/// Encode a (possibly composited) RGBA raster to a data URI, matching the SOURCE format.
/// JPEG-sourced content is flattened onto WHITE (the HTML render background, so any
/// edge/feather transparency from a soft mask looks identical) and re-encoded as JPEG —
/// far smaller than lossless PNG for photographic content, and adds no new quality loss
/// over the already-lossy source. Flate/line-art (lossless source) keeps its alpha in a
/// lossless PNG.
fn rgba_uri(img: image::RgbaImage, jpeg_src: bool) -> Option<String> {
    if jpeg_src {
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
}

/// One placed image XObject before clustering: its object id, placed bbox (page points),
/// and source pixel WIDTH (for the stitch resolution). Collected by `walk`, then grouped
/// by `finalize`.
struct RawTile {
    id: ObjectId,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    pw: u32,
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
pub fn positioned_images(doc: &Document, page_id: ObjectId, want_uris: bool) -> Vec<Placed> {
    let resources = match page_resources(doc, page_id) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let content = match doc.get_and_decode_page_content(page_id) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let xmap = xobjects_of(doc, &resources);
    let mut raws: Vec<RawTile> = Vec::new();
    walk(doc, &content.operations, &xmap, M::ID, &mut raws, 0);
    finalize(doc, raws, want_uris)
}

fn walk(
    doc: &Document,
    ops: &[lopdf::content::Operation],
    xmap: &HashMap<Vec<u8>, ObjectId>,
    base: M,
    out: &mut Vec<RawTile>,
    depth: u32,
) {
    if depth > crate::MAX_FORM_DEPTH {
        return;
    }
    let mut ctm = base;
    let mut stack: Vec<M> = Vec::new();
    for op in ops {
        let o = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => {
                if let Some(m) = stack.pop() {
                    ctm = m;
                }
            }
            "cm" if o.len() >= 6 => {
                let m = M { a: num(&o[0]), b: num(&o[1]), c: num(&o[2]), d: num(&o[3]), e: num(&o[4]), f: num(&o[5]) };
                ctm = m.mul(ctm);
            }
            "Do" => {
                let name = match o.first() {
                    Some(Object::Name(n)) => n,
                    _ => continue,
                };
                let id = match xmap.get(name) {
                    Some(&id) => id,
                    None => continue,
                };
                let stream = match doc.get_object(id).and_then(|x| x.as_stream().map(|s| s.clone())) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let subtype = stream.dict.get(b"Subtype").and_then(|x| x.as_name()).unwrap_or(b"");
                if subtype == b"Image" {
                    // Placed bbox = image unit square [0,1]^2 through the CTM.
                    let corners = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
                    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
                    for (u, v) in corners {
                        let px = u * ctm.a + v * ctm.c + ctm.e;
                        let py = u * ctm.b + v * ctm.d + ctm.f;
                        x0 = x0.min(px); x1 = x1.max(px);
                        y0 = y0.min(py); y1 = y1.max(py);
                    }
                    let (w, h) = (x1 - x0, y1 - y0);
                    if w < MIN_DIM || h < MIN_DIM {
                        continue; // diagram tile / rule / icon — not a figure
                    }
                    // Record geometry + pixel dims; uri building / grid stitching happens
                    // in finalize() once the whole page's tiles are known.
                    let pw = stream.dict.get(b"Width").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0) as u32;
                    out.push(RawTile { id, x0, x1, y0, y1, pw });
                } else if subtype == b"Form" {
                    // Form: descend with its /Matrix and own resources.
                    let fm = stream
                        .dict
                        .get(b"Matrix")
                        .ok()
                        .and_then(|x| x.as_array().ok())
                        .filter(|a| a.len() >= 6)
                        .map(|a| M { a: num(&a[0]), b: num(&a[1]), c: num(&a[2]), d: num(&a[3]), e: num(&a[4]), f: num(&a[5]) })
                        .unwrap_or(M::ID);
                    let form_ctm = fm.mul(ctm);
                    let form_res = stream
                        .dict
                        .get(b"Resources")
                        .ok()
                        .and_then(|x| deref(doc, x))
                        .and_then(|x| x.as_dict().ok())
                        .cloned();
                    let mut child = xmap.clone(); // inherit, then overlay form's own
                    if let Some(fr) = &form_res {
                        for (k, v) in xobjects_of(doc, fr) {
                            child.insert(k, v);
                        }
                    }
                    if let Ok(content) = lopdf::content::Content::decode(&stream.decompressed_content().unwrap_or_default()) {
                        walk(doc, &content.operations, &child, form_ctm, out, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Turn a page's raw tiles into placed images: detect mergeable image GRIDS (clusters
/// of ≥4 spatially-adjacent tiles spanning ≥2 columns and ≥2 rows — maps/diagrams that
/// authoring software exports as a tile mosaic) and stitch each into ONE image; every
/// other image is emitted on its own. In placeholder mode (`!want_uris`) the same
/// grouping applies with no pixel decode — a grid becomes one empty-uri slot.
fn finalize(doc: &Document, raws: Vec<RawTile>, want_uris: bool) -> Vec<Placed> {
    let mut out = Vec::new();
    for g in cluster(&raws) {
        let tiles: Vec<&RawTile> = g.iter().map(|&i| &raws[i]).collect();
        let (x0, x1, y0, y1) = union_bbox(&tiles);
        if tiles.len() >= MIN_GRID_TILES && is_grid(&tiles) {
            if want_uris {
                if let Some(uri) = stitch_grid(doc, &tiles, (x0, x1, y0, y1)) {
                    out.push(Placed { y_top: y1, y_bottom: y0, x_left: x0, x_right: x1, uri });
                    continue;
                }
                // stitch failed → fall through to per-tile emission
            } else {
                if tiles.iter().any(|t| decodable(doc, t.id)) {
                    out.push(Placed { y_top: y1, y_bottom: y0, x_left: x0, x_right: x1, uri: String::new() });
                }
                continue;
            }
        }
        // Not a grid (or stitch failed): emit each tile individually (prior behaviour).
        for t in tiles {
            if want_uris {
                if let Some(uri) = data_uri(doc, t.id) {
                    out.push(Placed { y_top: t.y1, y_bottom: t.y0, x_left: t.x0, x_right: t.x1, uri });
                }
            } else if decodable(doc, t.id) {
                out.push(Placed { y_top: t.y1, y_bottom: t.y0, x_left: t.x0, x_right: t.x1, uri: String::new() });
            }
        }
    }
    out
}

/// Union-find grouping of tiles whose placed bounding boxes touch/overlap (within a
/// small tolerance). Returns index groups; isolated images form singleton groups.
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
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    groups.into_values().collect()
}

fn union_bbox(tiles: &[&RawTile]) -> (f32, f32, f32, f32) {
    let (mut x0, mut x1, mut y0, mut y1) = (f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY);
    for t in tiles {
        x0 = x0.min(t.x0);
        x1 = x1.max(t.x1);
        y0 = y0.min(t.y0);
        y1 = y1.max(t.y1);
    }
    (x0, x1, y0, y1)
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
fn decode_rgba(doc: &Document, id: ObjectId) -> Option<image::RgbaImage> {
    let base = decode_rgb(doc, id)?;
    let (w, h) = (base.width(), base.height());
    let dict = doc.get_object(id).ok()?.as_stream().ok()?.dict.clone();
    match decode_smask(doc, &dict) {
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
fn stitch_grid(doc: &Document, tiles: &[&RawTile], bbox: (f32, f32, f32, f32)) -> Option<String> {
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
        let tile = match decode_rgba(doc, t.id) {
            Some(im) => im,
            None => continue,
        };
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
    rgba_uri(canvas, tiles.iter().all(|t| jpeg_source(doc, t.id)))
}
