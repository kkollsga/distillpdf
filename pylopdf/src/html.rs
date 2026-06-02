//! "AI-ready" HTML conversion: thin, semantic HTML from a PDF.
//!
//! Per-page <section>; headings (font-size clustering), bold/italic (font flags),
//! lists, monospace/code, tables (positioned), inline embedded images. No CSS,
//! no classes, no script — minimal tags for LLM consumption.

use crate::extract::{self, PosTable};
use crate::img;
use crate::text::{self, Span};
use lopdf::{Document, ObjectId};

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

struct Run {
    text: String,
    bold: bool,
    italic: bool,
}

struct Line {
    y: f32,
    x0: f32,
    size: f32,
    mono: bool,
    mono_w: usize,
    tot_w: usize,
    runs: Vec<Run>,
}

impl Line {
    fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
    fn words(&self) -> usize {
        self.text().split_whitespace().count()
    }
}

fn span_width(s: &Span) -> f32 {
    if s.width > 0.1 {
        s.width
    } else {
        s.text.chars().count() as f32 * s.size * 0.5
    }
}

/// Group spans into visual lines, each with style runs (merged by bold/italic).
/// Explicit whitespace spans are kept (they carry real spaces); plus a gap
/// heuristic re-derives spaces when they're encoded purely as positioning.
fn lines_of(mut spans: Vec<Span>) -> Vec<Line> {
    spans.retain(|s| !s.text.is_empty()); // keep ' ' spans (real spaces)
    if spans.is_empty() {
        return Vec::new();
    }
    let band = (spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32 * 0.6).max(2.0);
    spans.sort_by(|p, q| {
        let (bp, bq) = ((p.y / band).round(), (q.y / band).round());
        bq.partial_cmp(&bp)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(p.x.partial_cmp(&q.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut lines: Vec<Line> = Vec::new();
    let mut cur_band: Option<f32> = None;
    let mut prev_end = 0.0f32;
    for s in spans {
        let b = (s.y / band).round();
        let whitespace = s.text.trim().is_empty();

        if !whitespace {
            let band_changed = cur_band.map_or(true, |cb| (cb - b).abs() >= 0.5);
            // Break on a huge horizontal gap (a column gutter) so left/right
            // columns at the same y don't merge — split occurs between words,
            // never within one, so no fragmentation.
            let huge_gap = cur_band.is_some() && (s.x - prev_end) > (s.size * 4.0).max(36.0);
            if band_changed || huge_gap {
                lines.push(Line { y: s.y, x0: s.x, size: s.size, mono: false, mono_w: 0, tot_w: 0, runs: Vec::new() });
                cur_band = Some(b);
                prev_end = s.x;
            }
        }
        let line = match lines.last_mut() {
            Some(l) => l,
            None => continue, // leading whitespace before any line
        };

        if whitespace {
            // explicit space: ensure the current run ends with a single space
            if let Some(r) = line.runs.last_mut() {
                if !r.text.ends_with(' ') {
                    r.text.push(' ');
                }
            }
            prev_end = s.x + span_width(&s);
            continue;
        }

        let gap = s.x - prev_end;
        let need_space = gap > s.size * 0.28;
        prev_end = s.x + span_width(&s);
        line.size = line.size.max(s.size);
        let nchars = s.text.chars().count();
        line.tot_w += nchars;
        if s.mono {
            line.mono_w += nchars;
        }
        match line.runs.last_mut() {
            Some(r) if r.bold == s.bold && r.italic == s.italic => {
                if need_space && !r.text.ends_with(' ') {
                    r.text.push(' ');
                }
                r.text.push_str(&s.text);
            }
            _ => {
                let mut t = String::new();
                if need_space && !line.runs.is_empty() {
                    t.push(' ');
                }
                t.push_str(&s.text);
                line.runs.push(Run { text: t, bold: s.bold, italic: s.italic });
            }
        }
    }
    for l in &mut lines {
        l.mono = l.tot_w > 0 && l.mono_w * 2 >= l.tot_w;
    }
    lines
}

fn render_runs(runs: &[Run]) -> String {
    let mut o = String::new();
    for r in runs {
        let t = esc(r.text.trim());
        if t.is_empty() {
            continue;
        }
        match (r.bold, r.italic) {
            (true, true) => o.push_str(&format!("<b><i>{t}</i></b>")),
            (true, false) => o.push_str(&format!("<b>{t}</b>")),
            (false, true) => o.push_str(&format!("<i>{t}</i>")),
            (false, false) => o.push_str(&t),
        }
        o.push(' ');
    }
    o.trim().to_string()
}

const BULLETS: &[char] = &['•', '◦', '▪', '‣', '·', '−', '–', '*'];

fn list_kind(text: &str) -> Option<bool> {
    let t = text.trim_start();
    if t.starts_with(|c| BULLETS.contains(&c)) {
        return Some(false); // unordered
    }
    // "1." / "1)" / "12."
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &t[digits.len()..];
        if rest.starts_with('.') || rest.starts_with(')') {
            return Some(true); // ordered
        }
    }
    None
}

fn strip_marker(text: &str) -> String {
    let t = text.trim_start();
    let t = t.trim_start_matches(|c| BULLETS.contains(&c));
    let t = t.trim_start();
    // strip leading "N." / "N)"
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &t[digits.len()..];
        if let Some(r) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
            return r.trim_start().to_string();
        }
    }
    t.to_string()
}

fn heading_level(size: f32, body: f32) -> Option<u8> {
    let r = size / body.max(1.0);
    if r >= 1.7 {
        Some(1)
    } else if r >= 1.35 {
        Some(2)
    } else if r >= 1.18 {
        Some(3)
    } else {
        None
    }
}

fn table_html(t: &PosTable) -> String {
    let mut o = String::from("<table>");
    for (ri, row) in t.grid.iter().enumerate() {
        o.push_str("<tr>");
        let tag = if ri == 0 { "th" } else { "td" };
        for cell in row {
            o.push_str(&format!("<{tag}>{}</{tag}>", esc(cell.trim())));
        }
        o.push_str("</tr>");
    }
    o.push_str("</table>");
    o
}

/// Emit a run of consecutive text lines as headings / paragraphs / lists / code.
fn emit_lines(lines: &[&Line], body: f32, out: &mut String) {
    let mut i = 0;
    while i < lines.len() {
        let ln = lines[i];
        let txt = ln.text();
        // heading
        if ln.words() <= 14 && heading_level(ln.size, body).is_some() && !txt.trim().is_empty() {
            let lvl = heading_level(ln.size, body).unwrap();
            out.push_str(&format!("<h{lvl}>{}</h{lvl}>", render_runs(&ln.runs)));
            i += 1;
            continue;
        }
        // list
        if list_kind(&txt).is_some() {
            let ordered = list_kind(&txt).unwrap();
            let tag = if ordered { "ol" } else { "ul" };
            out.push_str(&format!("<{tag}>"));
            while i < lines.len() && list_kind(&lines[i].text()).is_some() {
                out.push_str(&format!("<li>{}</li>", esc(&strip_marker(&lines[i].text()))));
                i += 1;
            }
            out.push_str(&format!("</{tag}>"));
            continue;
        }
        // code / monospace block
        if ln.mono {
            out.push_str("<pre><code>");
            while i < lines.len() && lines[i].mono && list_kind(&lines[i].text()).is_none() {
                out.push_str(&esc(&lines[i].text()));
                out.push('\n');
                i += 1;
            }
            out.push_str("</code></pre>");
            continue;
        }
        // paragraph: gather consecutive normal lines, join with hyphenation handling
        let mut para = String::new();
        while i < lines.len() {
            let l = lines[i];
            if l.mono
                || list_kind(&l.text()).is_some()
                || (l.words() <= 14 && heading_level(l.size, body).is_some())
            {
                break;
            }
            let piece = render_runs(&l.runs);
            if para.is_empty() {
                para = piece;
            } else if para.ends_with('-') {
                para.pop();
                para.push_str(&piece);
            } else {
                para.push(' ');
                para.push_str(&piece);
            }
            i += 1;
        }
        if !para.trim().is_empty() {
            out.push_str(&format!("<p>{}</p>", para.trim()));
        }
    }
}

pub fn to_html(doc: &Document, raw: &[u8]) -> String {
    let pages = doc.get_pages();

    // Document-wide body font size = most common rounded span size.
    let mut hist: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
    let mut page_spans: Vec<(u32, ObjectId, Vec<Span>)> = Vec::new();
    for (&pno, &pid) in &pages {
        let spans = text::extract_spans(doc, pid, raw);
        for s in &spans {
            *hist.entry(s.size.round() as i32).or_insert(0) += 1;
        }
        page_spans.push((pno, pid, spans));
    }
    page_spans.sort_by_key(|(pno, _, _)| *pno);
    let body = hist.iter().max_by_key(|(_, c)| **c).map(|(s, _)| *s as f32).unwrap_or(10.0);

    let mut out = String::from("<!doctype html>\n<html>\n<body>\n");
    for (pno, _pid, spans) in &page_spans {
        out.push_str(&format!("<section data-page=\"{pno}\">\n"));
        let tables = extract::detect_tables_pos(spans);
        let lines = lines_of(spans.iter().map(clone_span).collect());

        // Mark lines consumed by a table (within its y-range).
        let in_table = |y: f32| {
            tables
                .iter()
                .any(|t| y <= t.y_top + body && y >= t.y_bottom - body)
        };

        // Ordered items by y (top -> bottom): text lines + tables + images.
        enum Item<'a> {
            L(&'a Line),
            T(&'a PosTable),
            Img(&'a str),
        }
        let images = img::positioned_images(doc, *_pid);
        let mut items: Vec<(f32, Item)> = Vec::new();
        for l in &lines {
            if !in_table(l.y) {
                items.push((l.y, Item::L(l)));
            }
        }
        for t in &tables {
            items.push((t.y_top, Item::T(t)));
        }
        for (y, uri) in &images {
            items.push((*y, Item::Img(uri)));
        }
        items.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Emit, grouping consecutive lines into text blocks.
        let mut run: Vec<&Line> = Vec::new();
        let mut flush = |run: &mut Vec<&Line>, out: &mut String| {
            if !run.is_empty() {
                emit_lines(run, body, out);
                run.clear();
            }
        };
        for (_, it) in &items {
            match it {
                Item::L(l) => run.push(l),
                Item::T(t) => {
                    flush(&mut run, &mut out);
                    out.push_str(&table_html(t));
                }
                Item::Img(uri) => {
                    flush(&mut run, &mut out);
                    out.push_str(&format!("<img src=\"{uri}\" />"));
                }
            }
        }
        flush(&mut run, &mut out);
        out.push_str("\n</section>\n");
    }
    out.push_str("</body>\n</html>\n");
    out
}

fn clone_span(s: &Span) -> Span {
    Span {
        x: s.x,
        y: s.y,
        size: s.size,
        width: s.width,
        text: s.text.clone(),
        bold: s.bold,
        italic: s.italic,
        mono: s.mono,
    }
}

