//! Decide, per page, whether OCR is needed — and whether an existing (scanner) text
//! layer should be distrusted and replaced.
//!
//! Three cases the feature handles:
//!  (a) page has an image but no extractable text          → `NeedsOcr`
//!  (b) page has an image and only sparse/stamp text       → `NeedsOcr`
//!  (c) page has an image and a *scanner/Tesseract* text   → `DropAndOcr`
//!      layer (low-quality OCR baked into the PDF)
//! A born-digital text page (no image, real text) is left untouched (`NotNeeded`).

use lopdf::{Document, Object, ObjectId};

use crate::text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OcrDecision {
    NotNeeded,
    /// Image page with no / sparse text — OCR it.
    NeedsOcr,
    /// Image page whose embedded text is untrusted scanner OCR — drop it and OCR.
    DropAndOcr,
}

/// Below this many characters, an imaged page is treated as having no real text layer.
const MIN_TEXT_CHARS: usize = 120;

/// PDF producers/creators that indicate a baked-in OCR layer we shouldn't trust.
const OCR_PRODUCERS: &[&str] = &[
    "tesseract", "abbyy", "finereader", "ocrmypdf", "ocr", "scan", "scansnap",
    "kofax", "omnipage", "readiris", "adobe acrobat capture", "paperport",
];

/// Pure decision given the page facts. Separated for testability.
fn decide_from(has_image: bool, n_text: usize, producer: &str, garbled: bool) -> OcrDecision {
    if !has_image {
        return OcrDecision::NotNeeded;
    }
    if n_text < MIN_TEXT_CHARS {
        return OcrDecision::NeedsOcr;
    }
    if looks_like_ocr_producer(producer) || garbled {
        return OcrDecision::DropAndOcr;
    }
    OcrDecision::NotNeeded
}

/// Per-page OCR decision against a real document.
pub(crate) fn decide(doc: &Document, page_id: ObjectId, raw: &[u8]) -> OcrDecision {
    let has_image = doc.get_page_images(page_id).map(|v| !v.is_empty()).unwrap_or(false);
    if !has_image {
        return OcrDecision::NotNeeded;
    }
    let txt = text::extract_page(doc, page_id, raw).unwrap_or_default();
    let n = txt.trim().chars().count();
    let producer = doc_producer(doc).unwrap_or_default();
    decide_from(has_image, n, &producer, text_is_garbled(&txt))
}

fn looks_like_ocr_producer(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    OCR_PRODUCERS.iter().any(|p| s.contains(p))
}

/// Heuristic: does this text look like low-quality OCR? Conservative — we only flag
/// clear signals (Unicode replacement chars, or a high rate of broken short "words"),
/// to avoid distrusting good born-digital text.
fn text_is_garbled(text: &str) -> bool {
    let t = text.trim();
    let total = t.chars().count();
    if total < 200 {
        return false; // too little to judge
    }
    let replacement = t.chars().filter(|&c| c == '\u{FFFD}').count();
    if replacement * 200 >= total {
        return true; // ≥0.5% replacement chars
    }
    // Fraction of alphabetic "words" with no vowel (OCR mangling tends to produce these).
    let mut words = 0usize;
    let mut novowel = 0usize;
    for w in t.split_whitespace() {
        let letters: String = w.chars().filter(|c| c.is_alphabetic()).collect();
        if letters.chars().count() >= 3 {
            words += 1;
            let has_vowel = letters.chars().any(|c| "aeiouáéíóúâêôãõàAEIOUÁÉÍÓÚ".contains(c));
            if !has_vowel {
                novowel += 1;
            }
        }
    }
    words >= 20 && novowel * 100 >= words * 18 // ≥18% vowel-less words
}

/// Read the document's Info /Producer (falling back to /Creator) as a lossy string.
fn doc_producer(doc: &Document) -> Option<String> {
    let info = doc.trailer.get(b"Info").ok()?;
    let dict = match info {
        Object::Reference(id) => doc.get_object(*id).ok()?.as_dict().ok()?,
        Object::Dictionary(d) => d,
        _ => return None,
    };
    for key in [b"Producer".as_slice(), b"Creator".as_slice()] {
        if let Ok(v) = dict.get(key) {
            if let Ok(bytes) = v.as_str() {
                let s = decode_pdf_string(bytes);
                if !s.trim().is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// Decode a PDF text string: UTF-16BE if it carries a BOM, else Latin-1/lossy UTF-8.
fn decode_pdf_string(b: &[u8]) -> String {
    if b.len() >= 2 && b[0] == 0xFE && b[1] == 0xFF {
        let u16s: Vec<u16> = b[2..].chunks(2).filter(|c| c.len() == 2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_image_never_ocr() {
        assert_eq!(decide_from(false, 0, "", false), OcrDecision::NotNeeded);
        assert_eq!(decide_from(false, 9999, "tesseract", true), OcrDecision::NotNeeded);
    }

    #[test]
    fn image_no_text_needs_ocr() {
        assert_eq!(decide_from(true, 0, "", false), OcrDecision::NeedsOcr);
        assert_eq!(decide_from(true, 50, "PJe", false), OcrDecision::NeedsOcr); // stamp-only
    }

    #[test]
    fn image_good_digital_text_left_alone() {
        assert_eq!(decide_from(true, 3000, "Microsoft Word", false), OcrDecision::NotNeeded);
    }

    #[test]
    fn scanner_producer_drops_layer() {
        assert_eq!(decide_from(true, 3000, "Tesseract OCR 5.3", false), OcrDecision::DropAndOcr);
        assert_eq!(decide_from(true, 3000, "ABBYY FineReader", false), OcrDecision::DropAndOcr);
        assert_eq!(decide_from(true, 3000, "ocrmypdf 14", false), OcrDecision::DropAndOcr);
    }

    #[test]
    fn garbled_text_drops_layer() {
        assert_eq!(decide_from(true, 3000, "Unknown", true), OcrDecision::DropAndOcr);
    }

    #[test]
    fn ocr_producer_match() {
        assert!(looks_like_ocr_producer("Tesseract 5"));
        assert!(looks_like_ocr_producer("ABBYY FineReader 15"));
        assert!(!looks_like_ocr_producer("Microsoft® Word 2019"));
        assert!(!looks_like_ocr_producer("LaTeX with hyperref"));
    }

    #[test]
    fn garble_heuristic() {
        // good Portuguese prose → not garbled
        let good = "Em dois de outubro de dois mil e dezenove perante a comissão de sindicância \
                    designada para apurar os fatos relativos ao processo administrativo em curso na \
                    promotoria de justiça regional conforme portaria publicada no diário oficial.";
        assert!(!text_is_garbled(good));
        // replacement chars → garbled
        let bad: String = std::iter::repeat("x\u{FFFD}y ").take(80).collect();
        assert!(text_is_garbled(&bad));
    }

    #[test]
    fn utf16be_producer_decodes() {
        let mut b = vec![0xFE, 0xFF];
        for ch in "Tesseract".encode_utf16() {
            b.extend_from_slice(&ch.to_be_bytes());
        }
        assert!(looks_like_ocr_producer(&decode_pdf_string(&b)));
    }
}
