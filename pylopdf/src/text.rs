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
        out.insert(name, FontInfo { two_byte, to_unicode });
    }
    out
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

fn decode_string(bytes: &[u8], font: Option<&FontInfo>, out: &mut String) {
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
                            out.push_str(s);
                        }
                        // unmapped code with a ToUnicode present: skip (likely notdef)
                    }
                    None if !fi.two_byte => {
                        // simple font, no ToUnicode: best-effort latin1
                        out.push(bytes[i] as char);
                    }
                    None => {
                        // 2-byte font, no ToUnicode: last-resort Identity decode
                        // (Identity-H CIDs often equal Unicode code points).
                        if let Some(ch) = char::from_u32(code) {
                            if ch != '\0' {
                                out.push(ch);
                            }
                        }
                    }
                }
                i += step;
            }
        }
        None => {
            for &b in bytes {
                out.push(b as char);
            }
        }
    }
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

/// Extract text for one page via content-stream interpretation + ToUnicode.
/// Returns None if the page content cannot be decoded.
pub fn extract_page(doc: &Document, page_id: ObjectId, raw: &[u8]) -> Option<String> {
    let content = doc.get_and_decode_page_content(page_id).ok()?;
    let fonts = build_fonts(doc, page_id, raw);
    let mut out = String::new();
    let mut cur: Option<&FontInfo> = None;
    for op in &content.operations {
        match op.operator.as_str() {
            "Tf" => {
                if let Some(Object::Name(n)) = op.operands.first() {
                    cur = fonts.get(n);
                }
            }
            "Tj" => {
                if let Some(Object::String(s, _)) = op.operands.first() {
                    decode_string(s, cur, &mut out);
                }
            }
            "'" | "\"" => {
                out.push('\n');
                if let Some(Object::String(s, _)) = op.operands.last() {
                    decode_string(s, cur, &mut out);
                }
            }
            "TJ" => {
                if let Some(Object::Array(arr)) = op.operands.first() {
                    for el in arr {
                        match el {
                            Object::String(s, _) => decode_string(s, cur, &mut out),
                            Object::Integer(n) if *n < -100 => out.push(' '),
                            Object::Real(r) if *r < -100.0 => out.push(' '),
                            _ => {}
                        }
                    }
                }
            }
            "Td" | "TD" | "T*" | "ET" => out.push('\n'),
            _ => {}
        }
    }
    Some(out)
}
