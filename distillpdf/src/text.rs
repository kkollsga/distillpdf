//! Content-stream text extraction with `/ToUnicode` CMap support.
//!
//! lopdf's built-in `extract_text` does not map CID/Type0 fonts, so synthetic
//! and many embedded-font PDFs come back empty. Here we walk the content stream
//! ourselves, decode show-text operators through each font's ToUnicode CMap, and
//! recover real Unicode — including 2-byte CID codes and diacritics.

use crate::geom::Mat;
use crate::pdfobj::{deref, num};
use crate::walker::{descend_form, xobject_at, Descend, ScopePolicy, XMap};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::HashMap;

struct FontInfo {
    two_byte: bool,
    to_unicode: Option<HashMap<u32, String>>,
    /// The ToUnicode CMap of this (Type0) font is an *identity* table — it maps each code to
    /// the character with that same scalar value. That proves the font's CID space IS Unicode
    /// (a subset font's CIDs are glyph indices, so its ToUnicode is never identity), which in
    /// turn means a code the table omits is a **gap in the producer's table**, not "this font
    /// has no glyph here": the code itself still carries the text. See [`is_identity_cmap`].
    identity_unicode: bool,
    /// `/Encoding /Differences` map: code -> decoded text (glyph names already
    /// resolved to Unicode). Fills the gap for simple fonts that have no
    /// ToUnicode — e.g. f-ligatures encoded as code 2/3 (`/fi`,`/fl`) that would
    /// otherwise surface as control chars.
    differences: Option<HashMap<u32, String>>,
    /// The font's **declared** base encoding (`/Encoding /WinAnsiEncoding`, or an
    /// `/Encoding` dict's `/BaseEncoding`) as a code → Unicode table. Used as a
    /// *per-code* fallback when the ToUnicode CMap omits the code — a partial
    /// ToUnicode is common (Distiller writes one `bfchar` per subsetted glyph) and
    /// without this every omitted code decodes to nothing, dropping letters out of
    /// mid-word ("Redding" → "edding").
    ///
    /// `None` whenever the font does **not** declare one. That is deliberate and is
    /// the whole safety property: a symbolic font with no `/Encoding` uses its font
    /// program's built-in cmap, whose codes a subsetter re-packs arbitrarily, so a
    /// Latin table would render `θ` as `a` and `π` as `^`. Dropping an unmappable
    /// code is recoverable; silently substituting a plausible wrong letter is not.
    base_encoding: Option<&'static [u16; 256]>,
    /// Computer-Modern *text* font (CMR/CMBX/CMSS/CMTI/CMTT/SFRM…) with no
    /// ToUnicode: decode its low codes through the OT1 (TeX text) encoding so
    /// f-ligatures (0x0b-0x0f) and accents/specials (0x10-0x1f) aren't emitted as
    /// control chars. Math fonts (CMMI/CMSY/CMEX) are excluded — different encoding.
    ot1_text: bool,
    /// CM math font family with no ToUnicode: 1 = CMMI/CMMIB/EUFM (Greek + math italic),
    /// 2 = CMSY/CMBSY/MSAM/MSBM (relations/operators). 0 = not a (decodable) math font.
    /// Low codes are decoded through the built-in TeX math encoding instead of being
    /// dropped as control chars.
    cm_math: u8,
    /// Glyph advance widths in 1000-em units, keyed by code/CID.
    widths: HashMap<u32, f32>,
    default_width: f32,
    bold: bool,
    italic: bool,
    mono: bool,
    /// Stable id of the font face (FNV-1a hash of the BaseFont name with its 6-char
    /// subset prefix stripped). 0 = unknown. Lets the style profile tell a heading face
    /// from the body face when they share a size but differ in font.
    font_id: u32,
}

/// FNV-1a hash of a font's BaseFont name, ignoring any `ABCDEF+` subset prefix, so the
/// same logical face subsetted differently across pages hashes the same. 0 = empty.
fn font_id_of(basefont: &str) -> u32 {
    let b = basefont.as_bytes();
    let start = if b.len() >= 7 && b[6] == b'+' && b[..6].iter().all(|c| c.is_ascii_alphabetic()) {
        7
    } else {
        0
    };
    let name = &b[start..];
    if name.is_empty() {
        return 0;
    }
    let mut h: u32 = 2166136261;
    for &c in name {
        h = h.wrapping_mul(16777619) ^ c as u32;
    }
    h
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
        out.insert(name, font_info(doc, dict, raw));
    }
    out
}

/// Build a font table from a resources dictionary's `/Font` subdict (used for
/// Form XObjects, whose fonts live in their own /Resources rather than the page's).
fn build_fonts_from_resources(doc: &Document, resources: &Dictionary, raw: &[u8]) -> HashMap<Vec<u8>, FontInfo> {
    let mut out = HashMap::new();
    if let Some(fd) = resources.get(b"Font").ok().and_then(|o| deref(doc, o)).and_then(|o| o.as_dict().ok()) {
        for (name, val) in fd.iter() {
            if let Some(dict) = deref(doc, val).and_then(|o| o.as_dict().ok()) {
                out.insert(name.clone(), font_info(doc, dict, raw));
            }
        }
    }
    out
}

/// Parse a Type1 FontFile's built-in `/Encoding` (cleartext `dup N /glyph put`
/// entries, before the binary `eexec` section) into code → text. Subset Computer
/// Modern fonts re-pack glyphs to compact codes here and carry no PDF-level
/// Encoding/Differences/ToUnicode, so without this their punctuation (notably the
/// decimal point, often moved off 0x2E) mis-decodes via the raw byte fallback.
fn type1_encoding(doc: &Document, descriptor: Option<&Dictionary>) -> Option<HashMap<u32, String>> {
    let r = descriptor?.get(b"FontFile").ok().and_then(|o| o.as_reference().ok())?;
    let stream = doc.get_object(r).ok().and_then(|o| o.as_stream().ok())?;
    let bytes = crate::pdfobj::content_bytes(stream);
    let end = bytes.windows(5).position(|w| w == b"eexec").unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]);
    let toks: Vec<&str> = text.split_whitespace().collect();
    let mut m = HashMap::new();
    let mut i = 0;
    while i + 3 < toks.len() {
        if toks[i] == "dup" {
            if let (Ok(code), Some(name)) = (toks[i + 1].parse::<u32>(), toks[i + 2].strip_prefix('/')) {
                if toks[i + 3] == "put" {
                    if let Some(t) = glyph_to_str(name) {
                        m.insert(code, t);
                    } else if name.len() == 1 && name.as_bytes()[0].is_ascii_alphanumeric() {
                        m.insert(code, name.to_string());
                    }
                }
            }
        }
        i += 1;
    }
    (!m.is_empty()).then_some(m)
}

/// Construct a [`FontInfo`] from a font dictionary.
fn font_info(doc: &Document, dict: &Dictionary, raw: &[u8]) -> FontInfo {
    {
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
                    .map(|s| crate::pdfobj::content_bytes(s).into_owned())
                    .filter(|b| !b.is_empty());
                let bytes = from_lopdf.or_else(|| recover_stream(raw, r.0))?;
                Some(parse_tounicode(&bytes))
            });

        let identity_unicode = two_byte && to_unicode.as_ref().is_some_and(is_identity_cmap);

        // /Encoding /Differences: code -> glyph name -> text. Only kept for simple
        // (non-Type0) fonts; resolves f-ligatures and named punctuation that have
        // no ToUnicode mapping.
        let mut differences = (!two_byte)
            .then(|| {
                dict.get(b"Encoding")
                    .ok()
                    .and_then(|o| deref(doc, o))
                    .and_then(|o| o.as_dict().ok())
                    .and_then(|enc| enc.get(b"Differences").ok())
                    .and_then(|o| deref(doc, o))
                    .and_then(|o| o.as_array().ok())
                    .map(|arr| {
                        let mut m = HashMap::new();
                        let mut code = 0u32;
                        for it in arr {
                            match it {
                                Object::Integer(i) => code = *i as u32,
                                Object::Real(r) => code = *r as u32,
                                Object::Name(n) => {
                                    if let Some(t) = glyph_to_str(&String::from_utf8_lossy(n)) {
                                        m.insert(code, t);
                                    }
                                    code += 1;
                                }
                                _ => {}
                            }
                        }
                        m
                    })
            })
            .flatten()
            .filter(|m: &HashMap<u32, String>| !m.is_empty());

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

        // Style flags from BaseFont name + FontDescriptor /Flags.
        let basefont = dict
            .get(b"BaseFont")
            .and_then(|o| o.as_name())
            .map(|n| String::from_utf8_lossy(n).to_lowercase())
            .unwrap_or_default();
        let descriptor = dict
            .get(b"FontDescriptor")
            .ok()
            .and_then(|o| deref(doc, o))
            .or_else(|| {
                dict.get(b"DescendantFonts")
                    .ok()
                    .and_then(|o| deref(doc, o))
                    .and_then(|o| o.as_array().ok())
                    .and_then(|a| a.first())
                    .and_then(|o| deref(doc, o))
                    .and_then(|o| o.as_dict().ok())
                    .and_then(|dd| dd.get(b"FontDescriptor").ok())
                    .and_then(|o| deref(doc, o))
            })
            .and_then(|o| o.as_dict().ok());
        // Subset Type1 fonts (notably Computer Modern) carry NO PDF Encoding/
        // Differences/ToUnicode but re-pack their glyphs to compact codes in the
        // embedded FontFile's built-in /Encoding. Read it so their punctuation
        // (e.g. a decimal point reassigned away from 0x2E) decodes correctly.
        if differences.is_none() && !two_byte {
            differences = type1_encoding(doc, descriptor);
        }
        let flags = descriptor.and_then(|d| d.get(b"Flags").ok()).and_then(obj_i64).unwrap_or(0);
        // Bold incl. heading weights (LaTeX/Nimbus heading faces are "Medi"/semibold,
        // not literally "Bold", and often don't set the ForceBold descriptor flag).
        let bold = ["bold", "black", "heavy", "semibold", "demibold", "-medi", "medium", "cmbx"]
            .iter()
            .any(|w| basefont.contains(w))
            || (flags & 0x40000) != 0;
        let italic =
            basefont.contains("italic") || basefont.contains("oblique") || (flags & 0x40) != 0;
        let mono = basefont.contains("mono")
            || basefont.contains("courier")
            || basefont.contains("consol")
            || (flags & 0x1) != 0;

        // A non-embedded standard base font (Times/Helvetica/Courier/Symbol/ZapfDingbats)
        // with NO /Widths: use the Adobe Standard-14 AFM advances rather than a flat 0.5-em
        // guess. The guess closes narrow two-column gutters (justified lines overshoot) and
        // mis-sizes table columns; the real metrics fix both. Only fills when /Widths was
        // absent (embedded subset fonts carry their own /Widths and keep it).
        if !two_byte && widths.is_empty() {
            if let Some(table) = crate::afm::standard_widths(&basefont, bold, italic) {
                for (code, &w) in table.iter().enumerate() {
                    if w != 0 {
                        widths.insert(code as u32, w as f32);
                    }
                }
            } else if let Some(mw) = descriptor.and_then(|d| d.get(b"MissingWidth").ok()).and_then(obj_i64) {
                // Non-standard font without /Widths: the FontDescriptor's /MissingWidth is a
                // better default than the flat 0.5-em fallback.
                default_width = mw as f32;
            }
        }

        // CM *text* family (not the CMMI/CMSY/CMEX math fonts, which use a
        // different built-in encoding). Only used when there's no ToUnicode and
        // the code isn't covered by /Differences.
        let is_cm_math = ["cmmi", "cmsy", "cmex", "cmbsy", "cmmib", "msam", "msbm", "eufm", "eufb"]
            .iter()
            .any(|w| basefont.contains(w));
        let ot1_text = !two_byte
            && to_unicode.is_none()
            && !is_cm_math
            && ["cmr", "cmbx", "cmss", "cmsl", "cmti", "cmtt", "cmcsc", "cmdunh",
                "cmvtt", "cmtex", "cmff", "cmfib", "sfrm", "sfbx", "sfss", "sftt", "sfsl", "sfti"]
                .iter()
                .any(|w| basefont.contains(w));

        // CM math font whose low codes carry Greek / operators via the built-in TeX
        // encoding (no ToUnicode) — decode them rather than dropping as control chars.
        let cm_math: u8 = if !two_byte && to_unicode.is_none() {
            if ["cmmi", "cmmib", "eufm", "eufb", "eurm", "eurb"].iter().any(|w| basefont.contains(w)) {
                1
            } else if ["cmsy", "cmbsy", "msam", "msbm"].iter().any(|w| basefont.contains(w)) {
                2
            } else {
                0
            }
        } else {
            0
        };
        // The declared base encoding, resolved to a code → Unicode table. This is the
        // per-code fallback for a ToUnicode CMap that omits codes (see the field doc).
        // Three gates, each of which exists to prevent a confidently-wrong substitution:
        //  * simple fonts only — a Type0 `/Encoding` names a CMap, not a Latin table;
        //  * only when the producer actually *declared* one — no `/Encoding` means "use
        //    the font program's built-in cmap", which a subsetter re-packs arbitrarily;
        //  * not for a font the descriptor flags symbolic (and not also nonsymbolic):
        //    its built-in cmap wins over any `/Encoding` it spuriously also carries.
        // CM text/math fonts are excluded too — their built-in TeX encoding, decoded by
        // `ot1_text`/`cm_math` further down, is the right table for them.
        let symbolic = (flags & 0x4) != 0 && (flags & 0x20) == 0;
        let base_encoding = (!two_byte && !symbolic && !ot1_text && cm_math == 0)
            .then(|| {
                let enc = dict.get(b"Encoding").ok().and_then(|o| deref(doc, o))?;
                let name: &[u8] = match enc {
                    Object::Name(n) => n,
                    Object::Dictionary(d) => d.get(b"BaseEncoding").ok().and_then(|o| o.as_name().ok())?,
                    _ => return None,
                };
                crate::encoding::base_encoding_table(name)
            })
            .flatten();

        let font_id = font_id_of(&basefont);
        FontInfo { two_byte, to_unicode, identity_unicode, differences, base_encoding, ot1_text, cm_math, widths, default_width, bold, italic, mono, font_id }
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

/// Caps guarding a hostile/garbled ToUnicode CMap: a `beginbfrange` like
/// `<0000> <FFFFFFFF> <0041>` would otherwise loop billions of times. The CID code space is
/// 16-bit, so a single range over 65 536 codes — or a map past ~1M entries — is pathological.
const MAX_CMAP_RANGE: u32 = 1 << 16;
const MAX_CMAP_ENTRIES: usize = 1 << 20;

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
                        let lo = be_u32(lo);
                        // Clamp a pathological range to the 16-bit CID space (no billion-iter loop).
                        let hi = be_u32(hi).min(lo.saturating_add(MAX_CMAP_RANGE - 1));
                        let base = utf16be(dst);
                        // Common case: single-char dst incremented across the range.
                        let base_cp = base.chars().next().map(|c| c as u32);
                        for (k, code) in (lo..=hi).enumerate() {
                            if map.len() >= MAX_CMAP_ENTRIES {
                                break;
                            }
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

/// A ToUnicode table this small proves nothing — a handful of accidentally-identity entries
/// (`<0041> -> A` in a subset font that happens to keep the Latin glyph order) must not be
/// read as "the CID space is Unicode". The real thing is a producer-wide table covering whole
/// Unicode blocks.
const IDENTITY_CMAP_MIN_ENTRIES: usize = 256;
/// Near-total, not total: a real identity table may still normalise a few code points
/// (variant Greek letters, presentation forms) through explicit `bfchar` entries.
const IDENTITY_CMAP_MIN_RATIO: f32 = 0.95;

/// Is this ToUnicode CMap an *identity* table — code N -> the character U+N?
///
/// Only meaningful for a Type0 font. Two shapes exist in the wild:
/// * a **subset** font, where the CIDs are glyph indices (1, 2, 3…) and ToUnicode is the only
///   thing that recovers the text. Its table is small and never identity.
/// * a font whose CIDs *are* Unicode code points, shipped with a big machine-generated
///   identity ToUnicode. `unicode_showcase.pdf`'s `/MainFont` is one: 38 917 entries, 100 %
///   identity, covering most of the BMP.
///
/// The distinction matters because the second kind has **gaps** — its table simply stops
/// short of some blocks (Supplemental Arrows-A, Geometric Shapes Extended…), and a code in a
/// gap still carries its text in the code itself. Recognising the shape lets the decoder fall
/// back to the code for exactly those fonts, without ever emitting a subset font's glyph
/// indices as if they were characters.
fn is_identity_cmap(map: &HashMap<u32, String>) -> bool {
    if map.len() < IDENTITY_CMAP_MIN_ENTRIES {
        return false;
    }
    let identity = map
        .iter()
        .filter(|(&code, text)| {
            let mut it = text.chars();
            it.next().map(|c| c as u32) == Some(code) && it.next().is_none()
        })
        .count();
    identity as f32 >= map.len() as f32 * IDENTITY_CMAP_MIN_RATIO
}

/// Resolve a PostScript / Adobe-Glyph-List glyph name to its text. Returns None
/// for names we don't know, so the caller falls back to the raw code byte — this
/// only ever ADDS fidelity (its main job is f-ligatures and named punctuation in
/// `/Differences` encodings that carry no ToUnicode). ASCII-equivalent names map
/// to ASCII so downstream logic (de-hyphenation on '-', etc.) is unaffected.
fn glyph_to_str(name: &str) -> Option<String> {
    // uniXXXX (one BMP codepoint) and uXXXX..uXXXXXX forms.
    if let Some(hex) = name.strip_prefix("uni") {
        if hex.len() >= 4 && hex.as_bytes()[..4].iter().all(|b| b.is_ascii_hexdigit()) {
            if let Some(c) = u32::from_str_radix(&hex[..4], 16).ok().and_then(char::from_u32) {
                return Some(c.to_string());
            }
        }
    }
    if let Some(hex) = name.strip_prefix('u') {
        if (4..=6).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Some(c) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                return Some(c.to_string());
            }
        }
    }
    let s = match name {
        // f-ligatures + long-s / dotless (the Phase-A target).
        "ff" => "ff", "fi" => "fi", "fl" => "fl", "ffi" => "ffi", "ffl" => "ffl",
        "ft" => "ft", "fj" => "fj", "st" => "st", "longst" => "st",
        "dotlessi" => "ı", "dotlessj" => "ȷ",
        // ASCII punctuation / digits / brackets (kept ASCII on purpose).
        "space" => " ", "exclam" => "!", "quotedbl" => "\"", "numbersign" => "#",
        "dollar" => "$", "percent" => "%", "ampersand" => "&", "quotesingle" => "'",
        "parenleft" => "(", "parenright" => ")", "asterisk" => "*", "plus" => "+",
        "comma" => ",", "hyphen" => "-", "sfthyphen" => "-", "period" => ".", "slash" => "/",
        "zero" => "0", "one" => "1", "two" => "2", "three" => "3", "four" => "4",
        "five" => "5", "six" => "6", "seven" => "7", "eight" => "8", "nine" => "9",
        "colon" => ":", "semicolon" => ";", "less" => "<", "equal" => "=",
        "greater" => ">", "question" => "?", "at" => "@",
        "bracketleft" => "[", "backslash" => "\\", "bracketright" => "]",
        "asciicircum" => "^", "underscore" => "_", "grave" => "`",
        "braceleft" => "{", "bar" => "|", "braceright" => "}", "asciitilde" => "~",
        // Typographic punctuation.
        "quoteleft" => "\u{2018}", "quoteright" => "\u{2019}",
        "quotedblleft" => "\u{201C}", "quotedblright" => "\u{201D}",
        "quotesinglbase" => "\u{201A}", "quotedblbase" => "\u{201E}",
        "bullet" => "\u{2022}", "endash" => "\u{2013}", "emdash" => "\u{2014}",
        "periodcentered" => "\u{00B7}", "ellipsis" => "\u{2026}",
        "dagger" => "\u{2020}", "daggerdbl" => "\u{2021}", "perthousand" => "\u{2030}",
        "guilsinglleft" => "\u{2039}", "guilsinglright" => "\u{203A}",
        "guillemotleft" => "\u{00AB}", "guillemotright" => "\u{00BB}",
        "minus" => "\u{2212}", "fraction" => "\u{2044}", "florin" => "\u{0192}",
        "trademark" => "\u{2122}", "copyright" => "\u{00A9}", "registered" => "\u{00AE}",
        "degree" => "\u{00B0}", "plusminus" => "\u{00B1}", "multiply" => "\u{00D7}",
        "divide" => "\u{00F7}", "section" => "\u{00A7}", "paragraph" => "\u{00B6}",
        "sterling" => "\u{00A3}", "euro" => "\u{20AC}", "yen" => "\u{00A5}", "cent" => "\u{00A2}",
        // Spacing diacritics.
        "acute" => "\u{00B4}", "dieresis" => "\u{00A8}", "circumflex" => "\u{02C6}",
        "tilde" => "\u{02DC}", "macron" => "\u{00AF}", "breve" => "\u{02D8}",
        "cedilla" => "\u{00B8}", "caron" => "\u{02C7}", "ogonek" => "\u{02DB}",
        "ring" => "\u{02DA}", "dotaccent" => "\u{02D9}", "hungarumlaut" => "\u{02DD}",
        // Accented Latin (Western + Romanian/Polish/Czech the corpus uses).
        "aacute" => "á", "Aacute" => "Á", "agrave" => "à", "Agrave" => "À",
        "acircumflex" => "â", "Acircumflex" => "Â", "atilde" => "ã", "Atilde" => "Ã",
        "adieresis" => "ä", "Adieresis" => "Ä", "aring" => "å", "Aring" => "Å",
        "abreve" => "ă", "Abreve" => "Ă", "aogonek" => "ą", "Aogonek" => "Ą",
        "ae" => "æ", "AE" => "Æ",
        "ccedilla" => "ç", "Ccedilla" => "Ç", "cacute" => "ć", "Cacute" => "Ć",
        "ccaron" => "č", "Ccaron" => "Č",
        "dcaron" => "ď", "Dcaron" => "Ď", "dcroat" => "đ", "Dcroat" => "Đ",
        "eacute" => "é", "Eacute" => "É", "egrave" => "è", "Egrave" => "È",
        "ecircumflex" => "ê", "Ecircumflex" => "Ê", "edieresis" => "ë", "Edieresis" => "Ë",
        "ecaron" => "ě", "Ecaron" => "Ě", "eogonek" => "ę", "Eogonek" => "Ę",
        "gbreve" => "ğ", "Gbreve" => "Ğ",
        "iacute" => "í", "Iacute" => "Í", "igrave" => "ì", "Igrave" => "Ì",
        "icircumflex" => "î", "Icircumflex" => "Î", "idieresis" => "ï", "Idieresis" => "Ï",
        "lslash" => "ł", "Lslash" => "Ł", "lacute" => "ĺ", "lcaron" => "ľ",
        "nacute" => "ń", "Nacute" => "Ń", "ntilde" => "ñ", "Ntilde" => "Ñ",
        "ncaron" => "ň", "Ncaron" => "Ň",
        "oacute" => "ó", "Oacute" => "Ó", "ograve" => "ò", "Ograve" => "Ò",
        "ocircumflex" => "ô", "Ocircumflex" => "Ô", "otilde" => "õ", "Otilde" => "Õ",
        "odieresis" => "ö", "Odieresis" => "Ö", "oslash" => "ø", "Oslash" => "Ø",
        "ohungarumlaut" => "ő", "Ohungarumlaut" => "Ő", "oe" => "œ", "OE" => "Œ",
        "racute" => "ŕ", "rcaron" => "ř", "Rcaron" => "Ř",
        "sacute" => "ś", "Sacute" => "Ś", "scaron" => "š", "Scaron" => "Š",
        "scedilla" => "ş", "Scedilla" => "Ş", "scommaaccent" => "ș", "Scommaaccent" => "Ș",
        "tcaron" => "ť", "Tcaron" => "Ť", "tcommaaccent" => "ț", "Tcommaaccent" => "Ț",
        "uacute" => "ú", "Uacute" => "Ú", "ugrave" => "ù", "Ugrave" => "Ù",
        "ucircumflex" => "û", "Ucircumflex" => "Û", "udieresis" => "ü", "Udieresis" => "Ü",
        "uring" => "ů", "Uring" => "Ů", "uhungarumlaut" => "ű", "uogonek" => "ų",
        "yacute" => "ý", "Yacute" => "Ý", "ydieresis" => "ÿ", "Ydieresis" => "Ÿ",
        "zacute" => "ź", "Zacute" => "Ź", "zcaron" => "ž", "Zcaron" => "Ž",
        "zdotaccent" => "ż", "Zdotaccent" => "Ż",
        "germandbls" => "ß", "thorn" => "þ", "Thorn" => "Þ", "eth" => "ð", "Eth" => "Ð",
        "exclamdown" => "¡", "questiondown" => "¿", "ordfeminine" => "ª", "ordmasculine" => "º",
        // Greek letters (Adobe Glyph List names) used by math fonts (PazoMath, Euler,
        // MathTime) via /Differences with no ToUnicode.
        "alpha" => "α", "beta" => "β", "gamma" => "γ", "delta" => "δ", "epsilon" => "ε",
        "zeta" => "ζ", "eta" => "η", "theta" => "θ", "iota" => "ι", "kappa" => "κ",
        "lambda" => "λ", "mu" => "μ", "nu" => "ν", "xi" => "ξ", "omicron" => "ο",
        "pi" => "π", "rho" => "ρ", "sigma" => "σ", "sigma1" => "ς", "tau" => "τ",
        "upsilon" => "υ", "phi" => "φ", "chi" => "χ", "psi" => "ψ", "omega" => "ω",
        "varepsilon" => "ε", "vartheta" => "ϑ", "varpi" => "ϖ", "varrho" => "ϱ",
        "varsigma" => "ς", "varphi" => "ϕ", "phi1" => "ϕ", "theta1" => "ϑ",
        "Alpha" => "Α", "Beta" => "Β", "Gamma" => "Γ", "Delta" => "Δ", "Epsilon" => "Ε",
        "Zeta" => "Ζ", "Eta" => "Η", "Theta" => "Θ", "Iota" => "Ι", "Kappa" => "Κ",
        "Lambda" => "Λ", "Mu" => "Μ", "Nu" => "Ν", "Xi" => "Ξ", "Omicron" => "Ο",
        "Pi" => "Π", "Rho" => "Ρ", "Sigma" => "Σ", "Tau" => "Τ", "Upsilon" => "Υ",
        "Phi" => "Φ", "Chi" => "Χ", "Psi" => "Ψ", "Omega" => "Ω",
        // Math operators / relations (AGL names).
        "summation" => "∑", "product" => "∏", "integral" => "∫", "radical" => "√",
        "partialdiff" => "∂", "gradient" => "∇", "nabla" => "∇", "infinity" => "∞",
        "element" => "∈", "notelement" => "∉", "owner" => "∋", "emptyset" => "∅",
        "intersection" => "∩", "union" => "∪", "logicaland" => "∧", "logicalor" => "∨",
        "logicalnot" => "¬", "existential" => "∃", "universal" => "∀",
        "lessequal" => "≤", "greaterequal" => "≥", "notequal" => "≠", "equivalence" => "≡",
        "approxequal" => "≈", "similar" => "∼", "congruent" => "≅", "proportional" => "∝",
        "propersubset" => "⊂", "propersuperset" => "⊃", "reflexsubset" => "⊆",
        "reflexsuperset" => "⊇", "arrowright" => "→", "arrowleft" => "←", "arrowup" => "↑",
        "arrowdown" => "↓", "arrowboth" => "↔", "arrowdblright" => "⇒", "arrowdblleft" => "⇐",
        "arrowdblboth" => "⇔", "minute" => "′", "second" => "″", "aleph" => "ℵ",
        "circlemultiply" => "⊗", "circleplus" => "⊕", "perpendicular" => "⊥", "angle" => "∠",
        "asteriskmath" => "∗", "dotmath" => "⋅", "circlemath" => "∘",
        // Single-letter names (A..Z, a..z) and unknowns.
        _ => {
            if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
                return Some(name.to_string());
            }
            return None;
        }
    };
    Some(s.to_string())
}

/// OT1 (TeX text) encoding for the positions that differ from ASCII — used for
/// Computer-Modern *text* fonts that carry no ToUnicode. Covers all of 0x00-0x1f
/// (uppercase Greek, f-ligatures, dotless i/j, accents, special letters) plus the
/// quote/dash positions, so CM text never leaks control chars and reads correctly.
fn ot1_text_code(c: u8) -> Option<&'static str> {
    Some(match c {
        0x00 => "Γ", 0x01 => "Δ", 0x02 => "Θ", 0x03 => "Λ", 0x04 => "Ξ", 0x05 => "Π",
        0x06 => "Σ", 0x07 => "Υ", 0x08 => "Φ", 0x09 => "Ψ", 0x0a => "Ω",
        0x0b => "ff", 0x0c => "fi", 0x0d => "fl", 0x0e => "ffi", 0x0f => "ffl",
        0x10 => "ı", 0x11 => "ȷ", 0x12 => "`", 0x13 => "´", 0x14 => "ˇ", 0x15 => "˘",
        0x16 => "¯", 0x17 => "˚", 0x18 => "¸", 0x19 => "ß", 0x1a => "æ", 0x1b => "œ",
        0x1c => "ø", 0x1d => "Æ", 0x1e => "Œ", 0x1f => "Ø",
        // text-encoding quote/dash positions (ASCII glyphs that CM text repurposes)
        0x22 => "”", 0x27 => "’", 0x5c => "“", 0x60 => "‘", 0x7b => "–", 0x7c => "—",
        0x7d => "˝", 0x7e => "˜", 0x7f => "¨",
        _ => return None,
    })
}

/// Windows-1252 C1 range (0x80-0x9F) — smart quotes, dashes, ellipsis, bullet. A font
/// that declares no encoding and no ToUnicode emits these as raw bytes; mapped here
/// before they fall through to `push_norm` (which would drop them as C1 control chars).
fn c1_remap(b: u8) -> Option<&'static str> {
    Some(match b {
        0x82 => "‚", 0x84 => "„", 0x85 => "…", 0x86 => "†", 0x87 => "‡",
        0x88 => "ˆ", 0x89 => "‰", 0x8b => "‹", 0x91 => "‘", 0x92 => "’",
        0x93 => "“", 0x94 => "”", 0x95 => "•", 0x96 => "–", 0x97 => "—",
        0x98 => "˜", 0x99 => "™", 0x9b => "›",
        _ => return None,
    })
}

/// Computer-Modern Math Italic (CMMI/CMMIB) built-in encoding for fonts with no
/// ToUnicode: uppercase + lowercase Greek (incl. variants), `∂`, and the math-italic
/// Latin letters (recovered as plain letters — they are variable names). Slots are the
/// canonical TeX `cmmi` encoding.
fn cmmi_code(c: u8) -> Option<&'static str> {
    Some(match c {
        0x00 => "Γ", 0x01 => "Δ", 0x02 => "Θ", 0x03 => "Λ", 0x04 => "Ξ", 0x05 => "Π",
        0x06 => "Σ", 0x07 => "Υ", 0x08 => "Φ", 0x09 => "Ψ", 0x0a => "Ω",
        0x0b => "α", 0x0c => "β", 0x0d => "γ", 0x0e => "δ", 0x0f => "ε", 0x10 => "ζ",
        0x11 => "η", 0x12 => "θ", 0x13 => "ι", 0x14 => "κ", 0x15 => "λ", 0x16 => "μ",
        0x17 => "ν", 0x18 => "ξ", 0x19 => "π", 0x1a => "ρ", 0x1b => "σ", 0x1c => "τ",
        0x1d => "υ", 0x1e => "φ", 0x1f => "χ", 0x20 => "ψ", 0x21 => "ω",
        0x22 => "ε", 0x23 => "ϑ", 0x24 => "ϖ", 0x25 => "ϱ", 0x26 => "ς", 0x27 => "ϕ",
        0x40 => "∂", 0x60 => "ℓ", 0x7b => "ı", 0x7c => "ȷ", 0x7d => "℘",
        // 0x41-0x5a (A-Z) and 0x61-0x7a (a-z) are math-italic Latin variables — already
        // valid ASCII, so they pass through `push_norm` unchanged (return None here).
        _ => return None,
    })
}

/// Computer-Modern Symbol (CMSY/CMBSY) built-in encoding for fonts with no ToUnicode:
/// the relations, operators and arrows of TeX math. Canonical TeX `cmsy` slots; only the
/// well-established, high-frequency positions are mapped (uncertain slots fall through
/// unchanged, exactly as today). Calligraphic caps are recovered as plain A-Z.
fn cmsy_code(c: u8) -> Option<&'static str> {
    Some(match c {
        0x00 => "−", 0x01 => "·", 0x02 => "×", 0x03 => "∗", 0x04 => "÷", 0x05 => "⋄",
        0x06 => "±", 0x07 => "∓", 0x08 => "⊕", 0x09 => "⊖", 0x0a => "⊗", 0x0b => "⊘",
        0x0c => "⊙", 0x0f => "•",
        0x11 => "≡", 0x12 => "⊆", 0x13 => "⊇", 0x14 => "≤", 0x15 => "≥",
        0x18 => "∼", 0x19 => "≈", 0x1a => "⊂", 0x1b => "⊃", 0x1c => "≪", 0x1d => "≫",
        0x20 => "←", 0x21 => "→", 0x22 => "↑", 0x23 => "↓", 0x24 => "↔",
        0x27 => "≃", 0x28 => "⇐", 0x29 => "⇒", 0x2c => "⇔",
        0x2f => "∝", 0x30 => "′", 0x31 => "∞", 0x32 => "∈", 0x33 => "∋",
        0x38 => "∀", 0x39 => "∃", 0x3a => "¬", 0x3b => "∅", 0x3c => "ℜ", 0x3d => "ℑ",
        0x3e => "⊤", 0x3f => "⊥", 0x40 => "ℵ",
        // 0x41-0x5a are calligraphic caps — pass through as plain ASCII A-Z (None).
        0x5b => "∪", 0x5c => "∩", 0x5e => "∧", 0x5f => "∨", 0x60 => "⊢", 0x61 => "⊣",
        0x6a => "∥", 0x6e => "√", 0x72 => "∇",
        _ => return None,
    })
}

/// Normalise a decoded char: map zero-width spaces to a real space, drop other
/// zero-width / BOM noise and stray control glyphs that pollute word boundaries.
fn push_norm(out: &mut String, ch: char) {
    // Safety net: never emit a raw control char. A glyph that decoded to one is an
    // unmapped ligature/symbol (no usable text); dropping it beats polluting output.
    if (ch as u32) < 0x20 && ch != '\t' && ch != '\n' && ch != '\r' {
        return;
    }
    match ch {
        // Latin ligatures -> ASCII (matches PyMuPDF; critical for LaTeX/arXiv text).
        '\u{FB00}' => out.push_str("ff"),
        '\u{FB01}' => out.push_str("fi"),
        '\u{FB02}' => out.push_str("fl"),
        '\u{FB03}' => out.push_str("ffi"),
        '\u{FB04}' => out.push_str("ffl"),
        '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
        // whitespace variants -> space
        '\u{200B}' | '\u{00A0}' | '\u{2009}' | '\u{202F}' => out.push(' '),
        // zero-width / BOM noise -> drop
        '\u{FEFF}' | '\u{200C}' | '\u{200D}' | '\0' => {}
        c => out.push(c),
    }
}

/// A decoded word: text (incl. any trailing space, for clean re-joining), the x
/// offset from the start of the show operator, and the advance of its glyphs
/// (excluding the trailing space, so `x_off + width` is the last glyph's edge).
struct Word {
    text: String,
    x_off: f32,
    width: f32,
}

/// One element of a show operator: a coded string, or a TJ kern (1000-em units).
enum Show<'a> {
    Str(&'a [u8]),
    Kern(f32),
}

/// Decode a show operator (Tj = one `Str`; TJ = strings interleaved with kerns)
/// into word spans, splitting at space glyphs and large kern gaps so each word
/// carries its own position — the substrate for column/table detection. Returns
/// the words and the total horizontal advance.
fn decode_words(elems: &[Show], font: Option<&FontInfo>, size: f32, tc: f32, tw: f32) -> (Vec<Word>, f32) {
    let mut words: Vec<Word> = Vec::new();
    let mut x = 0.0f32; // running advance from the operator start
    let mut cur = String::new();
    let mut cur_start = 0.0f32;
    let mut cur_end = 0.0f32;
    let mut pending_break = false; // a space/gap was seen; next glyph starts a word

    let flush = |cur: &mut String, start: f32, end: f32, words: &mut Vec<Word>| {
        if cur.chars().any(|c| !c.is_whitespace()) {
            words.push(Word { text: std::mem::take(cur), x_off: start, width: (end - start).max(0.0) });
        } else {
            cur.clear();
        }
    };

    // Append one decoded code: its char string `s`, its advance `w`, at offset `cx`.
    let push_code = |s: &str, w: f32, cx: f32,
                         cur: &mut String, cur_start: &mut f32, cur_end: &mut f32,
                         pending_break: &mut bool, words: &mut Vec<Word>| {
        let ws_only = !s.is_empty() && s.chars().all(|c| c.is_whitespace());
        if s.is_empty() {
            // A glyph that decodes to nothing but still advances a space-sized gap
            // is a space whose ToUnicode is missing (common in CID fonts) — keep
            // the word boundary so words don't merge ("of the" not "ofthe").
            if w > size * 0.15 {
                cur.push(' ');
                *pending_break = true;
            }
            return;
        }
        if ws_only {
            cur.push(' '); // keep one separating space with the word
            *pending_break = true;
        } else {
            if *pending_break || cur.is_empty() {
                flush(cur, *cur_start, *cur_end, words);
                *cur_start = cx;
                *pending_break = false;
            }
            cur.push_str(s);
            *cur_end = cx + w;
        }
    };

    for el in elems {
        match el {
            Show::Kern(k) => {
                // TJ number: advance is -k/1000*size; a large positive gap (k<-150)
                // is a visible space → word break.
                x += -k / 1000.0 * size;
                if *k < -150.0 {
                    cur.push(' ');
                    pending_break = true;
                }
            }
            Show::Str(bytes) => match font {
                Some(fi) => {
                    let step = if fi.two_byte { 2 } else { 1 };
                    let mut i = 0;
                    while i < bytes.len() {
                        let end = (i + step).min(bytes.len());
                        let code = be_u32(&bytes[i..end]);
                        let mut s = String::new();
                        let mut got = false;
                        if let Some(map) = &fi.to_unicode {
                            if let Some(t) = map.get(&code) {
                                for ch in t.chars() {
                                    push_norm(&mut s, ch);
                                }
                                got = true;
                            }
                        }
                        // /Differences fills the gap for simple fonts with no (or
                        // incomplete) ToUnicode — notably f-ligatures coded as 2/3.
                        if !got {
                            if let Some(diff) = &fi.differences {
                                if let Some(t) = diff.get(&code) {
                                    for ch in t.chars() {
                                        push_norm(&mut s, ch);
                                    }
                                    got = true;
                                }
                            }
                        }
                        // The font's DECLARED base encoding (`/WinAnsiEncoding` &c) — the
                        // per-code fallback for a ToUnicode CMap that simply omits codes.
                        // Distiller routinely writes one `bfchar` per subsetted glyph and
                        // leaves the rest out; without this, "Redding" decodes to "edding".
                        // Only ever set when the producer declared an encoding, so a symbolic
                        // font with no `/Encoding` still emits nothing rather than a wrong
                        // Latin letter (see `FontInfo::base_encoding`).
                        if !got {
                            if let Some(enc) = fi.base_encoding {
                                if let Some(ch) =
                                    enc.get(code as usize).copied().filter(|&u| u != 0).and_then(|u| char::from_u32(u as u32))
                                {
                                    push_norm(&mut s, ch);
                                    got = true;
                                }
                            }
                        }
                        // Raw fallback when there is no ToUnicode at all — a font that HAS
                        // ToUnicode but omits a code normally means "no glyph" (dropping it is
                        // correct; raw-falling-back would emit the CID as a control char).
                        // The exception is an *identity* ToUnicode (`identity_unicode`): there
                        // the CID space is Unicode, so an omitted code is a hole in the
                        // producer's table and the code itself is the text.
                        if !got && (fi.to_unicode.is_none() || fi.identity_unicode) {
                            if !fi.two_byte {
                                let b = bytes[i];
                                // CM text (OT1) → CM math (CMMI/CMSY) → Windows-1252 C1 →
                                // raw byte. Each layer only fires for its font kind, so a
                                // glyph that was being dropped is now recovered.
                                let mapped = if fi.ot1_text {
                                    ot1_text_code(b)
                                } else {
                                    match fi.cm_math {
                                        1 => cmmi_code(b),
                                        2 => cmsy_code(b),
                                        _ => None,
                                    }
                                };
                                match mapped.or_else(|| c1_remap(b)) {
                                    Some(t) => s.push_str(t),
                                    None => push_norm(&mut s, b as char),
                                }
                            } else if let Some(ch) = char::from_u32(code) {
                                push_norm(&mut s, ch);
                            }
                        }
                        let mut w = fi.widths.get(&code).copied().unwrap_or(fi.default_width) / 1000.0 * size + tc;
                        if !fi.two_byte && code == 32 {
                            w += tw;
                        }
                        push_code(&s, w, x, &mut cur, &mut cur_start, &mut cur_end, &mut pending_break, &mut words);
                        x += w;
                        i += step;
                    }
                }
                None => {
                    for &b in *bytes {
                        let mut s = String::new();
                        push_norm(&mut s, b as char);
                        let w = 0.5 * size + tc + if b == 32 { tw } else { 0.0 };
                        push_code(&s, w, x, &mut cur, &mut cur_start, &mut cur_end, &mut pending_break, &mut words);
                        x += w;
                    }
                }
            },
        }
    }
    flush(&mut cur, cur_start, cur_end, &mut words);
    (words, x)
}

/// 2x3 affine matrix (PDF row-vector convention): [a b c d e f].
/// A positioned run of text (origin in PDF user space, y increases upward).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SourceId(u32);

impl SourceId {
    const UNASSIGNED: Self = Self(u32::MAX);

    pub(crate) const fn ordinal(self) -> u32 {
        self.0
    }
}

/// The exact character interval contributed by one painted text occurrence.
///
/// Occurrence ids are assigned only after coincident-paint de-duplication, so two uses of the
/// same Form XObject at different page positions remain distinct while faux-bold overpainting
/// at one position remains one visible source. Character offsets, rather than byte offsets,
/// let a ruled-cell cut preserve Unicode text without exposing this bookkeeping publicly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SourceSlice {
    source: SourceId,
    char_start: u32,
    char_end: u32,
}

impl SourceSlice {
    fn unassigned() -> Self {
        Self {
            source: SourceId::UNASSIGNED,
            char_start: 0,
            char_end: 0,
        }
    }

    fn whole(source: SourceId, char_len: usize) -> Self {
        Self {
            source,
            char_start: 0,
            char_end: u32::try_from(char_len).expect("one text span cannot exceed u32 characters"),
        }
    }

    pub(crate) fn source(self) -> SourceId {
        self.source
    }

    pub(crate) const fn char_start(self) -> u32 {
        self.char_start
    }

    pub(crate) const fn char_end(self) -> u32 {
        self.char_end
    }

    pub(crate) const fn char_len(self) -> u32 {
        self.char_end - self.char_start
    }

    pub(crate) fn merge_if_touching(self, other: Self) -> Option<Self> {
        if self.source != other.source || other.char_start > self.char_end || other.char_end < self.char_start {
            return None;
        }
        Some(Self {
            source: self.source,
            char_start: self.char_start.min(other.char_start),
            char_end: self.char_end.max(other.char_end),
        })
    }

    pub(crate) fn sub_slice(self, relative_start: usize, relative_end: usize) -> Self {
        let start = u32::try_from(relative_start).expect("span split offset must fit u32");
        let end = u32::try_from(relative_end).expect("span split offset must fit u32");
        debug_assert!(start <= end && end <= self.char_len());
        Self {
            source: self.source,
            char_start: self.char_start + start,
            char_end: self.char_start + end,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_occurrence(ordinal: u32, char_len: usize) -> Self {
        Self::whole(SourceId(ordinal), char_len)
    }
}

#[derive(Clone)]
pub struct Span {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub width: f32,
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
    /// Baseline rotation in radians (CTM × text-matrix), 0 for upright text. Non-zero
    /// only for rotated text (e.g. a 90° y-axis title); kept out of the body flow and
    /// rendered as rotated SVG `<text>` when it's a figure label.
    pub angle: f32,
    /// Stable font-face id (see [`font_id_of`]); 0 = unknown.
    pub font: u32,
    /// The *marked-content id* this glyph run was painted under (`/MCID` of the innermost
    /// enclosing `BDC`), when the page declares one. This is the only handle that ties a
    /// tagged PDF's logical structure — `/StructTreeRoot` → `/Table` → `/TD` — to the actual
    /// glyphs, and it is what [`crate::structtree`]'s declared tables resolve against.
    ///
    /// Captured **only** for the page's own content stream. Marked-content ids inside a Form
    /// XObject or an annotation appearance number a *different* sequence (the form's own
    /// `/StructParents` key, §14.7.4.4), so stamping them with the page's numbering would
    /// associate a cell with unrelated text; those spans stay `None` and resolve by geometry
    /// or not at all.
    pub mcid: Option<u32>,
    /// Private painted-occurrence provenance. Assigned after coincident de-duplication and
    /// preserved by every geometry-only clone/turn; never projected into a public result.
    pub(crate) source: SourceSlice,
}

/// Extract positioned text spans for one page via content-stream interpretation,
/// recursing into the Form XObjects the page draws (`Do`). Body text rendered
/// through a template/overlay form — the norm in e-filing bundles (SEI/PROJUDI/PJe,
/// iText-assembled PDFs) where each appended document is a full-page Form XObject —
/// would otherwise be invisible to the page-direct walk. Page-direct and
/// form-internal text land in the same page user space.
pub fn extract_spans(doc: &Document, page_id: ObjectId, raw: &[u8]) -> Vec<Span> {
    let content = match doc.get_and_decode_page_content(page_id) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let fonts = build_fonts(doc, page_id, raw);
    let xmap = crate::walker::page_xobjects(doc, page_id);
    let mut spans = Vec::new();
    let mut budget = crate::WalkBudget::new(crate::MAX_FORM_WORK);
    let props = marked_properties(doc, page_id);
    decode_spans(doc, &content.operations, &fonts, &xmap, Mat::ID, raw, 0, &mut spans, &mut budget, Some(&props));
    // §12.5.5: an annotation's appearance stream is page content — a filled form field's
    // value, a stamp's caption — reachable from neither the content stream nor the page's
    // `/Resources`. `walker::placed_appearances` carries the `/BBox`→`/Rect` mapping that
    // puts it where a viewer puts it; its fonts are its own, exactly as a form's are.
    for (_, ap, actm) in crate::walker::placed_appearances(doc, page_id) {
        let f = match descend_form(doc, ap, &XMap::new(), ScopePolicy::OwnOnly, 0, &mut budget, fonts.len()) {
            Descend::Into(f) => f,
            Descend::Skip => continue,
            Descend::Halt => break,
        };
        let Some(fr) = &f.scope.resources else { continue };
        let ff = build_fonts_from_resources(doc, fr, raw);
        decode_spans(doc, &f.ops, &ff, &f.scope.xobjects, f.matrix.mul(actm), raw, 1, &mut spans, &mut budget, None);
    }
    dedup_coincident(&mut spans);
    assign_occurrence_provenance(&mut spans);
    spans
}



/// Emit one positioned text span from a decoded word, resolving its device position
/// from the text matrix and the graphics CTM (pure-translate / Y-flip / rotation
/// handling). A free fn — not a closure — so the page walk and its Form-XObject
/// recursion position text identically.
fn push_positioned_span(spans: &mut Vec<Span>, wtm: &Mat, ctm: &Mat, base_size: f32, width: f32, style: (bool, bool, bool, u32), s: String) {
        // Resolve the device position from the text matrix and the graphics CTM.
        // Resolve the device position from the text matrix and the graphics CTM.
        //  - ROTATED text (non-horizontal baseline) uses the full combined matrix: its
        //    true position, a magnitude-based height (so a 90° title isn't dropped by a
        //    near-zero `d`), and the baseline angle. (Baseline ~0 for italic SKEW, so
        //    skewed text stays upright and isn't pulled out of the body/tables.)
        //  - A PURE-TRANSLATE CTM (a≈d≈1, b≈c≈0) means the text is laid out in a LOCAL
        //    frame and only moved into place — reportlab/platypus and similar generators
        //    emit `q [1 0 0 1 tx ty] cm BT … ET Q` per block. Ignoring it collapses every
        //    block to local coords → scrambled reading order + lost text, so apply the
        //    translate. (CTM identity is the trivial sub-case: dm == wtm, no change — so
        //    the common Tm/Td-positioned corpus is byte-identical.)
        //  - Any OTHER CTM (scale / Y-flip / shear) transforms the whole coordinate
        //    system; such docs are internally consistent under the text matrix alone and
        //    re-tuning the whole pipeline to their device coords is a separate effort, so
        //    they keep the text-matrix position.
        let dm = wtm.mul(*ctm);
        let baseline = dm.b.atan2(dm.a);
        let rotated = baseline.abs() > 0.1;
        let pure_translate = !rotated
            && (ctm.a - 1.0).abs() < 1e-3
            && (ctm.d - 1.0).abs() < 1e-3
            && ctm.b.abs() < 1e-3
            && ctm.c.abs() < 1e-3;
        // `width` arrives in text space (glyph advance × the Tf size). The font size can
        // live in the text matrix instead of in Tf (size 1 in Tf, the real scale baked
        // into Tm), in which case `height` already picks the scale up via `wtm.d`; the
        // width must pick up the matching HORIZONTAL scale or it stays ~20× too small,
        // collapsing every word's measured extent and tearing lines apart on fake gaps.
        // Y-FLIP CTM (top-left origin): `[1 0 0 -1 0 H] cm` lays the page out with y growing
        // DOWNWARD. It is axis-aligned (b≈c≈0) but the device y-axis is inverted (dm.d<0).
        // Such a doc must use DEVICE coordinates (dm.f = H − text_y) so y is y-up like the
        // rest of the pipeline; keeping the raw text-matrix y would make a top glyph sort
        // below a bottom one and read the WHOLE document bottom-to-top (the SEC-filing bug).
        let yflip = !rotated && dm.d < 0.0 && dm.b.abs() < 1e-3 && dm.c.abs() < 1e-3;
        let (x, y, height, angle, sx) = if rotated {
            (dm.e, dm.f, base_size * (dm.c * dm.c + dm.d * dm.d).sqrt(), baseline, (dm.a * dm.a + dm.b * dm.b).sqrt())
        } else if pure_translate {
            (dm.e, dm.f, base_size * wtm.d, 0.0, wtm.a)
        } else if yflip {
            (dm.e, dm.f, base_size * dm.d.abs(), 0.0, dm.a.abs())
        } else {
            // Axis-aligned CTM that is neither a pure translate nor a pure Y-flip — e.g. a
            // SCALED (and possibly double-flipped) frame: a form drawn under `s 0 0 -s tx ty
            // cm`, the norm in e-filing template bundles where the body lives in a 0.06×,
            // Y-flipped XObject. When that scale/flip is in the CTM, the text matrix alone
            // omits it (positions collapse / invert); use the full device matrix. Identical
            // to `wtm` when the CTM is identity (dm == wtm), correct when it isn't.
            (dm.e, dm.f, base_size * dm.d.abs(), 0.0, dm.a.abs())
        };
        if !s.is_empty() && height.abs() >= 2.0 {
            spans.push(Span {
                x,
                y,
                size: height.abs().max(1.0),
                width: (width * sx).abs(),
                text: s,
                bold: style.0,
                italic: style.1,
                mono: style.2,
                angle,
                font: style.3,
                mcid: None, // stamped by the caller's marked-content stack (see `decode_spans`)
                source: SourceSlice::unassigned(),
            });
        }
}

/// Walk a content stream's operators, emitting positioned spans and recursing into
/// Form XObjects on `Do`. `base` is the graphics CTM the stream is placed under
/// (identity for the page; the form `/Matrix` × the invoking CTM for a nested form).
///
/// `props` is the stream's `/Resources /Properties` — the named property lists a `BDC` may
/// reference instead of writing its dictionary inline — and its presence is what switches
/// **marked-content capture** on. It is passed only for the page's own content stream: see
/// [`Span::mcid`] for why a form's or an annotation's marked-content ids must not be stamped
/// with the page's numbering.
#[allow(clippy::too_many_arguments)]
fn decode_spans(doc: &Document, ops: &[lopdf::content::Operation], fonts: &HashMap<Vec<u8>, FontInfo>, xmap: &XMap, base: Mat, raw: &[u8], depth: u32, spans: &mut Vec<Span>, budget: &mut crate::WalkBudget, props: Option<&HashMap<Vec<u8>, u32>>) {
    let mut tm = Mat::ID;
    let mut tlm = Mat::ID;
    let mut leading = 0.0f32;
    let mut size = 0.0f32;
    let mut tc = 0.0f32; // char spacing
    let mut tw = 0.0f32; // word spacing
    let mut ts = 0.0f32; // text rise (Ts): baseline shift in text space — sub/superscripts
    let mut cur: Option<&FontInfo> = None;
    let mut ctm = base; // graphics CTM (q/Q/cm) — needed for rotated/transformed text
    // `q`/`Q` save and restore the WHOLE graphics state, and PDF 32000-1 §9.3 puts the text
    // state parameters (Tc, Tw, TL, Tf/Tfs, Ts — Table 104) in it. Saving only the CTM let a
    // character spacing set once inside a `q` leak over everything drawn after the matching
    // `Q`: `geology_usgs_fs.pdf` sets `Tc 0.047` at operation 10, inside a `q`, and nothing
    // resets it, so every one of the page's 68,529 operations' glyph advances came out
    // 0.047·Tfs too wide. On its map labels — drawn one `Tj` per glyph, then repositioned by
    // an absolute `Td` for the last one — the accumulated drift overshot that `Td`, and the
    // x-sort read "Cloverdale" back as "Cloverdael".
    struct Saved<'a> {
        ctm: Mat,
        size: f32,
        tc: f32,
        tw: f32,
        ts: f32,
        leading: f32,
        font: Option<&'a FontInfo>,
    }
    let mut cstack: Vec<Saved> = Vec::new();
    // The marked-content stack (§14.6): `BDC`/`BMC` push, `EMC` pops, and the innermost
    // entry carrying an `/MCID` is the id every glyph painted here belongs to. Only tracked
    // when `props` says this is the page's own stream. `mc_over` counts pushes refused by
    // the nesting cap so an unbalanced-but-capped stream still pops in step — a stream of
    // 8M unmatched `BDC`s would otherwise grow the stack without bound.
    let mut mcstack: Vec<Option<u32>> = Vec::new();
    let mut mc_over = 0usize;
    let mut mcid: Option<u32> = None;

    for op in ops {
        // Total-work budget (see `crate::WalkBudget`): the depth cap alone lets a
        // self-referential form branch 2x per level. Out of budget → return the spans
        // decoded so far; a page degrades, it never comes back looking blank.
        if !budget.spend(1) {
            return;
        }
        let o = &op.operands;
        // Spans this operator paints directly. A `Do` recursion's spans are excluded
        // deliberately: they belong to the form's own marked-content numbering, not ours.
        let mark = spans.len();
        match op.operator.as_str() {
            "BDC" | "BMC" if props.is_some() => {
                let id = (op.operator == "BDC")
                    .then(|| o.get(1).and_then(|p| mcid_of(doc, p, props.unwrap_or(&HashMap::new()))))
                    .flatten();
                if mcstack.len() < crate::MAX_MARKED_NESTING {
                    mcstack.push(id);
                } else {
                    mc_over += 1;
                }
                mcid = mcstack.iter().rev().find_map(|m| *m);
            }
            "EMC" if props.is_some() => {
                if mc_over > 0 {
                    mc_over -= 1;
                } else {
                    mcstack.pop();
                }
                mcid = mcstack.iter().rev().find_map(|m| *m);
            }
            "q" => cstack.push(Saved { ctm, size, tc, tw, ts, leading, font: cur }),
            "Q" => {
                if let Some(g) = cstack.pop() {
                    ctm = g.ctm;
                    size = g.size;
                    tc = g.tc;
                    tw = g.tw;
                    ts = g.ts;
                    leading = g.leading;
                    cur = g.font;
                }
            }
            "cm" if o.len() >= 6 => {
                let cm = Mat { a: num(&o[0]), b: num(&o[1]), c: num(&o[2]), d: num(&o[3]), e: num(&o[4]), f: num(&o[5]) };
                ctm = cm.mul(ctm);
            }
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
            // Text rise (Ts): baseline shift in text space for sub/superscripts. It
            // persists across BT/ET like the other text-state params and is carried into
            // each word matrix above as a y-offset, so a raised/lowered glyph lands off
            // the line baseline where the HTML layer recognises it as <sup>/<sub>.
            "Ts" if !o.is_empty() => ts = num(&o[0]),
            "T*" => {
                tlm = Mat::translate(0.0, -leading).mul(tlm);
                tm = tlm;
            }
            "Tj" => {
                if let Some(Object::String(s, _)) = o.first() {
                    let style = cur.map(|f| (f.bold, f.italic, f.mono, f.font_id)).unwrap_or((false, false, false, 0));
                    let (words, total) = decode_words(&[Show::Str(s)], cur, size, tc, tw);
                    for wd in words {
                        let wtm = Mat::translate(wd.x_off, ts).mul(tm);
                        push_positioned_span(spans, &wtm, &ctm, size, wd.width, style, wd.text);
                    }
                    tm = Mat::translate(total, 0.0).mul(tm);
                }
            }
            "'" | "\"" => {
                tlm = Mat::translate(0.0, -leading).mul(tlm);
                tm = tlm;
                if let Some(Object::String(s, _)) = o.last() {
                    let style = cur.map(|f| (f.bold, f.italic, f.mono, f.font_id)).unwrap_or((false, false, false, 0));
                    let (words, total) = decode_words(&[Show::Str(s)], cur, size, tc, tw);
                    for wd in words {
                        let wtm = Mat::translate(wd.x_off, ts).mul(tm);
                        push_positioned_span(spans, &wtm, &ctm, size, wd.width, style, wd.text);
                    }
                    tm = Mat::translate(total, 0.0).mul(tm);
                }
            }
            "TJ" => {
                if let Some(Object::Array(arr)) = o.first() {
                    let elems: Vec<Show> = arr
                        .iter()
                        .filter_map(|el| match el {
                            Object::String(s, _) => Some(Show::Str(s)),
                            Object::Integer(n) => Some(Show::Kern(*n as f32)),
                            Object::Real(r) => Some(Show::Kern(*r)),
                            _ => None,
                        })
                        .collect();
                    let style = cur.map(|f| (f.bold, f.italic, f.mono, f.font_id)).unwrap_or((false, false, false, 0));
                    let (words, total) = decode_words(&elems, cur, size, tc, tw);
                    for wd in words {
                        let wtm = Mat::translate(wd.x_off, ts).mul(tm);
                        push_positioned_span(spans, &wtm, &ctm, size, wd.width, style, wd.text);
                    }
                    tm = Mat::translate(total, 0.0).mul(tm);
                }
            }
            // Draw a Form XObject: recurse into its content so body text rendered
            // through a template/overlay form is captured (placed under the form's
            // /Matrix and the CTM in effect at the `Do`). Inline images / non-Form
            // XObjects carry no text and are skipped.
            "Do" => {
                let Some((_, stream)) = xobject_at(doc, xmap, o) else {
                    continue;
                };
                // `OwnOnly`: a form's fonts and XObjects live in its OWN /Resources (PDF
                // 32000-1 §8.10.2 — it inherits the page's, never the invoking form's), and
                // a form without one is skipped rather than decoded through some other
                // scope's encoding. That is a deliberate policy difference from the raster
                // and vector walks, which overlay the parent scope; see `walker::ScopePolicy`.
                let f = match descend_form(doc, stream, xmap, ScopePolicy::OwnOnly, depth, budget, fonts.len()) {
                    Descend::Into(f) => f,
                    Descend::Skip => continue,
                    Descend::Halt => return,
                };
                let ff = match &f.scope.resources {
                    Some(fr) => build_fonts_from_resources(doc, fr, raw),
                    None => continue, // unreachable under OwnOnly, which refuses a form without /Resources
                };
                let sub = f.matrix.mul(ctm);
                // §8.10.2: the form's `/BBox` clips its content, glyphs included. The raster
                // and vector walks already read the key (`walker::form_bbox_clip`); this walk
                // did not, so a producer that reuses ONE oversized form body and selects a
                // band of it per placement got every glyph of the whole body, once per
                // placement. See `clip_spans_to`.
                let clip = crate::walker::form_bbox_clip(doc, stream, sub);
                let mark = spans.len();
                decode_spans(doc, &f.ops, &ff, &f.scope.xobjects, sub, raw, depth + 1, spans, budget, None);
                if let Some(bb) = clip {
                    clip_spans_to(spans, mark, bb);
                }
                continue; // the form's spans carry the form's numbering, never the page's
            }
            _ => {}
        }
        if let Some(m) = mcid {
            spans[mark..].iter_mut().for_each(|s| s.mcid = Some(m));
        }
    }
}

/// The `/MCID` a `BDC`'s property operand names — written inline as a dictionary, or as a
/// name into the stream's `/Resources /Properties`.
fn mcid_of(doc: &Document, prop: &Object, props: &HashMap<Vec<u8>, u32>) -> Option<u32> {
    match prop {
        Object::Name(n) => props.get(n.as_slice()).copied(),
        _ => {
            let d = crate::pdfobj::deref(doc, prop)?.as_dict().ok()?;
            u32::try_from(crate::pdfobj::deref(doc, d.get(b"MCID").ok()?)?.as_i64().ok()?).ok()
        }
    }
}

/// The page's own `/Resources /Properties` as name → `/MCID`, for the `/Tag /P0 BDC`
/// spelling. Page-level only, and deliberately so: a form's `/Properties` names a sequence in
/// the form's numbering, which this walk does not stamp.
fn marked_properties(doc: &Document, page_id: ObjectId) -> HashMap<Vec<u8>, u32> {
    let mut m = HashMap::new();
    let Ok((own, inherited)) = doc.get_page_resources(page_id) else { return m };
    for res in own.into_iter().chain(inherited.into_iter().filter_map(|id| doc.get_dictionary(id).ok())) {
        let Some(props) = crate::pdfobj::sub_dict(doc, res, b"Properties") else { continue };
        for (k, v) in props.iter() {
            if let Some(id) = crate::pdfobj::deref(doc, v)
                .and_then(|o| o.as_dict().ok())
                .and_then(|d| d.get(b"MCID").ok())
                .and_then(|o| crate::pdfobj::deref(doc, o))
                .and_then(|o| o.as_i64().ok())
                .and_then(|v| u32::try_from(v).ok())
            {
                m.insert(k.to_vec(), id);
            }
        }
    }
    m
}

/// Drop the spans a descended form's `/BBox` crops away — everything from index `from`
/// onward whose glyph box does not overlap the box, which is already in page space.
///
/// PDF 32000-1 §8.10.2 makes `/BBox` a clip on the form's content, so a glyph outside it
/// is not painted and is not text on the page. A LaTeX/pdftex figure inclusion leans on
/// exactly that: `med_mrna_vaccine_immunology_pmc.pdf` p13 embeds ONE 346 KB form body
/// twice — `/Im17` with `/BBox [10.5 473.2 582.3 643.9]` and `/Im18` with
/// `/BBox [6.3 63.7 577.6 474.1]`, two disjoint horizontal bands of the same figure —
/// and places each so its band lands where it belongs. Reading both bodies whole
/// emitted every label twice, 3.13 pt apart in x and 1.61 pt in y, which is a real
/// offset and therefore invisible to `dedup_coincident`'s integer-rounded key.
///
/// The test is *overlap*, not containment: a glyph the box cuts through is partly
/// painted, and a producer whose `/BBox` hugs its content must not lose its edge text.
/// That is the lenient reading — it can only ever keep more than a conforming reader
/// paints, never less.
fn clip_spans_to(spans: &mut Vec<Span>, from: usize, bb: crate::geom::Rect) {
    if from >= spans.len() {
        return;
    }
    let tail: Vec<Span> = spans
        .split_off(from)
        .into_iter()
        .filter(|s| {
            let (x0, x1, y0, y1) = span_bbox(s);
            let hit = crate::geom::Rect::new(x0, y0, x1, y1).intersect(bb);
            hit.width() >= 0.0 && hit.height() >= 0.0
        })
        .collect();
    spans.extend(tail);
}

/// Drop spans that coincide with one already emitted — same text at the same
/// position (to the nearest point). Some generators draw each glyph run twice at
/// the same spot (faux-bold / a duplicated content block); a viewer overprints
/// them into one, but extraction sees both, doubling every token. This is a
/// general structural de-duplication (what mature extractors do), not a per-doc
/// patch: two *distinct* words can never share an identical baseline position.
fn dedup_coincident(spans: &mut Vec<Span>) {
    let mut seen = std::collections::HashSet::new();
    spans.retain(|s| seen.insert((s.x.round() as i32, s.y.round() as i32, s.text.clone())));
}

/// Number the visible paint occurrences in retained content-walk order.
///
/// This deliberately runs after [`dedup_coincident`]. Form invocations are decoded once per
/// placement and therefore receive different ids; only occurrences that a viewer overprints
/// at the same rounded position have already collapsed to one.
fn assign_occurrence_provenance(spans: &mut [Span]) {
    for (i, span) in spans.iter_mut().enumerate() {
        let source = SourceId(u32::try_from(i).expect("one page cannot contain more than u32 spans"));
        span.source = SourceSlice::whole(source, span.text.chars().count());
    }
}

/// Effective span width (fall back to a char estimate if widths were absent).
fn span_width(s: &Span) -> f32 {
    if s.width > 0.1 {
        s.width
    } else {
        s.text.chars().count() as f32 * s.size * 0.5
    }
}

/// Bounding box of a span in PDF user space (y increases upward):
/// `(left, right, bottom, top)`.
fn span_bbox(s: &Span) -> (f32, f32, f32, f32) {
    (s.x, s.x + span_width(s), s.y, s.y + s.size.max(1.0))
}

fn fcmp(a: f32, b: f32) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Median font size across spans (used to scale XY-cut gap thresholds so they
/// adapt to the document's body text size rather than absolute points).
fn median_size(spans: &[Span]) -> f32 {
    let mut sizes: Vec<f32> = spans.iter().map(|s| s.size).collect();
    sizes.sort_by(|a, b| fcmp(*a, *b));
    sizes.get(sizes.len() / 2).copied().unwrap_or(10.0).max(1.0)
}

/// Widest empty gap in the union of 1-D intervals `[lo, hi]`. Sweeps the sorted
/// intervals tracking coverage; an interval whose `lo` exceeds the running max
/// `hi` exposes a gap. Returns `(gap_width, cut_coordinate)` for the widest
/// interior gap, or `None` when the projection is fully covered.
fn widest_gap(mut intervals: Vec<(f32, f32)>) -> Option<(f32, f32)> {
    if intervals.len() < 2 {
        return None;
    }
    intervals.sort_by(|a, b| fcmp(a.0, b.0));
    let mut max_hi = intervals[0].1;
    let mut best: Option<(f32, f32)> = None;
    for &(lo, hi) in intervals.iter().skip(1) {
        if lo > max_hi {
            let g = lo - max_hi;
            if best.is_none_or(|(bg, _)| g > bg) {
                best = Some((g, (max_hi + lo) * 0.5));
            }
        }
        if hi > max_hi {
            max_hi = hi;
        }
    }
    best
}

/// Widest INTERIOR vertical lane crossed by at most `tol` intervals — a crossing-tolerant
/// gutter detector. Unlike [`widest_gap`] (which needs a fully empty lane) this ignores a
/// handful of stray boxes (a centered page number, a running header) that would otherwise
/// veto an obvious two-column split. Returns `(lane_width, centre_x)`. The caller still
/// validates the split with `vertical_valid`, so a false lane in sparse text is rejected.
fn tolerant_vgap(intervals: &[(f32, f32)], tol: usize) -> Option<(f32, f32)> {
    let xmin = intervals.iter().map(|v| v.0).fold(f32::INFINITY, f32::min);
    let xmax = intervals.iter().map(|v| v.1).fold(f32::NEG_INFINITY, f32::max);
    if !(xmax - xmin > 2.0) {
        return None;
    }
    let crossings = |x: f32| intervals.iter().filter(|(lo, hi)| *lo < x && x < *hi).count();
    // Sweep the interior at 1pt resolution, tracking the widest contiguous run of x whose
    // crossing count stays within tolerance.
    let step = 1.0f32;
    let (mut best_w, mut best_c) = (0.0f32, 0.0f32);
    let (mut run_start, mut in_run) = (0.0f32, false);
    let mut x = xmin + step;
    while x < xmax {
        let clear = crossings(x) <= tol;
        if clear && !in_run {
            run_start = x;
            in_run = true;
        }
        if in_run && (!clear || x + step >= xmax) {
            let end = if clear { x } else { x - step };
            let w = end - run_start;
            if w > best_w {
                best_w = w;
                best_c = (run_start + end) * 0.5;
            }
            in_run = false;
        }
        x += step;
    }
    (best_w > 0.0).then_some((best_w, best_c))
}

/// A bounding box in PDF user space (y increases upward): `(left, right, bottom, top)`.
pub type BBox = (f32, f32, f32, f32);

/// Order arbitrary positioned boxes (spans, lines, table/image blocks) into
/// human reading order via a recursive XY-cut. `med` is the document's body text
/// size, used to scale the gutter thresholds. Returns indices into `boxes`.
///
/// This is the shared reading-order primitive: the text extractor feeds it span
/// boxes, the HTML layer feeds it block boxes, so both segment columns the same
/// way. See [`xy_cut`] for the algorithm.
pub fn xy_cut_order(boxes: &[BBox], med: f32) -> Vec<usize> {
    xy_cut_order_opt(boxes, med, false)
}

/// As [`xy_cut_order`], but `tolerant` enables the crossing-tolerant column-gutter detector
/// (a stray centered page number / running header in the gutter no longer vetoes a real
/// column split). Use it for SPAN-level prose ordering (where line interleaving hurts); keep
/// it OFF for page-item ordering, where over-splitting would detach figures from captions.
pub fn xy_cut_order_opt(boxes: &[BBox], med: f32, tolerant: bool) -> Vec<usize> {
    let mut order = Vec::with_capacity(boxes.len());
    xy_cut(boxes, (0..boxes.len()).collect(), med.max(1.0), 0, tolerant, &mut order);
    order
}

/// How deep [`xy_cut`] may recurse before it stops splitting and emits the boxes in line
/// order. A page-layout guard, **not** [`crate::MAX_FORM_DEPTH`]: the two govern unrelated
/// recursions (whitespace-gutter subdivision vs. Form-XObject nesting) and share the value
/// 40 by coincidence. Aliasing them would tie a layout heuristic to a DoS cap, so they stay
/// separate constants — the structure test that forbids re-declared caps exempts this one by
/// name for exactly that reason.
const MAX_XY_CUT_DEPTH: u32 = 40;

/// Recursive XY-cut: order box indices into human reading order by repeatedly
/// splitting on the widest whitespace gutter.
///
/// A *vertical* gutter (an empty column of whitespace running top-to-bottom)
/// separates page columns — the left block is read in full before the right.
/// A *horizontal* gutter separates stacked blocks (title vs. body, paragraph
/// breaks) — top read before bottom. This is what stops 2-column papers from
/// interleaving left-line/right-line, the dominant arXiv reading-order defect.
///
/// Vertical cuts are preferred when they clear the (larger) column threshold and
/// are at least as wide as the best horizontal gap, because column order matters
/// most. A full-width element (title/abstract/figure) fills the gutter and so
/// has no vertical gap there — it gets peeled off by a horizontal cut first,
/// after which the remaining body splits cleanly into columns.
fn xy_cut(boxes: &[BBox], idx: Vec<usize>, med: f32, depth: u32, tolerant: bool, out: &mut Vec<usize>) {
    if idx.len() <= 1 || depth >= MAX_XY_CUT_DEPTH {
        out.extend(sorted_lines(boxes, idx, med));
        return;
    }
    // A single column's x-projection has no interior gap (text fills the column
    // width across lines), so the threshold can sit just above a wide word space;
    // false gutters from sparse/centered layouts are rejected by `vertical_valid`.
    // Column gutter: a real gutter is far wider than a word space (~0.3·em) but can be
    // narrower than 1.2·em on tightly-set two-column pages (BERT p13's is ~1.06·em). Use
    // 1.0·em — still well above any word space — and rely on `vertical_valid` (overlap +
    // per-side box share) to reject false gutters from sparse/centered single-column text.
    let thr_v = (med * 1.0).max(7.0); // column gutter
    let thr_h = (med * 1.2).max(6.0); // block gap: must exceed normal line leading

    // The column-gutter projection ignores degenerate boxes: near-zero-height
    // spans are rotated-text artifacts (e.g. the arXiv sidebar id, whose advance
    // lands as a page-wide horizontal box) that would otherwise smear across the
    // gutter and defeat every column cut. They still take part in the ordering.
    let min_h = (med * 0.4).max(2.0);
    let vint: Vec<(f32, f32)> = idx
        .iter()
        .filter(|&&i| boxes[i].3 - boxes[i].2 >= min_h)
        .map(|&i| (boxes[i].0, boxes[i].1))
        .collect();
    let gv = widest_gap(vint.clone()); // strictly-empty column gutter
    let gh = widest_gap(idx.iter().map(|&i| (boxes[i].2, boxes[i].3)).collect());
    let gvw = gv.map(|(g, _)| g).unwrap_or(0.0);
    let ghw = gh.map(|(g, _)| g).unwrap_or(0.0);

    // Attempt a vertical (column) cut at `cut`: split left|right and recurse, left first.
    // Returns `Some(idx)` (the boxes handed back) when `cut` is not a real column boundary.
    let try_vertical = |out: &mut Vec<usize>, idx: Vec<usize>, cut: f32| -> Option<Vec<usize>> {
        if !vertical_valid(boxes, &idx, cut) {
            return Some(idx);
        }
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for i in idx {
            let b = boxes[i];
            if (b.0 + b.1) * 0.5 < cut { left.push(i) } else { right.push(i) }
        }
        xy_cut(boxes, left, med, depth + 1, tolerant, out);
        xy_cut(boxes, right, med, depth + 1, tolerant, out);
        None
    };

    let mut idx = idx;
    // Prefer a vertical cut when its gutter is at least as wide as the best
    // horizontal gap (columns dominate reading order).
    if gvw >= thr_v && gvw >= ghw {
        if let Some((_, c)) = gv {
            match try_vertical(out, idx, c) {
                None => return,
                Some(back) => idx = back, // invalid gutter; reconsider below
            }
        }
    }
    // Peel a full-width block (title/abstract/figure/caption) horizontally, top first; the
    // body underneath becomes cleanly two-column on recursion. This runs BEFORE the tolerant
    // gutter below, so a full-width caption is never split into columns.
    if ghw >= thr_h {
        let cut = gh.unwrap().1;
        let (mut top, mut bot) = (Vec::new(), Vec::new());
        for i in idx {
            let b = boxes[i];
            if (b.2 + b.3) * 0.5 >= cut {
                top.push(i) // higher y = visually higher = read first
            } else {
                bot.push(i)
            }
        }
        xy_cut(boxes, top, med, depth + 1, tolerant, out);
        xy_cut(boxes, bot, med, depth + 1, tolerant, out);
        return;
    }
    // No horizontal block break: a narrower-but-valid strictly-empty gutter still wins over
    // interleaving the columns line-by-line.
    if gvw >= thr_v {
        if let Some((_, c)) = gv {
            match try_vertical(out, idx, c) {
                None => return,
                Some(back) => idx = back,
            }
        }
    }
    // LAST RESORT (span ordering only): a TALL two-column body whose gutter is split only by a
    // stray centered page number / running header. Gated hard — tried only after the
    // full-width horizontal peel (figures/captions already blocked off), tolerating very few
    // crossings, requiring the lane to clear the column threshold AND both sides to span many
    // lines, so a short multi-line caption is never mistaken for a pair of columns.
    if tolerant && vint.len() >= 8 {
        let tol = (vint.len() / 120).max(1);
        if let Some((w, c)) = tolerant_vgap(&vint, tol) {
            if w >= thr_v && tall_columns(boxes, &idx, c, med) {
                match try_vertical(out, idx, c) {
                    None => return,
                    Some(back) => idx = back,
                }
            }
        }
    }
    out.extend(sorted_lines(boxes, idx, med));
}

/// Both sides of `cut` span a tall, overlapping y-range (≥ ~8 lines) — the signature of true
/// page columns, not a short multi-line caption that merely has words on either side of a
/// perceived lane. Used to gate the crossing-tolerant gutter.
fn tall_columns(boxes: &[BBox], idx: &[usize], cut: f32, med: f32) -> bool {
    let (mut lt, mut lb, mut rt, mut rb) = (f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY);
    for &i in idx {
        let b = boxes[i];
        if (b.0 + b.1) * 0.5 < cut {
            lt = lt.max(b.3);
            lb = lb.min(b.2);
        } else {
            rt = rt.max(b.3);
            rb = rb.min(b.2);
        }
    }
    let overlap = (lt.min(rt) - lb.max(rb)).max(0.0);
    overlap >= med * 8.0
}

/// Is a candidate vertical cut a real column boundary (not a sparse-layout or
/// map-label artifact)? Requires both sides to carry a meaningful share of boxes
/// and to overlap vertically — true side-by-side columns coexist over a shared
/// y-range, whereas centered titles and scattered labels do not.
fn vertical_valid(boxes: &[BBox], idx: &[usize], cut: f32) -> bool {
    let n = idx.len();
    let (mut lc, mut rc) = (0usize, 0usize);
    let (mut lt, mut lb) = (f32::NEG_INFINITY, f32::INFINITY);
    let (mut rt, mut rb) = (f32::NEG_INFINITY, f32::INFINITY);
    for &i in idx {
        let b = boxes[i];
        if (b.0 + b.1) * 0.5 < cut {
            lc += 1;
            lt = lt.max(b.3);
            lb = lb.min(b.2);
        } else {
            rc += 1;
            rt = rt.max(b.3);
            rb = rb.min(b.2);
        }
    }
    if lc < 3 || rc < 3 || lc * 10 < n || rc * 10 < n {
        return false; // a tiny sliver on one side is not a column
    }
    let overlap = (lt.min(rt) - lb.max(rb)).max(0.0);
    let min_height = (lt - lb).min(rt - rb).max(1.0);
    overlap >= 0.5 * min_height
}

/// Base-case ordering for a block with no further gutter: top-to-bottom by line
/// band, left-to-right within a line.
fn sorted_lines(boxes: &[BBox], mut idx: Vec<usize>, med: f32) -> Vec<usize> {
    let band = (med * 0.6).max(2.0);
    idx.sort_by(|&p, &q| {
        let (bp, bq) = ((boxes[p].2 / band).round(), (boxes[q].2 / band).round());
        fcmp(bq, bp).then(fcmp(boxes[p].0, boxes[q].0))
    });
    idx
}

/// Reconstruct reading-order text from positioned spans. Spans are first ordered
/// by a recursive XY-cut (column/block segmentation), then joined: a newline on a
/// line-band change, a space only on a real intra-line gap (never duplicating one
/// already carried by the span text).
fn text_from_spans(mut spans: Vec<Span>) -> String {
    spans.retain(|s| !s.text.is_empty());
    if spans.is_empty() {
        return String::new();
    }
    let med = median_size(&spans);
    let band = (med * 0.6).max(2.0);
    let boxes: Vec<BBox> = spans.iter().map(span_bbox).collect();
    let order = xy_cut_order(&boxes, med);

    let mut out = String::new();
    let mut last_band: Option<f32> = None;
    let mut prev_end = 0.0f32;
    for &i in &order {
        let s = &spans[i];
        let b = (s.y / band).round();
        let new_line = match last_band {
            Some(lb) => (lb - b).abs() >= 0.5,
            None => false,
        };
        if new_line {
            out.push('\n');
        } else if last_band.is_some() {
            let gap = s.x - prev_end;
            // Spans are word-level, so a gap at/above a typical space width (≈0.25em;
            // some fonts pack to ~0.28em) is a word boundary. 0.2em clears those while
            // staying below intra-word kerning (≈0).
            if gap > s.size * crate::textutil::SPACE_GAP && !out.ends_with(' ') && !s.text.starts_with(' ') {
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
                if let Ok(st) = doc.get_object(r).and_then(|o| o.as_stream().cloned()) {
                    let dec = st.decompressed_content();
                    let raw_len = st.content.len();
                    let dec_len = dec.as_ref().map(|d| d.len() as i64).unwrap_or(-1);
                    let used = crate::pdfobj::content_bytes(&st);
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
    // Stream integrity for this page's OWN content streams. A truncated Flate stream is
    // reported `Ok` by lopdf and renders short with no error at all, so the one page-level
    // diagnostic in the crate is where "the ops below are all there were" has to be said.
    for issue in crate::pdfobj::page_stream_issues(doc, page_id) {
        s += &format!("stream {:?}: {} ({}), {} bytes recovered\n", issue.object, issue.kind, issue.filter, issue.recovered);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An adversarial fixture (`tests/gen_fixtures.py::gen_form_bomb`), its bytes and page.
    fn adversarial(name: &str) -> (Vec<u8>, Document, ObjectId) {
        let path = format!("{}/../tests/fixtures_pdf/adversarial/{name}", env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{name} fixture must exist: {e}"));
        let doc = Document::load_mem(&raw).unwrap_or_else(|e| panic!("{name} fixture must load: {e}"));
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        (raw, doc, page_id)
    }

    #[test]
    fn a_self_referential_form_cannot_hang_the_text_walk() {
        // `form_bomb.pdf`: form /X invokes /X twice, so the walk branches 2x per level and
        // `MAX_FORM_DEPTH` alone allowed ~2^40 descents. This call never returned.
        let (raw, doc, page_id) = adversarial("form_bomb.pdf");
        let t = std::time::Instant::now();
        let spans = extract_spans(&doc, page_id, &raw);
        assert!(t.elapsed().as_secs() < 10, "form bomb ran for {:?} — the budget is not bounding it", t.elapsed());
        assert!(spans.is_empty(), "the bomb shows no text, so none may be invented for it");
    }

    #[test]
    fn a_form_drawn_three_times_yields_three_text_runs() {
        // The control, and the reason this fix is a BUDGET and not a visited set: one form
        // showing one word, invoked at three offsets, is three real runs on the page. An
        // `ObjectId` dedupe would return 1 and silently drop two thirds of the text.
        let (raw, doc, page_id) = adversarial("form_repeat.pdf");
        let spans = extract_spans(&doc, page_id, &raw);
        let hits: Vec<&Span> = spans.iter().filter(|s| s.text.contains("REPEAT")).collect();
        let seen: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(hits.len(), 3, "a repeated form must show its text once per invocation, got {seen:?}");
        let mut ys: Vec<i32> = hits.iter().map(|s| s.y.round() as i32).collect();
        ys.sort_unstable();
        ys.dedup();
        assert_eq!(ys.len(), 3, "the three occurrences must land at three offsets, got {ys:?}");
        let mut source_ids: Vec<u32> = hits.iter().map(|s| s.source.source().ordinal()).collect();
        source_ids.sort_unstable();
        source_ids.dedup();
        assert_eq!(source_ids.len(), 3, "each painted form invocation needs its own source id");
        assert!(hits.iter().all(|s| {
            s.source.char_start() == 0
                && s.source.char_end() as usize == s.text.chars().count()
        }));
    }

    #[test]
    fn occurrence_ids_are_assigned_after_visible_coincident_deduplication() {
        let span = |x: f32| Span {
            x,
            y: 20.0,
            size: 10.0,
            width: 20.0,
            text: "same".into(),
            bold: false,
            italic: false,
            mono: false,
            angle: 0.0,
            font: 0,
            mcid: None,
            source: SourceSlice::unassigned(),
        };
        let mut spans = vec![span(10.0), span(10.0), span(40.0)];

        dedup_coincident(&mut spans);
        assign_occurrence_provenance(&mut spans);

        assert_eq!(spans.len(), 2, "coincident overpaint is one visible occurrence");
        assert_eq!(
            spans.iter().map(|s| s.source.source().ordinal()).collect::<Vec<_>>(),
            vec![0, 1],
            "surviving paint sites are numbered in retained content order"
        );
        assert!(spans.iter().all(|s| (s.source.char_start(), s.source.char_end()) == (0, 4)));
    }

    #[test]
    fn an_exhausted_work_budget_degrades_a_repeated_form_instead_of_emptying_it() {
        // Degrade, don't vanish: a walk that runs out mid-page keeps the spans it decoded.
        let (raw, doc, page_id) = adversarial("form_repeat.pdf");
        let content = doc.get_and_decode_page_content(page_id).expect("fixture page has content");
        let fonts = build_fonts(&doc, page_id, &raw);
        let xmap = crate::walker::page_xobjects(&doc, page_id);
        let mut spans = Vec::new();
        let mut budget = crate::WalkBudget::new(700);
        decode_spans(&doc, &content.operations, &fonts, &xmap, Mat::ID, &raw, 0, &mut spans, &mut budget, None);
        assert!(!spans.is_empty(), "a tripped budget must not empty the page");
        assert!(spans.len() < 3, "the budget must really bite, got {} spans", spans.len());
    }

    /// The identity-ToUnicode gap: a Type0 font whose CIDs are Unicode code points, with a
    /// machine-generated identity table that stops short of two blocks. The six symbols in
    /// those gaps used to come out as spaces — the glyph is .notdef in the embedded font, so
    /// the CID is the only place the character survives.
    #[test]
    fn identity_tounicode_gaps_keep_the_symbol_instead_of_a_space() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/identity_cid_font.pdf");
        let raw = std::fs::read(path).unwrap();
        let doc = Document::load_mem(&raw).unwrap();
        let pid = *doc.get_pages().get(&1).unwrap();
        let out = extract_page(&doc, pid, &raw).unwrap();
        // The six the table omits (this is the defect) …
        for sym in ["\u{2B1F}", "\u{2B22}", "\u{2B21}", "\u{27F6}", "\u{27F5}", "\u{27F7}"] {
            assert!(out.contains(sym), "uncovered symbol {sym:?} lost from {out:?}");
        }
        // … and their same-line, same-font neighbours the table does cover.
        for sym in ["\u{2220}", "\u{221F}", "\u{22A5}", "\u{2225}", "\u{25B3}", "\u{25A1}",
                    "\u{25CB}", "\u{21CC}", "\u{21C4}", "\u{2191}", "\u{2193}"] {
            assert!(out.contains(sym), "covered symbol {sym:?} lost from {out:?}");
        }
        assert!(out.contains("Geometry: \u{2220} \u{221F} \u{22A5} \u{2225} \u{25B3} \u{25A1} \u{25CB} \u{2B1F} \u{2B22} \u{2B21}"),
                "line 1 not whole: {out:?}");
        assert!(out.contains("Chemistry: \u{21CC} \u{21C4} \u{2191} \u{2193} \u{27F6} \u{27F5} \u{27F7}"),
                "line 2 not whole: {out:?}");
    }

    /// The guard on the above: the same fixture's SUBSET font shows CID 0x41, which its
    /// (non-identity) table does not map. Falling back to the code there would invent an "A"
    /// out of a glyph index — the reason the fallback is gated on an identity table.
    #[test]
    fn a_subset_cid_font_never_invents_text_from_a_glyph_index() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/identity_cid_font.pdf");
        let raw = std::fs::read(path).unwrap();
        let doc = Document::load_mem(&raw).unwrap();
        let pid = *doc.get_pages().get(&1).unwrap();
        let out = extract_page(&doc, pid, &raw).unwrap();
        assert!(out.contains("Hello"), "subset font text lost: {out:?}");
        assert!(!out.contains("HelloA"), "unmapped CID 0x41 invented an 'A': {out:?}");
        assert!(!out.contains('A'), "no 'A' is drawn anywhere on this page: {out:?}");
    }

    /// `tests/gen_fixtures.py::gen_partial_tounicode`, page by page.
    fn partial_tounicode_page(n: u32) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/partial_tounicode.pdf");
        let raw = std::fs::read(path).unwrap();
        let doc = Document::load_mem(&raw).unwrap();
        let pid = *doc.get_pages().get(&n).unwrap();
        extract_page(&doc, pid, &raw).unwrap()
    }

    /// An *incomplete* ToUnicode (the common case — one `bfchar` per subsetted glyph) left
    /// every omitted code decoding to nothing, so letters vanished mid-word. The declared
    /// `/Encoding` is where that information was, unused.
    #[test]
    fn a_partial_tounicode_falls_back_to_the_declared_encoding() {
        let out = partial_tounicode_page(1);
        for word in ["Redding", "Sacramento", "EXPLANATION", "KILOMETERS", "42\u{b0}"] {
            assert!(out.contains(word), "word lost to a ToUnicode hole: {word:?} not in {out:?}");
        }
    }

    /// The guard, and the one that matters more: a symbolic font with NO `/Encoding` is read
    /// through its font program's built-in cmap, whose codes a subsetter re-packs. A Latin
    /// fallback there prints `a` for θ and `^` for π — confident nonsense that no downstream
    /// consumer can undo, unlike a gap.
    #[test]
    fn a_symbolic_font_with_no_encoding_never_substitutes_latin() {
        let out = partial_tounicode_page(2);
        for sym in ["\u{2206}", "\u{2248}"] {
            assert!(out.contains(sym), "a mapped symbol was lost: {sym:?} from {out:?}");
        }
        assert!(!out.contains('a'), "unmapped code 0x61 invented a WinAnsi 'a': {out:?}");
        assert!(!out.contains('^'), "unmapped code 0x5e invented a WinAnsi '^': {out:?}");
        assert!(!out.chars().any(|c| c.is_alphabetic()), "Latin substituted for a symbol font: {out:?}");
    }

    /// `/MacRomanEncoding` with no ToUnicode: the high bytes are MacRoman, not Latin-1.
    /// Read raw they came out `¥ É Ð Õ` — the mojibake that littered a real paper's quotes.
    #[test]
    fn a_macroman_font_with_no_tounicode_is_not_read_as_latin1() {
        let out = partial_tounicode_page(3);
        assert!(out.contains("\u{2022}\u{2026}\u{2013}\u{2019}"), "MacRoman mis-decoded: {out:?}");
        for bad in ['\u{a5}', '\u{c9}', '\u{d0}', '\u{d5}'] {
            assert!(!out.contains(bad), "MacRoman byte read as Latin-1 {bad:?}: {out:?}");
        }
    }

    #[test]
    fn a_forms_indirect_matrix_places_its_text_where_the_page_puts_it() {
        // `tests/gen_fixtures.py::gen_form_inherit`. The form's `/Matrix` is written as an
        // indirect reference; read directly (`as_array()` on a `Reference` fails) it
        // degraded to the identity, so every glyph inside the form landed 100 pt to the
        // LEFT of its authored position — text that is present but in the wrong column,
        // which reading order and figure attachment both then get wrong.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_inherit.pdf");
        let raw = std::fs::read(path).expect("form_inherit.pdf fixture must exist");
        let doc = Document::load_mem(&raw).expect("form_inherit.pdf fixture must load");
        let pid = *doc.get_pages().get(&1).expect("fixture has page 1");
        let spans = extract_spans(&doc, pid, &raw);
        let s = spans.iter().find(|s| s.text.contains("INHERIT")).expect("the form's label");
        assert!((s.x - 172.0).abs() < 1.0, "x {} (72 means the indirect /Matrix was lost)", s.x);
    }

    #[test]
    fn a_forms_bbox_clips_the_glyphs_it_crops_away() {
        // `tests/gen_fixtures.py::gen_form_bbox_text`. One form body holding two bands of
        // text, invoked twice through two disjoint `/BBox`es — the pdftex figure-inclusion
        // shape that `med_mrna_vaccine_immunology_pmc.pdf` p13 uses for real. §8.10.2 makes
        // `/BBox` a clip on the form's content; this walk did not read it, so it decoded
        // both bodies whole and every label came out twice, at the offset between the two
        // placements. That offset is real (3 pt here, 3.13 pt in the corpus file), so
        // `dedup_coincident`'s integer-rounded key cannot see it.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_bbox_text.pdf");
        let raw = std::fs::read(path).expect("form_bbox_text.pdf fixture must exist");
        let doc = Document::load_mem(&raw).expect("form_bbox_text.pdf fixture must load");
        let pid = *doc.get_pages().get(&1).expect("fixture has page 1");
        let spans = extract_spans(&doc, pid, &raw);
        let hits = |w: &str| spans.iter().filter(|s| s.text.contains(w)).count();
        assert_eq!(hits("TOPBAND"), 1, "the top band paints once, not once per placement");
        assert_eq!(hits("BOTBAND"), 1, "the bottom band paints once, not once per placement");
        // Kept, not merely deduped: each survivor must be the copy the form whose `/BBox`
        // contains it placed — the top one at (72+20, 400+300), the bottom at (75+20, 321.6+50).
        let top = spans.iter().find(|s| s.text.contains("TOPBAND")).unwrap();
        assert!((top.x - 92.0).abs() < 1.0 && (top.y - 700.0).abs() < 1.0, "TOPBAND at ({}, {})", top.x, top.y);
        let bot = spans.iter().find(|s| s.text.contains("BOTBAND")).unwrap();
        assert!((bot.x - 95.0).abs() < 1.0 && (bot.y - 371.6).abs() < 1.0, "BOTBAND at ({}, {})", bot.x, bot.y);
        // The guard: the clip must not eat the page's own text, which no `/BBox` governs.
        let out = extract_page(&doc, pid, &raw).expect("page text");
        assert!(out.contains("One body, two bands"), "page-level text lost to a form clip: {out:?}");
    }

    #[test]
    fn a_form_stream_with_no_filter_still_shows_its_text() {
        // `unfiltered_form.pdf` (`gen_fixtures.py::gen_unfiltered_form`): a label drawn inside
        // a Form XObject whose stream carries NO /Filter. Through lopdf 0.43
        // `decompressed_content()` *errored* for such a stream, so `.unwrap_or_default()` fed
        // the decoder zero bytes and every glyph inside the form disappeared — silently, and
        // only on this walker and the vector one (extract/img carry the raw-bytes fallback).
        // lopdf 0.44 returns the raw content instead; the fallback keeps us correct either way.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/unfiltered_form.pdf");
        let raw = std::fs::read(path).expect("unfiltered_form.pdf fixture must exist");
        let doc = Document::load_mem(&raw).expect("unfiltered_form.pdf fixture must load");
        let pid = *doc.get_pages().get(&1).expect("fixture has page 1");
        // The premise, asserted rather than assumed: the form really is unfiltered.
        let form_id = crate::walker::page_xobjects(&doc, pid).get(b"UF".as_slice()).copied().expect("/UF form");
        let form = doc.get_object(form_id).unwrap().as_stream().unwrap();
        assert!(form.dict.get(b"Filter").is_err(), "the fixture's form must carry no /Filter");
        assert_eq!(
            form.decompressed_content().ok().as_deref(),
            Some(&form.content[..]),
            "the premise, lopdf 0.44: an unfiltered stream decodes to its raw content",
        );

        let out = extract_page(&doc, pid, &raw).expect("page text");
        assert!(out.contains("Unfiltered form ink"), "the form's own label is lost: {out:?}");
        // The page-level text was never at risk — its presence proves the page decoded and
        // only the form descent was dropping content.
        assert!(out.contains("A Form Stream With No Filter"), "page-level text lost: {out:?}");
    }

    #[test]
    fn is_identity_cmap_needs_a_big_and_near_total_identity_table() {
        let ident = |lo: u32, n: u32| -> HashMap<u32, String> {
            (lo..lo + n).map(|c| (c, char::from_u32(c).unwrap().to_string())).collect()
        };
        // A real machine-generated identity table.
        assert!(is_identity_cmap(&ident(0x20, 1000)));
        // Too small to prove anything — a subset font can be accidentally identity.
        assert!(!is_identity_cmap(&ident(0x41, 26)));
        // Big, but the CIDs are glyph indices: not identity.
        let subset: HashMap<u32, String> =
            (1..1000u32).map(|g| (g, char::from_u32(0x400 + g).unwrap().to_string())).collect();
        assert!(!is_identity_cmap(&subset));
        // A few explicit normalisations inside an otherwise identity table are still identity.
        let mut mostly = ident(0x20, 1000);
        for c in 0x20..0x40u32 {
            mostly.insert(c, "?".to_string());
        }
        assert!(is_identity_cmap(&mostly));
    }

    #[test]
    fn widest_gap_finds_interior_lane() {
        // two clusters of intervals with a clear gap 10..40 between them
        let g = widest_gap(vec![(0.0, 10.0), (2.0, 8.0), (40.0, 50.0), (42.0, 48.0)]).unwrap();
        assert!((g.0 - 30.0).abs() < 0.01, "gap width");
        assert!((g.1 - 25.0).abs() < 0.01, "cut at midpoint");
    }

    #[test]
    fn tolerant_vgap_ignores_a_stray_crosser() {
        // a clean lane 20..40 except ONE interval (a centered page number) crosses x=30;
        // strict widest_gap is split, but tolerant (tol=1) recovers the ~20pt lane.
        let iv = vec![(0.0, 20.0), (0.0, 19.0), (40.0, 60.0), (41.0, 59.0), (29.0, 31.0)];
        assert!(widest_gap(iv.clone()).map(|(w, _)| w).unwrap_or(0.0) < 12.0, "strict lane is split");
        let (w, c) = tolerant_vgap(&iv, 1).unwrap();
        assert!(w >= 15.0, "tolerant recovers the wide lane, got {w}");
        assert!(c > 20.0 && c < 40.0, "cut inside the gutter");
    }

    #[test]
    fn xy_cut_two_columns_then_single() {
        // boxes: (left,right,bottom,top), y up. Two tall columns: L rows then R rows.
        let body = 10.0;
        let boxes: Vec<BBox> = vec![
            (0.0, 40.0, 90.0, 100.0),  // L top
            (0.0, 40.0, 60.0, 70.0),   // L mid
            (0.0, 40.0, 30.0, 40.0),   // L bot
            (60.0, 100.0, 90.0, 100.0),// R top
            (60.0, 100.0, 60.0, 70.0), // R mid
            (60.0, 100.0, 30.0, 40.0), // R bot
        ];
        let order = xy_cut_order(&boxes, body);
        // entire left column (indices 0,1,2) before the right (3,4,5)
        assert_eq!(order, vec![0, 1, 2, 3, 4, 5], "left column read fully before right");
        // a single column reads top-to-bottom (higher y first)
        let single: Vec<BBox> = vec![(0.0, 40.0, 30.0, 40.0), (0.0, 40.0, 90.0, 100.0), (0.0, 40.0, 60.0, 70.0)];
        assert_eq!(xy_cut_order(&single, body), vec![1, 2, 0], "top-to-bottom");
    }

    #[test]
    fn a_char_spacing_set_inside_a_q_does_not_survive_the_matching_q() {
        // `tests/gen_fixtures.py::gen_textstate_q`. The text state (Tc, Tw, Tf/Tfs, TL, Ts)
        // is part of the GRAPHICS state — PDF 32000-1 §9.3 — so `Q` restores it. Saving only
        // the CTM let `geology_usgs_fs.pdf`'s `Tc 0.047`, set inside a `q` at operation 10,
        // widen every glyph advance on all 68,529 operations of the page. On a GIS label
        // drawn one `Tj` per glyph and then repositioned by an absolute `Td` for its last
        // letter, the accumulated drift overshot that `Td` and the x-sort returned
        // "Cloverdale" as "Cloverdael" — a confidently wrong place name.
        //
        // Both lines of the fixture are byte-identical text; only the leading `q … Q` differs
        // from the empty state, so a leak shows up as a wrong word rather than a shifted one.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/textstate_q.pdf");
        let doc = Document::load(path).expect("textstate_q.pdf fixture must load");
        let raw = std::fs::read(path).expect("fixture readable");
        let page_id = *doc.get_pages().get(&1).expect("fixture has page 1");
        let spans = extract_spans(&doc, page_id, &raw);
        let mut by_line: std::collections::BTreeMap<i32, Vec<&Span>> = std::collections::BTreeMap::new();
        for s in &spans {
            by_line.entry(-(s.y.round() as i32)).or_default().push(s);
        }
        assert_eq!(by_line.len(), 2, "the fixture draws the word twice");
        for (_, mut line) in by_line {
            line.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
            let word: String = line.iter().map(|s| s.text.as_str()).collect();
            assert_eq!(word, "Cloverdale", "leaked char spacing transposed the last two glyphs");
        }
    }
}
