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

/// Build a base64 data URI for an image stream, or None if unsupported.
fn data_uri(doc: &Document, id: ObjectId) -> Option<String> {
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let dict = &stream.dict;
    let filters: Vec<Vec<u8>> = match dict.get(b"Filter").ok() {
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Array(a)) => a.iter().filter_map(|o| o.as_name().ok().map(|n| n.to_vec())).collect(),
        _ => vec![],
    };
    let b64 = base64::engine::general_purpose::STANDARD;

    // JPEG passthrough
    if filters.iter().any(|f| f == b"DCTDecode") {
        return Some(format!("data:image/jpeg;base64,{}", b64.encode(&stream.content)));
    }
    // JPEG2000 / fax / jbig2: not assembled here
    if filters.iter().any(|f| f == b"JPXDecode" || f == b"CCITTFaxDecode" || f == b"JBIG2Decode") {
        return None;
    }

    // Flate / raw samples -> assemble PNG
    let w = dict.get(b"Width").ok().and_then(|o| o.as_i64().ok())? as u32;
    let h = dict.get(b"Height").ok().and_then(|o| o.as_i64().ok())? as u32;
    let bpc = dict.get(b"BitsPerComponent").ok().and_then(|o| o.as_i64().ok()).unwrap_or(8);
    if bpc != 8 || w == 0 || h == 0 {
        return None;
    }
    let raw = stream.decompressed_content().ok()?;
    let total = (w as usize) * (h as usize);
    let ch = cs_channels(dict, doc).unwrap_or_else(|| if total > 0 && raw.len() % total == 0 { raw.len() / total } else { 0 });
    let dynimg = match ch {
        1 if raw.len() >= total => {
            image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(w, h, raw[..total].to_vec())?)
        }
        3 if raw.len() >= total * 3 => {
            image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, raw[..total * 3].to_vec())?)
        }
        _ => return None,
    };
    let png = png_bytes(dynimg)?;
    Some(format!("data:image/png;base64,{}", b64.encode(&png)))
}

fn png_bytes(img: image::DynamicImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Positioned images on a page: (y_top in PDF space, data URI). Recurses into
/// Form XObjects (which carry their own content + resources + /Matrix).
pub fn positioned_images(doc: &Document, page_id: ObjectId) -> Vec<(f32, String)> {
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
    out: &mut Vec<(f32, String)>,
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
                    let uri = cache.entry(id).or_insert_with(|| data_uri(doc, id)).clone();
                    if let Some(u) = uri {
                        out.push((ctm.f + ctm.d.max(0.0), u));
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
