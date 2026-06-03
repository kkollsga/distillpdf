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

/// Decode an image stream to RGB8, handling JPEG (DCTDecode) and Flate/raw
/// samples (gray/rgb). Returns None for formats we don't assemble.
fn decode_rgb(doc: &Document, id: ObjectId) -> Option<image::RgbImage> {
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let dict = &stream.dict;
    let filters = filters_of(dict);
    if filters.iter().any(|f| f == b"DCTDecode") {
        return image::load_from_memory_with_format(&stream.content, image::ImageFormat::Jpeg)
            .ok()
            .map(|d| d.to_rgb8());
    }
    if filters.iter().any(|f| f == b"JPXDecode" || f == b"CCITTFaxDecode" || f == b"JBIG2Decode") {
        return None;
    }
    let w = dict.get(b"Width").ok().and_then(|o| o.as_i64().ok())? as u32;
    let h = dict.get(b"Height").ok().and_then(|o| o.as_i64().ok())? as u32;
    let bpc = dict.get(b"BitsPerComponent").ok().and_then(|o| o.as_i64().ok()).unwrap_or(8);
    if bpc != 8 || w == 0 || h == 0 {
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
    if bpc != 8 || w == 0 || h == 0 {
        return None;
    }
    let raw = stream.decompressed_content().ok()?;
    let total = (w as usize) * (h as usize);
    if raw.len() < total {
        return None;
    }
    image::GrayImage::from_raw(w, h, raw[..total].to_vec())
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
    let mut max_a = 0u8;
    let (mut cmin, mut cmax) = ([255u8; 3], [0u8; 3]);
    for (x, y, px) in rgba.enumerate_pixels_mut() {
        let c = base.get_pixel(x, y).0;
        let a = mask_ref.map(|m| m.get_pixel(x, y).0[0]).unwrap_or(255);
        max_a = max_a.max(a);
        if a > 0 {
            for k in 0..3 {
                cmin[k] = cmin[k].min(c[k]);
                cmax[k] = cmax[k].max(c[k]);
            }
        }
        *px = image::Rgba([c[0], c[1], c[2], a]);
    }
    // Drop content-free overlays rather than emit them:
    //   * fully faint (max alpha < 40) — an invisible anti-alias / shadow layer;
    //   * a flat single-colour panel that never reaches strong opacity — a tint /
    //     drop-shadow fill (real line-art/photos hit full opacity somewhere).
    let spread = (0..3).map(|k| cmax[k].saturating_sub(cmin[k])).max().unwrap_or(0);
    if max_a < 40 || (spread < 8 && max_a < 160) {
        return None;
    }
    let png = png_bytes(image::DynamicImage::ImageRgba8(rgba))?;
    Some(format!("data:image/png;base64,{}", b64.encode(&png)))
}

fn png_bytes(img: image::DynamicImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// A raster image placed on the page: its top edge (for reading-order placement)
/// and data URI. (Width/height are used to filter tiny tiles at detection time but
/// not retained — the rendered `<img>` is sized by CSS, not intrinsic dimensions.)
pub struct Placed {
    pub y_top: f32,
    pub uri: String,
}

/// Images smaller than this (rendered, points) are diagram tiles / rules /
/// icons rather than figures — dropped so they don't flood the output with
/// hundreds of fragments (a single figure is often a mosaic of tiny tiles).
const MIN_DIM: f32 = 24.0;

/// Positioned images on a page, top-to-bottom. Recurses into Form XObjects
/// (which carry their own content + resources + /Matrix). Tiny tiles are
/// filtered out (see [`MIN_DIM`]).
pub fn positioned_images(doc: &Document, page_id: ObjectId) -> Vec<Placed> {
    let resources = match page_resources(doc, page_id) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let content = match doc.get_and_decode_page_content(page_id) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let xmap = xobjects_of(doc, &resources);
    let mut out = Vec::new();
    let mut cache: HashMap<ObjectId, Option<String>> = HashMap::new();
    walk(doc, &content.operations, &xmap, M::ID, &mut out, &mut cache, 0);
    out
}

#[allow(clippy::too_many_arguments)]
fn walk(
    doc: &Document,
    ops: &[lopdf::content::Operation],
    xmap: &HashMap<Vec<u8>, ObjectId>,
    base: M,
    out: &mut Vec<Placed>,
    cache: &mut HashMap<ObjectId, Option<String>>,
    depth: u32,
) {
    if depth > 8 {
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
                    let uri = cache.entry(id).or_insert_with(|| data_uri(doc, id)).clone();
                    if let Some(u) = uri {
                        out.push(Placed { y_top: y1, uri: u });
                    }
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
                        walk(doc, &content.operations, &child, form_ctm, out, cache, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}
