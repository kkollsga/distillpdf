//! Hybrid searchable-PDF writer: turn `OcrPage` blocks into a clean PDF whose text is
//! real, selectable PDF text (Helvetica + WinAnsi — covers Latin-1 incl. Portuguese
//! accents, no font embedding) and whose detected figure regions are re-embedded as
//! cropped images. The original full-page raster is dropped.
//!
//! Text is sized by role (title/heading/body/small) so the re-extraction path recovers
//! the same structure — this is what makes `to_html(in) ≈ to_html(to_pdf(in))` hold.

use image::DynamicImage;
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};

use crate::ocr::doctags::{BBox, Block, OcrPage};

/// One page to write: its block model, page size in points, and (optional) source image
/// for cropping figure regions.
pub(crate) struct PageInput {
    pub page: OcrPage,
    pub width: f32,
    pub height: f32,
    pub image: Option<DynamicImage>,
}

/// Font size by role. Encoding the role in the size lets the re-extraction path's
/// size-based heading detection reconstruct the structure.
fn role_size(b: &Block) -> f32 {
    match b {
        Block::Title(_) => 17.0,
        Block::Heading { level, .. } => (15.0 - (*level as f32 - 1.0) * 1.5).max(11.5),
        Block::Caption(_) | Block::Footnote(_) | Block::PageHeader(_) | Block::PageFooter(_) => 8.0,
        _ => 10.0,
    }
}

fn is_bold(b: &Block) -> bool {
    matches!(b, Block::Title(_) | Block::Heading { .. })
}

/// Text content of a block (None for tables/pictures, handled separately).
fn block_text(b: &Block) -> Option<&str> {
    match b {
        Block::Title(t) | Block::Para(t) | Block::ListItem(t) | Block::Caption(t)
        | Block::Footnote(t) | Block::Formula(t) | Block::Code(t) | Block::PageHeader(t)
        | Block::PageFooter(t) => Some(&t.text),
        Block::Heading { block, .. } => Some(&block.text),
        _ => None,
    }
}

fn block_bbox(b: &Block) -> Option<BBox> {
    match b {
        Block::Title(t) | Block::Para(t) | Block::ListItem(t) | Block::Caption(t)
        | Block::Footnote(t) | Block::Formula(t) | Block::Code(t) | Block::PageHeader(t)
        | Block::PageFooter(t) => t.bbox,
        Block::Heading { block, .. } => block.bbox,
        Block::Picture { bbox, .. } => *bbox,
        Block::Table(t) => t.bbox,
    }
}

/// Build the whole PDF and return its bytes.
pub(crate) fn write_pdf(pages: &[PageInput]) -> Result<Vec<u8>, String> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let helv = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1",
        "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
    });
    let helv_b = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold", "Encoding" => "WinAnsiEncoding",
    });

    let mut kids: Vec<Object> = Vec::new();
    for pin in pages {
        let (content, xobjects) = build_page_content(&mut doc, pin)?;
        let stream_id = doc.add_object(Stream::new(dictionary! {}, content.encode().map_err(|e| e.to_string())?));
        let mut xobj_dict = lopdf::Dictionary::new();
        for (name, id) in xobjects {
            xobj_dict.set(name.as_bytes().to_vec(), Object::Reference(id));
        }
        let resources = dictionary! {
            "Font" => dictionary! { "F1" => helv, "F2" => helv_b },
            "XObject" => xobj_dict,
        };
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), pin.width.into(), pin.height.into()],
            "Contents" => stream_id,
            "Resources" => resources,
        });
        kids.push(page_id.into());
    }

    let count = kids.len() as i64;
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => count,
    }));
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    // mark ourselves as the producer (so a re-run sees a clean, non-OCR producer)
    let info = doc.add_object(dictionary! {
        "Producer" => Object::String(b"distillpdf".to_vec(), lopdf::StringFormat::Literal)
    });
    doc.trailer.set("Info", info);

    let mut buf = Vec::new();
    doc.save_to(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Build one page's content stream operations + any image XObjects it references.
fn build_page_content(doc: &mut Document, pin: &PageInput) -> Result<(Content, Vec<(String, lopdf::ObjectId)>), String> {
    let (w, h) = (pin.width, pin.height);
    let mut ops: Vec<Operation> = Vec::new();
    let mut xobjects = Vec::new();
    let mut pic_no = 0;

    for b in &pin.page.blocks {
        match b {
            Block::Picture { bbox, .. } => {
                if let (Some(bb), Some(src)) = (bbox, &pin.image) {
                    let name = format!("Im{pic_no}");
                    if let Some(id) = embed_crop(doc, src, bb) {
                        let [x0, y0, x1, y1] = bb.to_pdf(w, h);
                        let (iw, ih) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
                        ops.push(Operation::new("q", vec![]));
                        ops.push(Operation::new("cm", vec![iw.into(), 0.into(), 0.into(), ih.into(), x0.into(), y0.into()]));
                        ops.push(Operation::new("Do", vec![Object::Name(name.as_bytes().to_vec())]));
                        ops.push(Operation::new("Q", vec![]));
                        xobjects.push((name, id));
                        pic_no += 1;
                    }
                }
            }
            Block::Table(t) => {
                // Render table cells as positioned text (row-major within the table box).
                emit_table_text(&mut ops, t, w, h);
            }
            _ => {
                if let (Some(text), Some(bb)) = (block_text(b), block_bbox(b)) {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let size = role_size(b);
                    let font = if is_bold(b) { "F2" } else { "F1" };
                    emit_wrapped_text(&mut ops, text, &bb, w, h, size, font);
                }
            }
        }
    }
    Ok((Content { operations: ops }, xobjects))
}

/// Word-wrap `text` to the box width and emit each line as positioned PDF text, top-down.
fn emit_wrapped_text(ops: &mut Vec<Operation>, text: &str, bb: &BBox, w: f32, h: f32, size: f32, font: &str) {
    let [x0, _y0, x1, y1] = bb.to_pdf(w, h);
    let box_w = (x1 - x0).max(size * 4.0);
    let leading = size * 1.2;
    let max_chars = (box_w / (size * 0.5)).max(8.0) as usize; // ~0.5em average glyph
    let lines = wrap(text, max_chars);
    let mut y = y1 - size; // first baseline just below the box top
    for line in lines {
        let bytes = to_winansi(&line);
        if bytes.is_empty() {
            y -= leading;
            continue;
        }
        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new("Tf", vec![Object::Name(font.as_bytes().to_vec()), size.into()]));
        ops.push(Operation::new("Td", vec![x0.into(), y.into()]));
        ops.push(Operation::new("Tj", vec![Object::String(bytes, lopdf::StringFormat::Literal)]));
        ops.push(Operation::new("ET", vec![]));
        y -= leading;
    }
}

fn emit_table_text(ops: &mut Vec<Operation>, t: &crate::ocr::doctags::Table, w: f32, h: f32) {
    let bb = t.bbox.unwrap_or(BBox { l: 0.05, t: 0.1, r: 0.95, b: 0.9 });
    let [x0, y0, x1, y1] = bb.to_pdf(w, h);
    let rows = t.rows.len().max(1);
    let row_h = ((y1 - y0) / rows as f32).max(10.0);
    let size = (row_h * 0.6).clamp(7.0, 11.0);
    for (ri, row) in t.rows.iter().enumerate() {
        let cols = row.len().max(1);
        let col_w = (x1 - x0) / cols as f32;
        let y = y1 - size - ri as f32 * row_h;
        for (ci, cell) in row.iter().enumerate() {
            let txt = cell.text.trim();
            if txt.is_empty() {
                continue;
            }
            let x = x0 + ci as f32 * col_w;
            let bytes = to_winansi(txt);
            let font = if cell.header { "F2" } else { "F1" };
            ops.push(Operation::new("BT", vec![]));
            ops.push(Operation::new("Tf", vec![Object::Name(font.as_bytes().to_vec()), size.into()]));
            ops.push(Operation::new("Td", vec![x.into(), y.into()]));
            ops.push(Operation::new("Tj", vec![Object::String(bytes, lopdf::StringFormat::Literal)]));
            ops.push(Operation::new("ET", vec![]));
        }
    }
}

/// Crop the source image to a normalized bbox and embed it as an RGB Image XObject
/// (FlateDecode). Returns the object id, or None if the crop is degenerate.
fn embed_crop(doc: &mut Document, src: &DynamicImage, bb: &BBox) -> Option<lopdf::ObjectId> {
    let (sw, sh) = (src.width() as f32, src.height() as f32);
    let x = (bb.l * sw).clamp(0.0, sw) as u32;
    let y = (bb.t * sh).clamp(0.0, sh) as u32;
    let cw = ((bb.r - bb.l) * sw).clamp(1.0, sw) as u32;
    let ch = ((bb.b - bb.t) * sh).clamp(1.0, sh) as u32;
    if cw < 2 || ch < 2 || x + cw > src.width() || y + ch > src.height() {
        return None;
    }
    // Encode the crop as JPEG and embed via /DCTDecode (the `jpeg` image feature is on),
    // so no extra deflate dependency is needed.
    let crop = src.crop_imm(x, y, cw, ch).to_rgb8();
    let mut jpg = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 85);
    if image::DynamicImage::ImageRgb8(crop).write_with_encoder(enc).is_err() {
        return None;
    }
    let stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => cw as i64,
            "Height" => ch as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
        },
        jpg,
    );
    Some(doc.add_object(stream))
}

// ---- text wrapping + WinAnsi encoding --------------------------------------

fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= max_chars {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// PDF literal-string bytes in WinAnsi. Unmappable chars become '?'.
fn to_winansi(s: &str) -> Vec<u8> {
    s.chars().map(win_ansi_byte).collect()
}

fn win_ansi_byte(c: char) -> u8 {
    let u = c as u32;
    match c {
        // ASCII and Latin-1 supplement map 1:1 to WinAnsi (covers Portuguese accents).
        _ if (0x20..=0x7E).contains(&u) => u as u8,
        _ if (0xA0..=0xFF).contains(&u) => u as u8,
        // common WinAnsi punctuation in the 0x80–0x9F band
        '\u{2013}' => 0x96, // en dash
        '\u{2014}' => 0x97, // em dash
        '\u{2018}' => 0x91, // ‘
        '\u{2019}' => 0x92, // ’
        '\u{201C}' => 0x93, // “
        '\u{201D}' => 0x94, // ”
        '\u{2022}' => 0x95, // bullet
        '\u{2026}' => 0x85, // ellipsis
        '\u{20AC}' => 0x80, // euro
        _ => b'?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::doctags::parse;

    fn page_from(dt: &str) -> PageInput {
        PageInput { page: parse(dt), width: 612.0, height: 792.0, image: None }
    }

    #[test]
    fn winansi_portuguese() {
        // accents map into the Latin-1 byte range, not '?'
        for c in "áàâãçéêíóôõúÁÇÕ".chars() {
            assert_ne!(win_ansi_byte(c), b'?', "{c} should map");
        }
        assert_eq!(win_ansi_byte('—'), 0x97);
        assert_eq!(win_ansi_byte('“'), 0x93);
    }

    #[test]
    fn wrap_words() {
        let l = wrap("the quick brown fox jumps", 9);
        assert!(l.iter().all(|s| s.chars().count() <= 9 || !s.contains(' ')));
        assert_eq!(l.join(" "), "the quick brown fox jumps");
    }

    #[test]
    fn writes_loadable_pdf_with_text() {
        let dt = "<loc_50><loc_30><loc_450><loc_60>COMISSÃO SINDICANTE\n\
                  <loc_50><loc_80><loc_450><loc_300>Em 02 de outubro de 2019 perante a comissão.";
        let bytes = write_pdf(&[page_from(dt)]).unwrap();
        assert!(bytes.starts_with(b"%PDF-"));
        // re-open with lopdf and confirm it parses + has one page
        let doc = Document::load_mem(&bytes).unwrap();
        assert_eq!(doc.get_pages().len(), 1);
    }

    fn visible_text(html: &str) -> String {
        // crude tag strip for comparing content
        let mut out = String::new();
        let mut in_tag = false;
        for c in html.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn roundtrip_to_html_matches() {
        // The invariant: to_html(in) ≈ to_html(to_pdf(in)) at the visible-text level.
        // H_a = render the OCR blocks to HTML directly.
        // H_b = write a clean PDF, then run distillPDF's normal to_html on it.
        let dt = "<loc_40><loc_30><loc_560><loc_55>TERMO DE DECLARACOES\n\
                  <loc_40><loc_70><loc_560><loc_180>Em dois de outubro de dois mil e dezenove perante \
                  a comissao de sindicancia designada pela portaria.\n\
                  <loc_40><loc_200><loc_560><loc_320>O presidente da comissao declarou aberta a sessao \
                  para a tomada de declaracoes das testemunhas arroladas.";
        let h_a = crate::ocr::render::doctags_to_html(dt);
        let bytes = write_pdf(&[page_from(dt)]).unwrap();
        let doc = Document::load_mem(&bytes).unwrap();
        let h_b = crate::html::to_html(&doc, &bytes, crate::html::Mode::Page, false, false);
        let (ta, tb) = (visible_text(&h_a), visible_text(&h_b));
        // every content word from the direct render must survive the PDF round trip
        for w in ta.split(' ').filter(|w| w.chars().count() > 3) {
            assert!(tb.contains(w), "round-trip dropped {w:?}\n A={ta}\n B={tb}");
        }
    }

    #[test]
    fn roundtrip_text_preserved() {
        // Build a page, write a PDF, re-extract text with distillPDF's own extractor,
        // and assert the visible words survive in order (the core to_pdf invariant).
        let dt = "<loc_40><loc_30><loc_560><loc_60>TERMO DE DECLARACOES\n\
                  <loc_40><loc_80><loc_560><loc_200>Em dois de outubro de dois mil e dezenove.";
        let bytes = write_pdf(&[page_from(dt)]).unwrap();
        let doc = Document::load_mem(&bytes).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        let got = crate::text::extract_page(&doc, page_id, &bytes).unwrap_or_default();
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let got = norm(&got);
        for w in ["TERMO", "DECLARACOES", "outubro", "dezenove"] {
            assert!(got.contains(w), "extracted text missing {w:?}: {got:?}");
        }
    }
}
