//! Content-stream text extraction with `/ToUnicode` CMap support.
//!
//! lopdf's built-in `extract_text` does not map CID/Type0 fonts, so synthetic
//! and many embedded-font PDFs come back empty. Here we walk the content stream
//! ourselves, decode show-text operators through each font's ToUnicode CMap, and
//! recover real Unicode — including 2-byte CID codes and diacritics.

use lopdf::{Document, Object, ObjectId};
use std::collections::HashMap;

struct FontInfo {
    two_byte: bool,
    to_unicode: Option<HashMap<u32, String>>,
    /// Glyph advance widths in 1000-em units, keyed by code/CID.
    widths: HashMap<u32, f32>,
    default_width: f32,
}

fn obj_i64(o: &Object) -> Option<i64> {
    match o {
        Object::Integer(i) => Some(*i),
        Object::Real(r) => Some(*r as i64),
        _ => None,
    }
}

/// Find the first occurrence of `needle` in `hay` starting at `from`.
fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Lenient recovery of a stream's bytes straight from the raw PDF, for malformed
/// streams that omit `/Length` (e.g. oxidize-pdf's ToUnicode CMaps), which lopdf
/// reads as empty. Mirrors what PyMuPDF does: scan `N 0 obj ... stream..endstream`.
fn recover_stream(raw: &[u8], obj_num: u32) -> Option<Vec<u8>> {
    let marker = format!("{obj_num} 0 obj");
    let obj_pos = find_from(raw, marker.as_bytes(), 0)?;
    let s = find_from(raw, b"stream", obj_pos)? + b"stream".len();
    let mut start = s;
    if raw.get(start) == Some(&b'\r') {
        start += 1;
    }
    if raw.get(start) == Some(&b'\n') {
        start += 1;
    }
    let end = find_from(raw, b"endstream", start)?;
    let mut e = end;
    if e > start && raw[e - 1] == b'\n' {
        e -= 1;
    }
    if e > start && raw[e - 1] == b'\r' {
        e -= 1;
    }
    if e > start {
        Some(raw[start..e].to_vec())
    } else {
        None
    }
}

/// Build per-page font table: resource name -> FontInfo.
fn build_fonts(doc: &Document, page_id: ObjectId, raw: &[u8]) -> HashMap<Vec<u8>, FontInfo> {
    let mut out = HashMap::new();
    let fonts = match doc.get_page_fonts(page_id) {
        Ok(f) => f,
        Err(_) => return out,
    };
    for (name, dict) in fonts {
        let subtype = dict.get(b"Subtype").and_then(|o| o.as_name()).unwrap_or(b"");
        let two_byte = subtype == b"Type0";
        let to_unicode = dict
            .get(b"ToUnicode")
            .ok()
            .and_then(|o| o.as_reference().ok())
            .and_then(|r| {
                // Prefer lopdf's loaded content; fall back to raw recovery when the
                // stream is missing /Length (lopdf yields empty bytes).
                let from_lopdf = doc
                    .get_object(r)
                    .ok()
                    .and_then(|o| o.as_stream().ok())
                    .map(|s| s.decompressed_content().unwrap_or_else(|_| s.content.clone()))
                    .filter(|b| !b.is_empty());
                let bytes = from_lopdf.or_else(|| recover_stream(raw, r.0))?;
                Some(parse_tounicode(&bytes))
            });

        // Glyph widths.
        let mut widths = HashMap::new();
        let mut default_width = if two_byte { 1000.0 } else { 500.0 };
        if two_byte {
            // Type0: widths live on the descendant CIDFont (/DW default, /W array).
            if let Some(desc) = dict
                .get(b"DescendantFonts")
                .ok()
                .and_then(|o| deref(doc, o))
                .and_then(|o| o.as_array().ok())
                .and_then(|a| a.first())
                .and_then(|o| deref(doc, o))
                .and_then(|o| o.as_dict().ok())
            {
                if let Some(dw) = desc.get(b"DW").ok().and_then(obj_i64) {
                    default_width = dw as f32;
                }
                if let Some(w) = desc.get(b"W").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_array().ok()) {
                    parse_cid_widths(w, &mut widths);
                }
            }
        } else if let (Some(first), Some(ws)) = (
            dict.get(b"FirstChar").ok().and_then(obj_i64),
            dict.get(b"Widths").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_array().ok()),
        ) {
            for (i, w) in ws.iter().enumerate() {
                if let Some(wd) = obj_i64(w) {
                    widths.insert(first as u32 + i as u32, wd as f32);
                }
            }
        }

        out.insert(name, FontInfo { two_byte, to_unicode, widths, default_width });
    }
    out
}

/// Dereference an object that may be an indirect reference.
fn deref<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Object> {
    match o {
        Object::Reference(r) => doc.get_object(*r).ok(),
        other => Some(other),
    }
}

/// Parse a Type0 /W array: `[ c [w...] ]` and `[ c1 c2 w ]` forms.
fn parse_cid_widths(w: &[Object], widths: &mut HashMap<u32, f32>) {
    let mut i = 0;
    while i < w.len() {
        let c1 = match obj_i64(&w[i]) {
            Some(v) => v as u32,
            None => {
                i += 1;
                continue;
            }
        };
        match w.get(i + 1) {
            Some(Object::Array(list)) => {
                for (k, wd) in list.iter().enumerate() {
                    if let Some(v) = obj_i64(wd) {
                        widths.insert(c1 + k as u32, v as f32);
                    }
                }
                i += 2;
            }
            Some(o2) => {
                let c2 = obj_i64(o2).unwrap_or(c1 as i64) as u32;
                let wd = w.get(i + 2).and_then(obj_i64).unwrap_or(1000) as f32;
                for c in c1..=c2 {
                    widths.insert(c, wd);
                }
                i += 3;
            }
            None => break,
        }
    }
}

#[derive(Debug)]
enum Tok {
    Hex(Vec<u8>),
    Word(String),
    ArrStart,
    ArrEnd,
}

fn tokenize_cmap(text: &str) -> Vec<Tok> {
    let b = text.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'<' => {
                let mut j = i + 1;
                let mut hex = String::new();
                while j < b.len() && b[j] != b'>' {
                    if (b[j] as char).is_ascii_hexdigit() {
                        hex.push(b[j] as char);
                    }
                    j += 1;
                }
                if hex.len() % 2 == 1 {
                    hex.push('0');
                }
                let bytes = (0..hex.len())
                    .step_by(2)
                    .filter_map(|k| u8::from_str_radix(&hex[k..k + 2], 16).ok())
                    .collect();
                toks.push(Tok::Hex(bytes));
                i = j + 1;
            }
            b'[' => {
                toks.push(Tok::ArrStart);
                i += 1;
            }
            b']' => {
                toks.push(Tok::ArrEnd);
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let mut j = i;
                let mut w = String::new();
                while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'<' && b[j] != b'[' && b[j] != b']' {
                    w.push(b[j] as char);
                    j += 1;
                }
                toks.push(Tok::Word(w));
                i = j;
            }
        }
    }
    toks
}

fn be_u32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32)
}

/// Decode UTF-16BE bytes into a String (handles surrogate pairs / ligatures).
fn utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks(2).map(|c| be_u32(c) as u16).collect();
    char::decode_utf16(units)
        .map(|r| r.unwrap_or('\u{FFFD}'))
        .collect()
}

fn parse_tounicode(data: &[u8]) -> HashMap<u32, String> {
    let text = String::from_utf8_lossy(data);
    let toks = tokenize_cmap(&text);
    let mut map = HashMap::new();
    let is_word = |t: &Tok, w: &str| matches!(t, Tok::Word(s) if s == w);
    let mut i = 0;
    while i < toks.len() {
        if is_word(&toks[i], "beginbfchar") {
            i += 1;
            while i < toks.len() && !is_word(&toks[i], "endbfchar") {
                if let (Tok::Hex(src), Some(Tok::Hex(dst))) = (&toks[i], toks.get(i + 1)) {
                    map.insert(be_u32(src), utf16be(dst));
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if is_word(&toks[i], "beginbfrange") {
            i += 1;
            while i < toks.len() && !is_word(&toks[i], "endbfrange") {
                match (toks.get(i), toks.get(i + 1), toks.get(i + 2)) {
                    (Some(Tok::Hex(lo)), Some(Tok::Hex(hi)), Some(Tok::Hex(dst))) => {
                        let (lo, hi) = (be_u32(lo), be_u32(hi));
                        let base = utf16be(dst);
                        // Common case: single-char dst incremented across the range.
                        let base_cp = base.chars().next().map(|c| c as u32);
                        for (k, code) in (lo..=hi).enumerate() {
                            if let Some(cp) = base_cp {
                                if let Some(ch) = char::from_u32(cp + k as u32) {
                                    map.insert(code, ch.to_string());
                                }
                            }
                        }
                        i += 3;
                    }
                    (Some(Tok::Hex(lo)), Some(Tok::Hex(_hi)), Some(Tok::ArrStart)) => {
                        let mut code = be_u32(lo);
                        i += 3; // past lo, hi, '['
                        while i < toks.len() && !matches!(toks[i], Tok::ArrEnd) {
                            if let Tok::Hex(dst) = &toks[i] {
                                map.insert(code, utf16be(dst));
                                code += 1;
                            }
                            i += 1;
                        }
                        i += 1; // past ']'
                    }
                    _ => i += 1,
                }
            }
        } else {
            i += 1;
        }
    }
    map
}

/// Normalise a decoded char: map zero-width spaces to a real space, drop other
/// zero-width / BOM noise that pollutes word boundaries.
fn push_norm(out: &mut String, ch: char) {
    match ch {
        '\u{200B}' | '\u{00A0}' | '\u{2009}' | '\u{202F}' => out.push(' '),
        '\u{FEFF}' | '\u{200C}' | '\u{200D}' | '\0' => {}
        c => out.push(c),
    }
}

/// Decode a show-string into `out` and return the horizontal advance in points
/// (font widths + char-spacing `tc` + word-spacing `tw`, applied at `size`).
fn decode_string(
    bytes: &[u8],
    font: Option<&FontInfo>,
    size: f32,
    tc: f32,
    tw: f32,
    out: &mut String,
) -> f32 {
    let mut adv = 0.0f32;
    match font {
        Some(fi) => {
            let step = if fi.two_byte { 2 } else { 1 };
            let mut i = 0;
            while i < bytes.len() {
                let end = (i + step).min(bytes.len());
                let code = be_u32(&bytes[i..end]);
                match &fi.to_unicode {
                    Some(map) => {
                        if let Some(s) = map.get(&code) {
                            for ch in s.chars() {
                                push_norm(out, ch);
                            }
                        }
                    }
                    None if !fi.two_byte => push_norm(out, bytes[i] as char),
                    None => {
                        if let Some(ch) = char::from_u32(code) {
                            push_norm(out, ch);
                        }
                    }
                }
                let w = fi.widths.get(&code).copied().unwrap_or(fi.default_width);
                adv += w / 1000.0 * size + tc;
                // PDF word spacing applies to single-byte code 32 only.
                if !fi.two_byte && code == 32 {
                    adv += tw;
                }
                i += step;
            }
        }
        None => {
            for &b in bytes {
                push_norm(out, b as char);
                adv += 0.5 * size + tc + if b == 32 { tw } else { 0.0 };
            }
        }
    }
    adv
}

/// 2x3 affine matrix (PDF row-vector convention): [a b c d e f].
#[derive(Clone, Copy)]
struct Mat {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}
impl Mat {
    const ID: Mat = Mat { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };
    fn mul(self, r: Mat) -> Mat {
        Mat {
            a: self.a * r.a + self.b * r.c,
            b: self.a * r.b + self.b * r.d,
            c: self.c * r.a + self.d * r.c,
            d: self.c * r.b + self.d * r.d,
            e: self.e * r.a + self.f * r.c + r.e,
            f: self.e * r.b + self.f * r.d + r.f,
        }
    }
    fn translate(tx: f32, ty: f32) -> Mat {
        Mat { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: tx, f: ty }
    }
}

/// A positioned run of text (origin in PDF user space, y increases upward).
pub struct Span {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub width: f32,
    pub text: String,
}

fn num(o: &Object) -> f32 {
    match o {
        Object::Integer(i) => *i as f32,
        Object::Real(r) => *r,
        _ => 0.0,
    }
}

/// Extract positioned text spans for one page via content-stream interpretation.
pub fn extract_spans(doc: &Document, page_id: ObjectId, raw: &[u8]) -> Vec<Span> {
    let content = match doc.get_and_decode_page_content(page_id) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let fonts = build_fonts(doc, page_id, raw);
    let mut spans = Vec::new();
    let mut tm = Mat::ID;
    let mut tlm = Mat::ID;
    let mut leading = 0.0f32;
    let mut size = 0.0f32;
    let mut tc = 0.0f32; // char spacing
    let mut tw = 0.0f32; // word spacing
    let mut cur: Option<&FontInfo> = None;

    let mut emit = |tm: &Mat, size: f32, width: f32, s: String| {
        if !s.is_empty() {
            spans.push(Span {
                x: tm.e,
                y: tm.f,
                size: size.abs().max(1.0),
                width: width.abs(),
                text: s,
            });
        }
    };

    for op in &content.operations {
        let o = &op.operands;
        match op.operator.as_str() {
            "BT" => {
                tm = Mat::ID;
                tlm = Mat::ID;
            }
            "Tf" => {
                if let Some(Object::Name(n)) = o.first() {
                    cur = fonts.get(n);
                }
                if let Some(s) = o.get(1) {
                    size = num(s);
                }
            }
            "Td" if o.len() >= 2 => {
                tlm = Mat::translate(num(&o[0]), num(&o[1])).mul(tlm);
                tm = tlm;
            }
            "TD" if o.len() >= 2 => {
                leading = -num(&o[1]);
                tlm = Mat::translate(num(&o[0]), num(&o[1])).mul(tlm);
                tm = tlm;
            }
            "Tm" if o.len() >= 6 => {
                tlm = Mat {
                    a: num(&o[0]),
                    b: num(&o[1]),
                    c: num(&o[2]),
                    d: num(&o[3]),
                    e: num(&o[4]),
                    f: num(&o[5]),
                };
                tm = tlm;
            }
            "TL" if !o.is_empty() => leading = num(&o[0]),
            "Tc" if !o.is_empty() => tc = num(&o[0]),
            "Tw" if !o.is_empty() => tw = num(&o[0]),
            "T*" => {
                tlm = Mat::translate(0.0, -leading).mul(tlm);
                tm = tlm;
            }
            "Tj" => {
                if let Some(Object::String(s, _)) = o.first() {
                    let mut t = String::new();
                    let w = decode_string(s, cur, size, tc, tw, &mut t);
                    emit(&tm, size * tm.d, w, t);
                }
            }
            "'" | "\"" => {
                tlm = Mat::translate(0.0, -leading).mul(tlm);
                tm = tlm;
                if let Some(Object::String(s, _)) = o.last() {
                    let mut t = String::new();
                    let w = decode_string(s, cur, size, tc, tw, &mut t);
                    emit(&tm, size * tm.d, w, t);
                }
            }
            "TJ" => {
                if let Some(Object::Array(arr)) = o.first() {
                    let mut t = String::new();
                    let mut w = 0.0f32;
                    for el in arr {
                        match el {
                            Object::String(s, _) => w += decode_string(s, cur, size, tc, tw, &mut t),
                            // numeric kern: subtract advance (units/1000 * size).
                            Object::Integer(n) => {
                                w -= *n as f32 / 1000.0 * size;
                                if *n < -150 {
                                    t.push(' ');
                                }
                            }
                            Object::Real(r) => {
                                w -= *r / 1000.0 * size;
                                if *r < -150.0 {
                                    t.push(' ');
                                }
                            }
                            _ => {}
                        }
                    }
                    emit(&tm, size * tm.d, w, t);
                }
            }
            _ => {}
        }
    }
    spans
}

/// Effective span width (fall back to a char estimate if widths were absent).
fn span_width(s: &Span) -> f32 {
    if s.width > 0.1 {
        s.width
    } else {
        s.text.chars().count() as f32 * s.size * 0.5
    }
}

/// Reconstruct reading-order text from positioned spans, joining horizontally
/// adjacent spans without a space (handles per-glyph-positioned text) and
/// inserting a space only on a word-sized gap.
fn text_from_spans(mut spans: Vec<Span>) -> String {
    spans.retain(|s| !s.text.is_empty());
    if spans.is_empty() {
        return String::new();
    }
    let band = (spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32 * 0.6).max(2.0);
    spans.sort_by(|p, q| {
        let bp = (p.y / band).round();
        let bq = (q.y / band).round();
        bq.partial_cmp(&bp)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(p.x.partial_cmp(&q.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut out = String::new();
    let mut last_band: Option<f32> = None;
    let mut prev_end = 0.0f32;
    for s in &spans {
        let b = (s.y / band).round();
        let new_line = match last_band {
            Some(lb) => (lb - b).abs() >= 0.5,
            None => false,
        };
        if new_line {
            out.push('\n');
        } else if last_band.is_some() {
            // same line: insert a space only when there's a real gap, and never
            // duplicate one that the span text already carries.
            let gap = s.x - prev_end;
            if gap > s.size * 0.28 && !out.ends_with(' ') && !s.text.starts_with(' ') {
                out.push(' ');
            }
        }
        out.push_str(&s.text);
        prev_end = s.x + span_width(s);
        last_band = Some(b);
    }
    out
}

/// Diagnostic: report font table + content status for one page.
pub fn debug_page(doc: &Document, page_id: ObjectId, raw: &[u8]) -> String {
    let fonts = build_fonts(doc, page_id, raw);
    let mut s = format!("fonts={}\n", fonts.len());
    for (k, fi) in &fonts {
        s += &format!(
            "  '{}': two_byte={} tounicode_len={}\n",
            String::from_utf8_lossy(k),
            fi.two_byte,
            fi.to_unicode.as_ref().map(|m| m.len() as i64).unwrap_or(-1)
        );
    }
    // ToUnicode raw diagnostics per font.
    if let Ok(fonts) = doc.get_page_fonts(page_id) {
        for (name, dict) in fonts {
            if let Some(r) = dict.get(b"ToUnicode").ok().and_then(|o| o.as_reference().ok()) {
                if let Ok(st) = doc.get_object(r).and_then(|o| o.as_stream().map(|s| s.clone())) {
                    let dec = st.decompressed_content();
                    let raw_len = st.content.len();
                    let dec_len = dec.as_ref().map(|d| d.len() as i64).unwrap_or(-1);
                    let used = dec.unwrap_or_else(|_| st.content.clone());
                    let parsed = parse_tounicode(&used).len();
                    let sample: String = String::from_utf8_lossy(&used).chars().take(50).collect();
                    s += &format!(
                        "  TU '{}': ref={:?} raw={} dec={} parsed={} dict={:?} sample={:?}\n",
                        String::from_utf8_lossy(&name),
                        r,
                        raw_len,
                        dec_len,
                        parsed,
                        st.dict,
                        sample
                    );
                }
            }
        }
    }
    match doc.get_and_decode_page_content(page_id) {
        Ok(c) => {
            s += &format!("ops={}\n", c.operations.len());
            let tfs: Vec<String> = c
                .operations
                .iter()
                .filter(|o| o.operator == "Tf")
                .filter_map(|o| match o.operands.first() {
                    Some(Object::Name(n)) => Some(String::from_utf8_lossy(n).into_owned()),
                    _ => None,
                })
                .take(5)
                .collect();
            s += &format!("Tf_names={:?}\n", tfs);
        }
        Err(e) => s += &format!("content ERR: {e}\n"),
    }
    s
}

/// Extract text for one page via positioned spans + reading-order reconstruction.
/// Returns None if the page content cannot be decoded.
pub fn extract_page(doc: &Document, page_id: ObjectId, raw: &[u8]) -> Option<String> {
    let spans = extract_spans(doc, page_id, raw);
    Some(text_from_spans(spans))
}
