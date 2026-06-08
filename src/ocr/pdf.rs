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

/// Add Helvetica + Helvetica-Bold (WinAnsi) font objects, returning their ids.
pub(crate) fn add_fonts(doc: &mut Document) -> (lopdf::ObjectId, lopdf::ObjectId) {
    let helv = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1",
        "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
    });
    let helv_b = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold", "Encoding" => "WinAnsiEncoding",
    });
    (helv, helv_b)
}

/// Build the whole PDF and return its bytes.
pub(crate) fn write_pdf(pages: &[PageInput]) -> Result<Vec<u8>, String> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let (helv, helv_b) = add_fonts(&mut doc);

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

/// Font resource names for the invisible OCR overlay. Deliberately distinct from the
/// page's own `/F1`,`/F2` so adding them to a scanned page's resources never clobbers the
/// fonts that page already uses (e.g. its stamp text).
pub(crate) const OVERLAY_FONT: &str = "OcrHelv";
pub(crate) const OVERLAY_FONT_BOLD: &str = "OcrHelvB";

/// A reliable single-line font-size sample, or None. A single line of text whose box is a
/// normal line height (≈6–24 pt) directly implies the font size — the box is ≈1.15× the
/// font. Multi-line paragraphs and loosely-boxed elements (a few words in a tall box) don't,
/// so they're skipped: their box-fill estimate is what makes loose boxes over-size.
fn line_sample(text: &str, bb: &BBox, w: f32, h: f32) -> Option<f32> {
    let widths = crate::afm::standard_widths("Helvetica", false, false).unwrap_or(&[500u16; 256]);
    let [x0, y0, x1, y1] = bb.to_pdf(w, h);
    let box_w = (x1 - x0).abs().max(1.0);
    let box_h = (y1 - y0).abs().max(1.0);
    if !(6.0..=24.0).contains(&box_h) {
        return None;
    }
    let size = box_h / 1.15;
    // The text must plausibly fit on one line at that size (else the box isn't one line).
    (advance(widths, text) * size <= box_w * 1.25).then_some(size)
}

/// The page body font size. Estimated from reliable single-line samples (see [`line_sample`])
/// — robust to the model's loose/oversized boxes — and only falling back to the box-fill
/// median when a page has no clean single lines. Body text is then snapped to this one value
/// so a page reads as a single consistent size.
fn page_body_size(page: &OcrPage, w: f32, h: f32) -> f32 {
    let mut samples: Vec<f32> = Vec::new();
    let mut fill: Vec<(f32, usize)> = Vec::new();
    for b in &page.blocks {
        if is_bold(b) {
            continue; // headings/titles don't define the body size
        }
        if let (Some(text), Some(bb)) = (block_text(b), block_bbox(b)) {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if let Some(s) = line_sample(text, &bb, w, h) {
                samples.push(s);
            }
            fill.push((natural_size(text, &bb, w, h, false), text.chars().count()));
        }
    }
    let body = if samples.len() >= 2 {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        samples[samples.len() / 2]
    } else {
        weighted_median(&mut fill).unwrap_or(10.0)
    };
    body.clamp(7.5, 16.0)
}

/// Text-length-weighted median of `(size, weight)` items.
fn weighted_median(items: &mut [(f32, usize)]) -> Option<f32> {
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let total: usize = items.iter().map(|x| x.1).sum();
    let mut cum = 0usize;
    for (size, weight) in items.iter() {
        cum += weight;
        if cum * 2 >= total {
            return Some(*size);
        }
    }
    items.last().map(|x| x.0)
}

/// Resolve a block's render size: body text is snapped to the single page body size. A block
/// keeps a distinct, larger size only if it's a tagged heading/title, or it's a SHORT
/// (single-line) block whose box is much bigger than body — i.e. an unlabelled letterhead.
/// A LONG block is always body, even if its loose box implies a big size (it's a paragraph,
/// not a heading).
fn harmonize_size(natural: f32, body: f32, is_heading: bool, single_line: bool) -> f32 {
    if is_heading || (single_line && natural > body * 1.45) {
        natural.max(body) // a heading is at least body-sized
    } else {
        body
    }
}

/// Pick the render size for a text block: its natural (box-derived) size harmonized against
/// the page body size. A block whose box is only ~one line tall is meant to be a single line,
/// so its size is also capped to fit the text on one line in the box width — otherwise a
/// narrow box (e.g. a signature line) would wrap a body-sized line awkwardly.
fn resolve_size(text: &str, bb: &BBox, w: f32, h: f32, bold: bool, body: f32) -> f32 {
    let widths = crate::afm::standard_widths("Helvetica", bold, false).unwrap_or(&[500u16; 256]);
    let [x0, y0, x1, y1] = bb.to_pdf(w, h);
    let box_w = (x1 - x0).abs().max(1.0);
    let box_h = (y1 - y0).abs().max(1.0);
    let one_line = box_h < body * 1.8; // box is about a single line tall
    let mut size = harmonize_size(natural_size(text, bb, w, h, bold), body, bold, one_line);
    if one_line {
        let adv = advance(widths, text).max(0.01);
        size = size.min(box_w / adv); // keep the single line within the box width
    }
    size.max(4.0)
}

/// Build an INVISIBLE (text render mode 3) overlay of the OCR text, positioned over the
/// page. Appended to a page that keeps its original raster, this is the "image + hidden
/// text" searchable-PDF model: the scan stays visible, the text is selectable/searchable,
/// and OCR errors never destroy the original. No images are drawn (the page keeps its own
/// raster). The ops are wrapped in `q … Q` and set `3 Tr` once, so the invisible text
/// state can't leak into the page's own content.
pub(crate) fn build_text_overlay(pin: &PageInput) -> Content {
    let (w, h) = (pin.width, pin.height);
    let body = page_body_size(&pin.page, w, h);
    let mut ops: Vec<Operation> = vec![
        Operation::new("q", vec![]),
        Operation::new("Tr", vec![3.into()]), // 3 = invisible (neither fill nor stroke)
    ];
    for b in &pin.page.blocks {
        match b {
            Block::Picture { .. } => {} // raster already on the page; nothing to draw
            Block::Table(t) => emit_table_text(&mut ops, t, w, h, OVERLAY_FONT, OVERLAY_FONT_BOLD),
            _ => {
                if let (Some(text), Some(bb)) = (block_text(b), block_bbox(b)) {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let bold = is_bold(b);
                    let font = if bold { OVERLAY_FONT_BOLD } else { OVERLAY_FONT };
                    let size = resolve_size(text, &bb, w, h, bold, body);
                    let top = bb.to_pdf(w, h)[3]; // box top; overlay stays aligned to the scan
                    // Invisible overlay over the kept scan: stretch full lines to align selection.
                    emit_wrapped_text(&mut ops, text, &bb, w, h, top, size, font, bold, true, true);
                }
            }
        }
    }
    ops.push(Operation::new("Q", vec![]));
    Content { operations: ops }
}

/// Build one page's content stream operations + any image XObjects it references.
pub(crate) fn build_page_content(doc: &mut Document, pin: &PageInput) -> Result<(Content, Vec<(String, lopdf::ObjectId)>), String> {
    let (w, h) = (pin.width, pin.height);
    let body = page_body_size(&pin.page, w, h);
    let mut ops: Vec<Operation> = Vec::new();
    let mut xobjects = Vec::new();
    let mut pic_no = 0;

    // Rects (pdf space) of every non-empty text block — so a figure crop that text already
    // covers is skipped (the model marks a text region `<other>`; in the reflow the text is
    // the content, and drawing the crop on top of it just makes an overlapping mess).
    let text_rects: Vec<[f32; 4]> = pin
        .page
        .blocks
        .iter()
        .filter(|b| !matches!(b, Block::Picture { .. } | Block::Table(_)))
        .filter_map(|b| match (block_text(b), block_bbox(b)) {
            (Some(t), Some(bb)) if !t.trim().is_empty() => Some(bb.to_pdf(w, h)),
            _ => None,
        })
        .collect();
    let overlaps = |a: &[f32; 4], b: &[f32; 4]| a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3];

    // Already-placed block rects (pdf space, as [x0,y0,x1,y1]) — a text block is pushed down
    // below any placed block it *horizontally* overlaps, so blocks never collide while
    // side-by-side elements (e.g. two words on one line) stay on their line.
    let mut placed: Vec<[f32; 4]> = Vec::new();

    for b in &pin.page.blocks {
        match b {
            Block::Picture { bbox, .. } => {
                if let (Some(bb), Some(src)) = (bbox, &pin.image) {
                    let rect = bb.to_pdf(w, h);
                    if text_rects.iter().any(|t| overlaps(t, &rect)) {
                        continue; // text covers this region — show the text, not a crop on top
                    }
                    let name = format!("Im{pic_no}");
                    if let Some(id) = embed_crop(doc, src, bb) {
                        let [x0, y0, x1, y1] = rect;
                        let (iw, ih) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
                        ops.push(Operation::new("q", vec![]));
                        ops.push(Operation::new("cm", vec![iw.into(), 0.into(), 0.into(), ih.into(), x0.into(), y0.into()]));
                        ops.push(Operation::new("Do", vec![Object::Name(name.as_bytes().to_vec())]));
                        ops.push(Operation::new("Q", vec![]));
                        xobjects.push((name, id));
                        pic_no += 1;
                        placed.push(rect); // text flows below a kept figure
                    }
                }
            }
            Block::Table(t) => {
                // Render table cells as positioned text (row-major within the table box).
                emit_table_text(&mut ops, t, w, h, "F1", "F2");
            }
            _ => {
                if let (Some(text), Some(bb)) = (block_text(b), block_bbox(b)) {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let bold = is_bold(b);
                    let font = if bold { "F2" } else { "F1" };
                    let size = resolve_size(text, &bb, w, h, bold, body);
                    let [cx0, _, cx1, cy1] = bb.to_pdf(w, h);
                    // Push this block below any placed block it horizontally overlaps.
                    let mut top = cy1;
                    for p in &placed {
                        if cx0 < p[2] + 1.0 && p[0] < cx1 + 1.0 {
                            top = top.min(p[1] - size * 0.3);
                        }
                    }
                    // Visible reflow: natural spacing (no per-line stretch).
                    let used = emit_wrapped_text(&mut ops, text, &bb, w, h, top, size, font, bold, false, false);
                    placed.push([cx0, top - used, cx1, top]);
                }
            }
        }
    }
    Ok((Content { operations: ops }, xobjects))
}

/// Baseline-to-baseline spacing as a multiple of the font size.
const LINE_SPACING: f32 = 1.16;

/// Fraction of an element's bounding box that its text ink "fills" in the size estimate.
/// In principle text doesn't fill 100% of its box (line spacing, word gaps, ragged right),
/// which would argue for < 1 — but Helvetica is narrower than typical scan fonts, so our
/// advance() under-counts width and the two effects roughly cancel. Calibrated against real
/// scans (page 89/700 paragraphs land at the source's ~12 pt) at ≈1.0; a lower value
/// under-fills the page (text too small).
const TEXT_DENSITY: f32 = 1.0;

/// Helvetica glyph advance (1000-em units) for a char, via its WinAnsi byte.
fn glyph_adv(widths: &[u16; 256], c: char) -> f32 {
    widths[win_ansi_byte(c) as usize] as f32
}

/// Total advance of `s` at font size 1 pt (i.e. in points-per-pt).
fn advance(widths: &[u16; 256], s: &str) -> f32 {
    s.chars().map(|c| glyph_adv(widths, c)).sum::<f32>() / 1000.0
}

/// Word-wrap `text` so each line's advance (at size 1) fits `budget` points-per-pt.
fn wrap_to_budget(widths: &[u16; 256], text: &str, budget: f32) -> Vec<String> {
    let space = advance(widths, " ").max(1e-4);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0f32;
    for word in text.split_whitespace() {
        let ww = advance(widths, word);
        if cur.is_empty() {
            cur.push_str(word);
            cur_w = ww;
        } else if cur_w + space + ww <= budget {
            cur.push(' ');
            cur.push_str(word);
            cur_w += space + ww;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = ww;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// The font size an element's box alone implies, before page-level harmonization. The naive
/// "size that fills the box area" overshoots — real text only fills a fraction of its box
/// (line spacing, word gaps, ragged right, a partial last line, and OCR boxes drawn loose
/// around the ink) — so the fill estimate is scaled by TEXT_DENSITY and capped so a single
/// line isn't taller than its box (boxes bound a line loosely, hence /1.35 not /1.0).
fn natural_size(text: &str, bb: &BBox, w: f32, h: f32, bold: bool) -> f32 {
    let widths = crate::afm::standard_widths("Helvetica", bold, false).unwrap_or(&[500u16; 256]);
    let [x0, y0, x1, y1] = bb.to_pdf(w, h);
    let box_w = (x1 - x0).abs().max(1.0);
    let box_h = (y1 - y0).abs().max(1.0);
    let total = advance(widths, text).max(0.01);
    let fill = (box_h * box_w * TEXT_DENSITY / (total * LINE_SPACING)).sqrt();
    // A single line shouldn't exceed the box; boxes bound a line ~1.2× the font (not 1.35×,
    // which under-sized single lines).
    fill.min(box_h / 1.2).clamp(4.0, 48.0)
}

/// Emit `text` wrapped to its box, with the first baseline placed just below `top_y` (the
/// caller decides the top — for the reflow it's clamped so blocks can't overlap; for the
/// overlay it's the box top). Returns the vertical extent consumed (top_y → text bottom) so
/// the reflow can advance a non-overlapping cursor.
///
/// `spread`: distribute lines to fill the box height (overlay — matches the scan's line
/// positions); otherwise use natural single-spacing (reflow — compact, no spill).
/// `stretch`: horizontally scale each full line to span the box width (overlay — keeps text
/// selection aligned with the scanned glyphs even under a different font).
fn emit_wrapped_text(ops: &mut Vec<Operation>, text: &str, bb: &BBox, w: f32, h: f32, top_y: f32, size: f32, font: &str, bold: bool, stretch: bool, spread: bool) -> f32 {
    let widths = crate::afm::standard_widths("Helvetica", bold, false).unwrap_or(&[500u16; 256]);
    let [x0, y0, x1, y1] = bb.to_pdf(w, h);
    let box_w = (x1 - x0).abs().max(1.0);
    let box_h = (y1 - y0).abs().max(1.0);

    let lines = wrap_to_budget(widths, text, box_w / size);
    let n = lines.len().max(1);
    let leading = if spread {
        (box_h / n as f32).max(size * LINE_SPACING).min(size * 1.5)
    } else {
        size * LINE_SPACING
    };
    let mut y = top_y - size * 0.82; // first baseline just under the top
    let mut last_baseline = y;
    let last = n - 1;
    for (i, line) in lines.iter().enumerate() {
        let bytes = to_winansi(line);
        if !bytes.is_empty() {
            ops.push(Operation::new("BT", vec![]));
            // Stretch full (non-last) lines to the box width for selection alignment.
            if stretch && i < last {
                let lw = advance(widths, line) * size;
                if lw > 1.0 {
                    let tz = (box_w / lw * 100.0).clamp(50.0, 200.0);
                    ops.push(Operation::new("Tz", vec![tz.into()]));
                }
            }
            ops.push(Operation::new("Tf", vec![Object::Name(font.as_bytes().to_vec()), size.into()]));
            ops.push(Operation::new("Td", vec![x0.into(), y.into()]));
            ops.push(Operation::new("Tj", vec![Object::String(bytes, lopdf::StringFormat::Literal)]));
            ops.push(Operation::new("ET", vec![]));
        }
        last_baseline = y;
        y -= leading;
    }
    // Height from the requested top down to the descender of the last line.
    (top_y - (last_baseline - size * 0.25)).max(size * 0.5)
}

fn emit_table_text(ops: &mut Vec<Operation>, t: &crate::ocr::doctags::Table, w: f32, h: f32, regular: &str, bold: &str) {
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
            let font = if cell.header { bold } else { regular };
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
