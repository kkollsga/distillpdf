//! Clean-room pure-Rust parser for the DocTags grammar emitted by granite-docling
//! (and SmolDocling). Produces a typed [`OcrPage`] block model.
//!
//! Grammar (from the public DocTags spec / docling-core, no code copied):
//! * An element is `<tag><loc_x1><loc_y1><loc_x2><loc_y2>content</tag>`; coordinates are
//!   integers normalized by 500 (page-relative, origin top-left). The closing tag and the
//!   element tag are both optional in practice — granite often emits the *bare* form
//!   `<loc..>text` for plain text and `<loc..><region>` for a picture, so the parser is
//!   lenient and accepts both the wrapped and bare dialects.
//! * Structural tags: `title`, `section_header_level_1..6`, `text`, `page_header`,
//!   `page_footer`, `caption`, `list_item`, `formula`, `code`, `footnote`,
//!   `document_index`, `key_value_region`, `picture`, `otsl` (table).
//! * A `picture` carries a classification label (`logo`, `signature`, `line_chart`, …)
//!   and an optional `<caption>…</caption>`.
//! * A table (`otsl`) uses OTSL cell tokens: `<fcel>` filled, `<ecel>` empty,
//!   `<lcel>/<ucel>/<xcel>` span continuations, `<nl>` row break, `<ched>/<rhed>/<srow>`
//!   header/section cells.

/// DocTags location integers are normalized by this divisor (page-relative 0..1).
const LOC_DIV: f32 = 500.0;

/// A normalized bounding box (0..1, origin top-left).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BBox {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl BBox {
    /// Map to PDF user space (origin bottom-left) given page size in points.
    pub(crate) fn to_pdf(&self, w: f32, h: f32) -> [f32; 4] {
        [self.l * w, (1.0 - self.b) * h, self.r * w, (1.0 - self.t) * h]
    }
    pub(crate) fn height_pts(&self, h: f32) -> f32 {
        (self.b - self.t).abs() * h
    }
}

/// A run of text with its (optional) box. `level` is the heading depth (1..6) for headings.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TextBlock {
    pub text: String,
    pub bbox: Option<BBox>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cell {
    pub text: String,
    pub header: bool,
    pub colspan: u32,
    pub rowspan: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Table {
    pub bbox: Option<BBox>,
    pub rows: Vec<Vec<Cell>>,
    pub caption: Option<String>,
}

/// A typed page element in reading order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    Title(TextBlock),
    Heading { level: u8, block: TextBlock },
    Para(TextBlock),
    ListItem(TextBlock),
    Caption(TextBlock),
    Footnote(TextBlock),
    Formula(TextBlock),
    Code(TextBlock),
    PageHeader(TextBlock),
    PageFooter(TextBlock),
    Picture { bbox: Option<BBox>, class: Option<String>, caption: Option<String> },
    Table(Table),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct OcrPage {
    pub blocks: Vec<Block>,
}

// ---- tokenizer -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Loc(f32),       // <loc_N> -> N
    Open(String),   // <name>
    Close(String),  // </name>
    Text(String),   // raw text between tags
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut text = String::new();
    let flush = |text: &mut String, toks: &mut Vec<Tok>| {
        if !text.is_empty() {
            toks.push(Tok::Text(std::mem::take(text)));
        }
    };
    while i < s.len() {
        if bytes[i] == b'<' {
            // find matching '>'
            if let Some(rel) = s[i + 1..].find('>') {
                let inner = &s[i + 1..i + 1 + rel];
                // A lone '<' that isn't a real tag (e.g. math "a < b"): treat as text.
                if inner.is_empty() || inner.contains('<') {
                    text.push('<');
                    i += 1;
                    continue;
                }
                // Only treat `<...>` as a token if it's a *known* DocTags tag; otherwise
                // it's literal content (e.g. a math `a < b`), kept verbatim as text.
                let tok = if let Some(n) = inner.strip_prefix("loc_") {
                    n.parse::<f32>().ok().map(Tok::Loc)
                } else if let Some(name) = inner.strip_prefix('/') {
                    is_known_tag(name).then(|| Tok::Close(name.to_string()))
                } else {
                    is_known_tag(inner).then(|| Tok::Open(inner.to_string()))
                };
                match tok {
                    Some(t) => {
                        flush(&mut text, &mut toks);
                        toks.push(t);
                    }
                    None => {
                        // not a recognized tag — keep the literal "<inner>"
                        text.push('<');
                        text.push_str(inner);
                        text.push('>');
                    }
                }
                i += 1 + rel + 1;
                continue;
            }
        }
        // copy one UTF-8 char
        let ch_len = utf8_len(bytes[i]);
        text.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    flush(&mut text, &mut toks);
    toks
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

// ---- tag classification ----------------------------------------------------

/// Is `name` a DocTags tag we recognize (so `<name>` is a token, not literal text)?
fn is_known_tag(name: &str) -> bool {
    matches!(name, "otsl" | "picture" | "caption" | "doctag")
        || is_structural(name)
        || is_region(name)
        || is_otsl_cell(name)
}

fn is_structural(name: &str) -> bool {
    matches!(
        name,
        "title" | "text" | "page_header" | "page_footer" | "caption" | "list_item"
            | "formula" | "code" | "footnote" | "document_index" | "key_value_region"
    ) || name.starts_with("section_header_level_")
}

fn is_region(name: &str) -> bool {
    matches!(
        name,
        "logo" | "photograph" | "icon" | "engineering_drawing" | "line_chart" | "bar_chart"
            | "pie_chart" | "flow_chart" | "other" | "table" | "screenshot" | "signature"
            | "geographical_map" | "map" | "page_thumbnail" | "stamp" | "chart" | "form"
            | "qr_code" | "barcode" | "screenshot_from_computer" | "screenshot_from_manual"
    )
}

fn is_otsl_cell(name: &str) -> bool {
    matches!(name, "fcel" | "ecel" | "lcel" | "ucel" | "xcel" | "nl" | "ched" | "rhed" | "srow")
}

// ---- parser ----------------------------------------------------------------

/// The text content of a DocTags line with its `<…>` tags removed — the key used to detect
/// a repetition loop (two lines "repeat" when their text matches, regardless of bbox).
fn strip_tags(line: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in line.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Collapse repetition-loop runs in raw DocTags. The vision model occasionally gets stuck
/// emitting the same line (or a short cycle of lines) over and over — e.g. "!" ×190, or a
/// 2-line email header repeated dozens of times. We detect a period-`p` (1..=8) block that
/// repeats ≥3× consecutively and keep a single copy.
///
/// Deliberately conservative — only *immediately adjacent* cycles are collapsed, so content
/// that legitimately recurs but is separated by other text (e.g. an email signature quoted
/// once per thread) is untouched. On a 509-page real corpus this removed ~1950 garbage
/// lines from looping pages while altering only 2 lines across 435 clean pages.
fn collapse_loops(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    let n = lines.len();
    if n < 3 {
        return input.to_string();
    }
    let keys: Vec<String> = lines.iter().map(|l| strip_tags(l)).collect();
    let mut keep = vec![true; n];
    let mut i = 0;
    while i < n {
        let mut hit = false;
        for p in 1..=8usize {
            if i + 2 * p > n {
                continue;
            }
            let mut reps = 1usize;
            while i + (reps + 1) * p <= n && keys[i + reps * p..i + (reps + 1) * p] == keys[i..i + p] {
                reps += 1;
            }
            // A single repeated line (p==1) is always a loop; for a longer cycle require it
            // to carry real text (≥4 chars), so we never collapse incidental short patterns.
            let block_has_text = (0..p).any(|k| keys[i + k].chars().count() >= 4);
            if reps >= 3 && (p == 1 || block_has_text) {
                for slot in keep.iter_mut().take(i + reps * p).skip(i + p) {
                    *slot = false;
                }
                i += reps * p;
                hit = true;
                break;
            }
        }
        if !hit {
            i += 1;
        }
    }
    if keep.iter().all(|&k| k) {
        return input.to_string(); // no loops — avoid a needless reallocation
    }
    lines
        .iter()
        .zip(keep)
        .filter_map(|(l, k)| k.then_some(*l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a DocTags string into a typed page model.
pub(crate) fn parse(input: &str) -> OcrPage {
    let delooped = collapse_loops(input);
    // Strip an optional <doctag>…</doctag> wrapper.
    let s = delooped.trim();
    let s = s.strip_prefix("<doctag>").unwrap_or(s);
    let s = s.strip_suffix("</doctag>").unwrap_or(s);
    let toks = tokenize(s);
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Text(t) if t.trim().is_empty() => i += 1,
            Tok::Close(_) => i += 1,
            Tok::Open(name) if name == "otsl" => {
                let (table, ni) = parse_otsl(&toks, i);
                blocks.push(Block::Table(table));
                i = ni;
            }
            Tok::Open(name) if name == "picture" => {
                let (pic, ni) = parse_picture(&toks, i);
                blocks.push(pic);
                i = ni;
            }
            Tok::Open(name) if is_structural(name) => {
                let label = name.clone();
                i += 1;
                let bbox = take_locs(&toks, &mut i);
                let text = take_text(&toks, &mut i, Some(&label));
                blocks.push(make_text_block(&label, TextBlock { text, bbox }));
            }
            Tok::Open(name) if is_region(name) => {
                let class = name.clone();
                i += 1;
                blocks.push(Block::Picture { bbox: None, class: Some(class), caption: None });
            }
            Tok::Loc(_) => {
                // Bare element: up to 4 locs, then a region tag or text.
                let bbox = take_locs(&toks, &mut i);
                match toks.get(i) {
                    Some(Tok::Open(name)) if is_region(name) => {
                        let class = name.clone();
                        i += 1;
                        blocks.push(Block::Picture { bbox, class: Some(class), caption: None });
                    }
                    _ => {
                        let text = take_text(&toks, &mut i, None);
                        if !text.trim().is_empty() {
                            blocks.push(classify_bare(TextBlock { text, bbox }));
                        }
                    }
                }
            }
            Tok::Open(_) => i += 1, // unknown tag — skip
            Tok::Text(_) => {
                // stray text with no box/label — keep as a paragraph
                let text = take_text(&toks, &mut i, None);
                if !text.trim().is_empty() {
                    blocks.push(classify_bare(TextBlock { text, bbox: None }));
                }
            }
        }
    }
    OcrPage { blocks }
}

/// Consume up to 4 consecutive `Loc` tokens into a BBox (None if fewer than 4).
fn take_locs(toks: &[Tok], i: &mut usize) -> Option<BBox> {
    let mut v = Vec::with_capacity(4);
    while v.len() < 4 {
        match toks.get(*i) {
            Some(Tok::Loc(n)) => {
                v.push(*n);
                *i += 1;
            }
            _ => break,
        }
    }
    if v.len() == 4 {
        Some(BBox { l: v[0] / LOC_DIV, t: v[1] / LOC_DIV, r: v[2] / LOC_DIV, b: v[3] / LOC_DIV })
    } else {
        None
    }
}

/// Accumulate text until the element ends: a matching close tag, the start of a new
/// element (a structural/region/otsl/picture open, or a fresh `Loc` run), or EOF.
fn take_text(toks: &[Tok], i: &mut usize, label: Option<&str>) -> String {
    let mut out = String::new();
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Text(t) => {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push_str(t.trim());
                *i += 1;
            }
            Tok::Close(name) => {
                if Some(name.as_str()) == label {
                    *i += 1; // consume our own close
                }
                break;
            }
            Tok::Loc(_) => break, // next element's box
            Tok::Open(_) => break, // next element / inline marker
        }
    }
    normalize_ws(&out)
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn make_text_block(label: &str, tb: TextBlock) -> Block {
    if let Some(n) = label.strip_prefix("section_header_level_") {
        let level = n.parse::<u8>().unwrap_or(2).clamp(1, 6);
        return Block::Heading { level, block: tb };
    }
    match label {
        "title" => Block::Title(tb),
        "page_header" => Block::PageHeader(tb),
        "page_footer" => Block::PageFooter(tb),
        "caption" => Block::Caption(tb),
        "list_item" => Block::ListItem(tb),
        "formula" => Block::Formula(tb),
        "code" => Block::Code(tb),
        "footnote" => Block::Footnote(tb),
        _ => Block::Para(tb),
    }
}

/// Heuristic classification for the *bare* dialect (granite emits no explicit
/// `<section_header>`): promote short ALL-CAPS or numbered lines to headings.
fn classify_bare(tb: TextBlock) -> Block {
    if let Some(level) = header_level(&tb.text) {
        Block::Heading { level, block: tb }
    } else {
        Block::Para(tb)
    }
}

/// Returns a heading level for a line that looks like a heading, else None.
fn header_level(t: &str) -> Option<u8> {
    let t = t.trim();
    let n = t.chars().count();
    if !(3..=75).contains(&n) {
        return None;
    }
    // "3.1.2 Title" → level = dotted-depth (capped 1..4)
    if let Some(first) = t.split_whitespace().next() {
        let core = first.trim_end_matches('.');
        if !core.is_empty() && core.chars().all(|c| c.is_ascii_digit() || c == '.') && core.contains(char::is_numeric) {
            let depth = core.split('.').filter(|p| !p.is_empty()).count();
            // require an actual title after the number
            if t.split_whitespace().count() >= 2 {
                return Some((depth as u8).clamp(1, 4));
            }
        }
    }
    // ALL-CAPS heading (no trailing period)
    let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
    if !letters.is_empty()
        && letters.iter().filter(|c| c.is_uppercase()).count() * 100 >= letters.len() * 85
        && !t.ends_with('.')
    {
        return Some(1);
    }
    None
}

/// Parse a `<picture>…</picture>` element. `i` points at the `picture` Open.
fn parse_picture(toks: &[Tok], mut i: usize) -> (Block, usize) {
    i += 1; // consume <picture>
    let bbox = take_locs(toks, &mut i);
    let mut class = None;
    let mut caption = None;
    while i < toks.len() {
        match &toks[i] {
            Tok::Close(name) if name == "picture" => {
                i += 1;
                break;
            }
            Tok::Open(name) if is_region(name) => {
                class = Some(name.clone());
                i += 1;
            }
            Tok::Open(name) if name == "caption" => {
                i += 1;
                let _ = take_locs(toks, &mut i);
                let c = take_text(toks, &mut i, Some("caption"));
                if !c.is_empty() {
                    caption = Some(c);
                }
            }
            Tok::Loc(_) => {
                let _ = take_locs(toks, &mut i);
            }
            Tok::Text(t) if t.trim().is_empty() => i += 1,
            Tok::Text(_) => {
                // free text inside picture → caption fallback
                let c = take_text(toks, &mut i, None);
                if caption.is_none() && !c.is_empty() {
                    caption = Some(c);
                }
            }
            _ => i += 1,
        }
    }
    (Block::Picture { bbox, class, caption }, i)
}

/// Parse an `<otsl>…</otsl>` table. `i` points at the `otsl` Open.
fn parse_otsl(toks: &[Tok], mut i: usize) -> (Table, usize) {
    i += 1; // consume <otsl>
    let bbox = take_locs(toks, &mut i);
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut caption = None;
    while i < toks.len() {
        match &toks[i] {
            Tok::Close(name) if name == "otsl" => {
                i += 1;
                break;
            }
            Tok::Open(name) if name == "caption" => {
                i += 1;
                let _ = take_locs(toks, &mut i);
                let c = take_text(toks, &mut i, Some("caption"));
                if !c.is_empty() {
                    caption = Some(c);
                }
            }
            Tok::Open(name) if name == "nl" => {
                rows.push(std::mem::take(&mut cur));
                i += 1;
            }
            Tok::Open(name) if name == "lcel" => {
                // left-looking: extend the previous cell's colspan
                if let Some(c) = cur.last_mut() {
                    c.colspan += 1;
                }
                i += 1;
            }
            Tok::Open(name) if name == "ucel" || name == "xcel" => {
                // up-looking: extend the rowspan of the cell directly above
                let col = cur.len();
                if let Some(prev) = rows.last_mut() {
                    if let Some(c) = prev.get_mut(col) {
                        c.rowspan += 1;
                    }
                }
                cur.push(Cell { text: String::new(), header: false, colspan: 1, rowspan: 1 });
                i += 1;
            }
            Tok::Open(name) if is_otsl_cell(name) => {
                let header = matches!(name.as_str(), "ched" | "rhed");
                let empty = name == "ecel";
                i += 1;
                let _ = take_locs(toks, &mut i);
                let text = if empty { String::new() } else { take_text(toks, &mut i, None) };
                cur.push(Cell { text, header, colspan: 1, rowspan: 1 });
            }
            Tok::Loc(_) => {
                let _ = take_locs(toks, &mut i);
            }
            _ => i += 1,
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    (Table { bbox, rows, caption }, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_single_line_loop() {
        // "!" ×6 (a real model failure mode) collapses to one; surrounding text survives.
        let mut dt = String::from("<loc_1><loc_1><loc_9><loc_9>ANEXO O\n");
        for _ in 0..6 {
            dt.push_str("<loc_1><loc_1><loc_9><loc_9>!\n");
        }
        dt.push_str("<loc_1><loc_1><loc_9><loc_9>fim");
        let out = collapse_loops(&dt);
        assert_eq!(out.matches(">!").count(), 1, "the ! loop should collapse to one");
        assert!(out.contains("ANEXO O") && out.contains("fim"));
    }

    #[test]
    fn collapse_multiline_cycle_keeps_legit_repeats() {
        // A 2-line block repeated 4× collapses to one cycle …
        let blk = "<loc_0><loc_0><loc_9><loc_9>Gabinete x\n<loc_0><loc_0><loc_9><loc_9>Assessoria y\n";
        let looped = blk.repeat(4);
        let out = collapse_loops(&looped);
        assert_eq!(out.matches("Gabinete x").count(), 1);
        // … but a line that recurs SEPARATED by other content is left intact (not a loop).
        let legit = "<loc_0><loc_0><loc_9><loc_9>Dayse Alfaya\n<loc_0><loc_0><loc_9><loc_9>De: outro email\n<loc_0><loc_0><loc_9><loc_9>Dayse Alfaya";
        assert_eq!(collapse_loops(legit).matches("Dayse Alfaya").count(), 2);
    }

    #[test]
    fn bare_text_and_picture() {
        // The dialect granite actually emits (no element wrappers).
        let dt = "<loc_69><loc_25><loc_187><loc_45><logo>\n\
                  <loc_72><loc_68><loc_432><loc_93>COMISSÃO SINDICANTE\n\
                  <loc_71><loc_143><loc_458><loc_448>Em 02 de outubro de 2019, às 11:30 horas.";
        let page = parse(dt);
        assert_eq!(page.blocks.len(), 3);
        assert!(matches!(page.blocks[0], Block::Picture { class: Some(ref c), .. } if c == "logo"));
        // ALL-CAPS short line -> heading
        assert!(matches!(page.blocks[1], Block::Heading { level: 1, .. }));
        assert!(matches!(page.blocks[2], Block::Para(_)));
    }

    #[test]
    fn wrapped_dialect_and_headings() {
        let dt = "<title><loc_10><loc_10><loc_400><loc_30>Attention Is All You Need</title>\
                  <section_header_level_1><loc_10><loc_40><loc_200><loc_55>3.1 Encoder and Decoder Stacks</section_header_level_1>\
                  <text><loc_10><loc_60><loc_400><loc_120>The encoder is composed of a stack.</text>";
        let page = parse(dt);
        assert_eq!(page.blocks.len(), 3);
        assert!(matches!(page.blocks[0], Block::Title(_)));
        match &page.blocks[1] {
            Block::Heading { level, block } => {
                assert_eq!(*level, 1);
                assert_eq!(block.text, "3.1 Encoder and Decoder Stacks");
            }
            _ => panic!("expected heading"),
        }
        assert!(matches!(page.blocks[2], Block::Para(_)));
    }

    #[test]
    fn numbered_header_level_from_depth() {
        // bare numbered lines map to depth
        let one = parse("<loc_1><loc_1><loc_9><loc_9>1 Introduction");
        assert!(matches!(one.blocks[0], Block::Heading { level: 1, .. }));
        let two = parse("<loc_1><loc_1><loc_9><loc_9>3.1 Encoder and Decoder Stacks");
        assert!(matches!(two.blocks[0], Block::Heading { level: 2, .. }));
    }

    #[test]
    fn otsl_table() {
        let dt = "<otsl><loc_0><loc_0><loc_500><loc_200>\
                  <ched>Year<ched>Value<nl>\
                  <fcel>2019<fcel>42<nl>\
                  <fcel>2020<fcel>43<nl></otsl>";
        let page = parse(dt);
        assert_eq!(page.blocks.len(), 1);
        match &page.blocks[0] {
            Block::Table(t) => {
                assert_eq!(t.rows.len(), 3);
                assert_eq!(t.rows[0].len(), 2);
                assert!(t.rows[0][0].header);
                assert_eq!(t.rows[0][0].text, "Year");
                assert_eq!(t.rows[1][1].text, "42");
            }
            _ => panic!("expected table"),
        }
    }

    #[test]
    fn otsl_colspan() {
        // a header spanning two columns: <ched>Title<lcel>
        let dt = "<otsl><ched>Spanned<lcel><nl><fcel>a<fcel>b<nl></otsl>";
        let page = parse(dt);
        match &page.blocks[0] {
            Block::Table(t) => {
                assert_eq!(t.rows[0][0].colspan, 2);
                assert_eq!(t.rows[0][0].text, "Spanned");
            }
            _ => panic!("expected table"),
        }
    }

    #[test]
    fn bbox_to_pdf_flips_y() {
        let b = BBox { l: 0.0, t: 0.0, r: 1.0, b: 1.0 };
        let r = b.to_pdf(612.0, 792.0);
        assert_eq!(r, [0.0, 0.0, 612.0, 792.0]);
        let top = BBox { l: 0.0, t: 0.0, r: 0.5, b: 0.1 };
        let pr = top.to_pdf(600.0, 800.0);
        // a box near the page top has a HIGH pdf-y (origin bottom-left)
        assert!(pr[1] > 700.0);
    }
}
