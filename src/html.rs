//! "AI-ready" HTML conversion: thin, semantic HTML from a PDF.
//!
//! Per-page <section>; headings (font-size clustering), bold/italic (font flags),
//! lists, monospace/code, tables (positioned), inline embedded images. No CSS,
//! no classes, no script — minimal tags for LLM consumption.

use crate::extract::{self, PosTable};
use crate::img;
use crate::links;
use crate::text::{self, Span};
use crate::vector;
use lopdf::{Document, ObjectId};

/// A link's rectangle (PDF user space) plus its resolved href, for hit-testing
/// text spans during HTML emission. Internal links point at `#page-N`.
struct LinkBox {
    rect: [f32; 4],
    href: String,
}

/// A PDF named-destination name (e.g. "cite.devlin2018", "section.3.1") → a valid,
/// stable HTML id/fragment: keep [A-Za-z0-9._-], map anything else to '-'. Used for
/// both the in-text link href (`#slug`) and the anchor `id` at the target, so a
/// citation resolves to its exact reference/figure/equation instead of a page.
fn slug(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect()
}

/// href of the link whose rectangle contains point `(x, y)`, if any.
fn href_at(x: f32, y: f32, links: &[LinkBox]) -> Option<String> {
    links
        .iter()
        .find(|l| x >= l.rect[0] && x <= l.rect[2] && y >= l.rect[1] && y <= l.rect[3])
        .map(|l| l.href.clone())
}

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
    href: Option<String>,
    script: i8, // 0 = baseline, 1 = superscript (<sup>), -1 = subscript (<sub>)
}

struct Line {
    y: f32,
    x0: f32,
    x1: f32,
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
fn lines_of(mut spans: Vec<Span>, links: &[LinkBox]) -> Vec<Line> {
    // Rotated text (a 90° axis title, a sidebar id) is never part of the body reading
    // order — it's handled as a figure label (or dropped). Keep it out of the lines.
    spans.retain(|s| s.angle.abs() < 0.01);
    spans.retain(|s| !s.text.is_empty()); // keep ' ' spans (real spaces)
    if spans.is_empty() {
        return Vec::new();
    }
    let avg = spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32;
    let band = (avg * 0.6).max(2.0);
    // A sub/superscript glyph sits a few points off the baseline; left as-is its raised
    // box floats into the row ABOVE and xy_cut reads it out of order (orphaning a lone
    // "2" at the start of the next line). For ORDERING only, snap each small offset
    // glyph onto the baseline of the adjacent base-size glyph it scripts, so it stays
    // in its own row right beside that glyph. Its sup/sub polarity is still classified
    // from the ORIGINAL y in the assembly loop below.
    let base_sz = avg * 0.85;
    let order_y: Vec<f32> = (0..spans.len())
        .map(|i| {
            let s = &spans[i];
            if s.size >= base_sz {
                return s.y;
            }
            // Only re-base an ISOLATED script (a lone footnote marker / exponent among
            // base-size text). A small glyph sitting among OTHER small glyphs is part of
            // a dense math run (stacked sub/superscripts, an inline formula); leaving its
            // order alone avoids reshuffling equation fragments into spurious lines/lists.
            let in_cluster = spans.iter().enumerate().any(|(j, t)| {
                j != i && t.size < base_sz && (t.x - s.x).abs() < avg * 3.0 && (t.y - s.y).abs() < band * 1.5
            });
            if in_cluster {
                return s.y;
            }
            let mut best: Option<(f32, f32)> = None; // (|dx|, base baseline y)
            for (j, t) in spans.iter().enumerate() {
                if i == j || t.size < base_sz {
                    continue;
                }
                let dy = (s.y - t.y).abs();
                if dy <= t.size * 0.05 || dy >= t.size * 0.9 {
                    continue; // not in the sub/superscript offset window
                }
                let (s_end, t_end) = (s.x + span_width(s), t.x + span_width(t));
                let dx = if s.x >= t.x { s.x - t_end } else { t.x - s_end }; // gap to base
                if dx > t.size * 1.2 {
                    continue; // not horizontally adjacent to this base glyph
                }
                let adx = dx.abs();
                if best.map_or(true, |(bd, _)| adx < bd) {
                    best = Some((adx, t.y));
                }
            }
            best.map_or(s.y, |(_, by)| by)
        })
        .collect();
    // Order spans column-aware (same XY-cut as the text path) so a visual line is
    // never assembled across a column gutter — left and right columns become
    // separate lines, splitting only between words, never within one.
    let boxes: Vec<text::BBox> = spans
        .iter()
        .enumerate()
        .map(|(i, s)| (s.x, s.x + span_width(s), order_y[i], order_y[i] + s.size.max(1.0)))
        .collect();
    let order = text::xy_cut_order(&boxes, avg.max(1.0));
    let mut lines: Vec<Line> = Vec::new();
    let mut cur_band: Option<f32> = None;
    let mut prev_end = 0.0f32;
    for &si in &order {
        let s = &spans[si];
        let b = (s.y / band).round();
        let whitespace = s.text.trim().is_empty();

        // Combining diacritic drawn as a separate glyph OVERLAPPING its base (the
        // accent x starts before the base's right edge): compose it onto the
        // preceding character instead of orphaning it as its own span/<p>.
        if !whitespace && cur_band.is_some() && s.x < prev_end - s.size * 0.2 {
            if let Some(cm) = mark_to_combining(&s.text) {
                if let Some(r) = lines.last_mut().and_then(|l| l.runs.last_mut()) {
                    if !r.text.trim_end().is_empty() {
                        r.text.push(cm);
                        prev_end = prev_end.max(s.x + span_width(s));
                        continue;
                    }
                }
            }
        }

        // Sub/superscript: a SMALL glyph whose baseline is offset from the current
        // line's, sitting horizontally adjacent to it (e.g. R^H, T_i, x², footnote
        // markers). It must NOT start a new line (which would orphan it as a lone
        // <p>) — attach it to the current line, marked sup (raised) or sub (lowered).
        let mut script: i8 = 0;
        if !whitespace {
            if let Some(l) = lines.last() {
                let dy = s.y - l.y; // +raised (sup), -lowered (sub)
                let adj = (s.x - prev_end).abs() < l.size * 1.5;
                if s.size < l.size * 0.85 && adj && dy.abs() > l.size * 0.08 && dy.abs() < l.size * 0.7 {
                    script = if dy > 0.0 { 1 } else { -1 };
                }
            }
        }
        if !whitespace {
            let band_changed = cur_band.map_or(true, |cb| (cb - b).abs() >= 0.5);
            // Break on a huge horizontal gap (a column gutter) so left/right
            // columns at the same y don't merge — split occurs between words,
            // never within one, so no fragmentation.
            let huge_gap = cur_band.is_some() && (s.x - prev_end) > (s.size * 4.0).max(36.0);
            // A script glyph stays on the current line (don't break, don't move the
            // band) so the following baseline glyph rejoins the same line.
            if (band_changed || huge_gap) && script == 0 {
                lines.push(Line { y: s.y, x0: s.x, x1: s.x, size: s.size, mono: false, mono_w: 0, tot_w: 0, runs: Vec::new() });
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
            prev_end = s.x + span_width(s);
            continue;
        }

        let gap = s.x - prev_end;
        // word-level spans: a gap below a space width is a join; but a punctuation
        // glyph's side bearing also looks like a gap, so apply typographic binding.
        let prev_txt = line.runs.last().map(|r| r.text.trim_end()).unwrap_or("");
        let mut pit = prev_txt.chars().rev();
        let (prev_c, prev_c2) = (pit.next(), pit.next());
        let need_space = gap > s.size * 0.2 && !binds(prev_c, prev_c2, s.text.chars().next());
        let end = s.x + span_width(s);
        // Link hit-test: a span belongs to a link if its center is in the rect.
        let href = href_at((s.x + end) * 0.5, s.y + s.size * 0.5, links);
        prev_end = end;
        line.size = line.size.max(s.size);
        line.x1 = line.x1.max(prev_end);
        let nchars = s.text.chars().count();
        line.tot_w += nchars;
        if s.mono {
            line.mono_w += nchars;
        }
        match line.runs.last_mut() {
            Some(r) if r.bold == s.bold && r.italic == s.italic && r.href == href && r.script == script => {
                if need_space && !r.text.ends_with(' ') {
                    r.text.push(' ');
                }
                r.text.push_str(&s.text);
            }
            _ => {
                let mut t = String::new();
                if need_space && !line.runs.is_empty() && script == 0 {
                    t.push(' ');
                }
                t.push_str(&s.text);
                line.runs.push(Run { text: t, bold: s.bold, italic: s.italic, href, script });
            }
        }
    }
    for l in &mut lines {
        l.mono = l.tot_w > 0 && l.mono_w * 2 >= l.tot_w;
    }
    lines
}

/// Typographic binding: should the inter-span x-gap NOT become a space, because
/// the two characters bind together? Word-level spans split punctuation onto its
/// own positioned glyph, whose side bearing looks like a word gap; without this we
/// emit `( 2015 )`, `1 , 380`, `i.e .` instead of `(2015)`, `1,380`, `i.e.`.
/// Rules are general (not per-document): never a space *before* closing/clitic
/// punctuation, never *after* an opening bracket, and none inside a numeric
/// separator (digit , . – digit).
fn binds(prev: Option<char>, prev2: Option<char>, next: Option<char>) -> bool {
    let (p, n) = match (prev, next) {
        (Some(p), Some(n)) => (p, n),
        _ => return false,
    };
    if ")]},.;:!?%".contains(n) {
        return true; // no space before closing/trailing punctuation
    }
    if "([{".contains(p) {
        return true; // no space after an opening bracket
    }
    // numeric separator: 1,380 · 3.14 · 27–31 · 1:3 ratio · 12:30 (digit both sides
    // of , . - – : /)
    if matches!(p, ',' | '.' | '-' | '\u{2013}' | ':' | '/') && n.is_ascii_digit() && prev2.is_some_and(|c| c.is_ascii_digit()) {
        return true;
    }
    false
}

/// A standalone diacritic glyph → its Unicode combining mark. Some PDFs draw an
/// accent as a separate glyph positioned over/under the base letter (Romanian ș/ț
/// comma-below; math x̄, ã, etc.) instead of a precomposed character. Composing it
/// onto the base (base + combining mark) renders correctly and stops the accent
/// from being orphaned as its own span/<p>.
fn mark_to_combining(s: &str) -> Option<char> {
    let mut it = s.chars();
    let c = it.next()?;
    if it.next().is_some() {
        return None; // single glyph only
    }
    Some(match c {
        ',' => '\u{0326}',                // comma below (ș, ț)
        '¸' => '\u{0327}',                // cedilla
        '¯' | '\u{02C9}' | '‾' => '\u{0304}', // macron / overline
        '~' | '\u{02DC}' => '\u{0303}',   // tilde
        '´' | '\u{02CA}' => '\u{0301}',   // acute
        '`' | '\u{02CB}' => '\u{0300}',   // grave
        '¨' => '\u{0308}',                // diaeresis
        'ˆ' => '\u{0302}',                // circumflex
        'ˇ' => '\u{030C}',                // caron
        '˚' | '°' => '\u{030A}',          // ring above
        '˙' => '\u{0307}',                // dot above
        _ => return None,
    })
}

// `close_a!` resets `cur_href = None` so a later run starts a fresh <a>; that reset is
// read on the next loop iteration in the unmatched-link path, but is (correctly) dead in
// the expansions that immediately reopen a link or fall at end-of-fn — hence the allow.
#[allow(unused_assignments)]
fn render_runs(runs: &[Run]) -> String {
    let mut o = String::new();
    // Track the href of the currently-open <a> so consecutive runs that share a
    // link (a citation/URL split across styled glyph-runs, e.g. "Rad"+"ford") stay
    // in ONE <a> instead of becoming adjacent split links.
    let mut cur_href: Option<String> = None;
    macro_rules! close_a {
        () => {
            if cur_href.is_some() {
                while o.ends_with(' ') {
                    o.pop();
                }
                o.push_str("</a> ");
                cur_href = None;
            }
        };
    }
    for r in runs {
        let t = esc(r.text.trim());
        if t.is_empty() {
            continue;
        }
        let styled = match (r.bold, r.italic) {
            (true, true) => format!("<b><i>{t}</i></b>"),
            (true, false) => format!("<b>{t}</b>"),
            (false, true) => format!("<i>{t}</i>"),
            (false, false) => t,
        };
        let styled = match r.script {
            1 => format!("<sup>{styled}</sup>"),
            -1 => format!("<sub>{styled}</sub>"),
            _ => styled,
        };
        match &r.href {
            Some(h) => {
                if cur_href.as_deref() != Some(h.as_str()) {
                    close_a!();
                    o.push_str(&format!("<a href=\"{}\">", esc(h)));
                    cur_href = Some(h.clone());
                }
            }
            None => close_a!(),
        }
        o.push_str(&styled);
        o.push(' ');
    }
    close_a!();
    o.trim().to_string()
}

// Unambiguous bullet glyphs. Includes U+0095 / U+0085: some embedded fonts map
// their LaTeX-itemize bullet to those C1 control code points.
const BULLETS: &[char] = &['•', '◦', '▪', '‣', '\u{95}', '\u{85}'];
// Math-ambiguous markers (minus, en-dash, middot, asterisk) that ALSO open display
// equations ("− 1", "· x", "∗ footnote"). Treated as a bullet only when followed by
// real prose, never a lone math token — this is what stops equation fragments from
// becoming one-item lists.
const WEAK_BULLETS: &[char] = &['·', '−', '–', '*', '\u{2013}'];

fn list_kind(text: &str) -> Option<bool> {
    let t = text.trim_start();
    if t.starts_with(|c| BULLETS.contains(&c)) {
        return Some(false); // unordered
    }
    if let Some(c) = t.chars().next() {
        if WEAK_BULLETS.contains(&c) {
            let rest = t[c.len_utf8()..].trim_start();
            let words = rest.split_whitespace().count();
            let alpha = rest.chars().filter(|c| c.is_alphabetic()).count();
            // require a space after the marker and a real prose run (≥3 words, ≥6
            // letters) — rejects "− 1", "· x", "∗ 2" math fragments.
            if t[c.len_utf8()..].starts_with(char::is_whitespace) && words >= 3 && alpha >= 6 {
                return Some(false);
            }
        }
    }
    // "1." / "1)" / "12)" — ordered list. Guards:
    //  - 1-2 digit marker only: a 4-digit "2014)" is a wrapped citation year
    //    ("…Pennington et al., 2014) methods…"), not a list item.
    //  - NOT a multi-level section number like "3.1" (digit right after the dot).
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && digits.len() <= 2 {
        let rest = &t[digits.len()..];
        if let Some(after) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
            if !after.starts_with(|c: char| c.is_ascii_digit()) {
                return Some(true); // ordered
            }
        }
    }
    None
}

fn strip_marker(text: &str) -> String {
    let t = text.trim_start();
    let t = t.trim_start_matches(|c| BULLETS.contains(&c) || WEAK_BULLETS.contains(&c));
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

/// Level implied by a leading section number/letter: "2"→1, "2.1"→2, "2.1.3"→3,
/// appendix "A"→1, "A.2"→2. Returns the depth (capped at 3) when the text is
/// `<number> <Title…>`; `None` otherwise (rejects bare decimals like "28.4").
/// The leading 1–2 digit number of an ordered-list marker ("12. …" / "3) …" → 12 / 3),
/// if the line is one. A multi-level section number ("3.1") or a periodless section
/// heading ("1 Introduction") is NOT an ordered marker.
fn ordered_num(text: &str) -> Option<u32> {
    let t = text.trim_start();
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = &t[digits.len()..];
    let after = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    if after.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// True when `lines[i]` is part of a tight ENUMERATED run — three or more ordered
/// items with consecutive numbers (…n−1, n, n+1…) inside a small window. Such a run
/// is a list ("1. … 2. … 3. …", possibly with short continuation lines between
/// items), NOT a sequence of section headings (whose numbers are spread far apart by
/// section bodies). Used to keep an enumerated claims/contributions list out of the
/// per-line heading promotion.

/// True when `lines[i]` belongs to a COLON-INTRODUCED enumerated list — a consecutive
/// numbered run (≥2 items) whose FIRST item is immediately preceded by a colon-
/// terminated lead-in line ("…we investigate the following questions:" → "1. Question:
/// … / 2. Question: …"). That lead-in colon is the structural marker of a list the
/// prose announces, so its items are NOT section headings even though each carries a
/// colon label (which `in_enumerated_run` deliberately treats as a heading, and which
/// needs ≥3 items it doesn't reach here). Empirically safe: across the corpus NO real
/// numbered section heading is preceded by a colon-ending line, whereas this catches
/// BERT's C.1 Q&A list. Distinct from `in_enumerated_run` (which excludes colon items).
fn colon_introduced_list(lines: &[&Line], i: usize) -> bool {
    let n = match ordered_num(&lines[i].text()) {
        Some(n) => n,
        None => return false,
    };
    let colon_above = |idx: usize| idx > 0 && lines[idx - 1].text().trim_end().ends_with(':');
    // The item directly under a colon-terminated lead-in ("…the following questions:")
    // is the first entry of a prose-announced list (also covers items split across
    // regions by an interleaved table, where only the immediate predecessor is in view).
    if colon_above(i) {
        return true;
    }
    // A LATER item (2., 3., …) of that same run: walk the consecutive number streak back
    // to the FIRST item and test ITS lead-in, so "2. Question…" is also a list entry when
    // "1. Question…" sits under the colon intro.
    let lo = i.saturating_sub(24);
    let hi = (i + 24).min(lines.len());
    let mut idx_of: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for j in lo..hi {
        if let Some(m) = ordered_num(&lines[j].text()) {
            idx_of.entry(m).or_insert(j);
        }
    }
    let mut first = n;
    while first > 1 && idx_of.contains_key(&(first - 1)) {
        first -= 1;
    }
    first < n && idx_of.get(&first).is_some_and(|&fi| colon_above(fi))
}

fn in_enumerated_run(lines: &[&Line], i: usize) -> bool {
    let n = match ordered_num(&lines[i].text()) {
        Some(n) => n,
        None => return false,
    };
    // An enumerated item is NOT a demotable prose claim — it is a labelled subsection
    // heading — when it carries a run-in label: a ':' early in the line ("1) Forward
    // diffusion process: …") or a BOLD lead ("1. **Two formal corpora, one
    // underpowered.** …"). The heading path (bold-lead split) keeps those; only a
    // plain, non-bold, colon-less numbered run ("1. In seven independent substrates…")
    // is the claim/contribution list we demote.
    let heading_item = |idx: usize| {
        let l = lines[idx];
        l.text().chars().take(45).any(|c| c == ':')
            || l.runs.iter().any(|r| !r.text.trim().is_empty() && r.bold)
    };
    if heading_item(i) {
        return false;
    }
    let lo = i.saturating_sub(8);
    let hi = (i + 16).min(lines.len());
    let mut present = std::collections::HashSet::new();
    for j in lo..hi {
        if heading_item(j) {
            continue;
        }
        if let Some(m) = ordered_num(&lines[j].text()) {
            present.insert(m);
        }
    }
    // longest consecutive streak through n
    let mut len = 1u32;
    let mut k = n;
    while k > 0 && present.contains(&(k - 1)) {
        len += 1;
        k -= 1;
    }
    let mut k = n;
    while present.contains(&(k + 1)) {
        len += 1;
        k += 1;
    }
    len >= 3
}

fn numbered_level(text: &str) -> Option<u8> {
    let b = text.trim_start().as_bytes();
    let mut i = 0usize;
    let mut comps = 0u8;
    loop {
        let start = i;
        if i < b.len() && b[i].is_ascii_digit() {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        } else if comps == 0
            && i < b.len()
            && b[i].is_ascii_uppercase()
            && (i + 1 >= b.len() || !(b[i + 1] as char).is_ascii_alphabetic())
        {
            i += 1; // single capital = appendix label (A, B, …), only as first component
        } else {
            break;
        }
        if i == start {
            break;
        }
        comps += 1;
        if i < b.len() && b[i] == b'.' {
            if i + 1 < b.len() && b[i + 1].is_ascii_digit() {
                i += 1; // "." between components
                continue;
            }
            i += 1; // trailing period after the number
        }
        break;
    }
    if comps == 0 {
        return None;
    }
    let mut j = i;
    while j < b.len() && b[j] == b' ' {
        j += 1;
    }
    // require a real space then an alphabetic title (so "3.2" / "28.4" don't match)
    if j == i || j >= b.len() || !(b[j] as char).is_alphabetic() {
        return None;
    }
    Some(comps.min(3))
}

/// IEEE/physics-style Roman-numeral section: "I. INTRODUCTION", "II. Methods",
/// "III. ...". Requires uppercase Roman digits + a period + a capitalised title
/// (the period avoids matching the pronoun "I" or stray "V"/"X" variables).
/// Mapped to level 1 (Roman papers use letter subsections, handled by size).
fn roman_section(text: &str) -> Option<u8> {
    let b = text.trim_start().as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b'I' | b'V' | b'X' | b'L' | b'C' | b'D' | b'M') {
        i += 1;
    }
    if i == 0 || i > 5 || b.get(i) != Some(&b'.') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() && b[j] == b' ' {
        j += 1;
    }
    if j == i + 1 || !b.get(j).map_or(false, |c| (*c as char).is_alphabetic()) {
        return None;
    }
    Some(1)
}

/// True if the text contains a 19xx/20xx 4-digit year — a reference-list tell
/// (section titles essentially never carry a year).
fn has_year(t: &str) -> bool {
    t.as_bytes().windows(4).any(|w| {
        ((w[0] == b'1' && w[1] == b'9') || (w[0] == b'2' && w[1] == b'0'))
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
    })
}

/// Count author-initial tokens (" X." — capital then period at a word start).
/// Two or more signals a citation / running-head author list, not a section.
fn initials_count(t: &str) -> usize {
    let b = t.as_bytes();
    (0..b.len())
        .filter(|&i| {
            b[i].is_ascii_uppercase()
                && b.get(i + 1) == Some(&b'.')
                && (i == 0 || b[i - 1] == b' ')
        })
        .count()
}

fn size_level(ratio: f32) -> u8 {
    if ratio >= 1.5 {
        1
    } else if ratio >= 1.2 {
        2
    } else {
        3
    }
}

const TOP_HEADS: &[&str] = &[
    "abstract", "references", "acknowledgements", "acknowledgments",
    "introduction", "conclusion", "conclusions", "appendix",
];

// Theorem-environment labels: bold leads, not section headers. "denition" /
// "proposition" etc. cover the fi-ligature-dropped spelling some PDFs emit.
const THEOREM_ENV: &[&str] = &[
    "theorem", "lemma", "definition", "denition", "corollary", "proposition",
    "remark", "example", "proof", "claim", "question", "questions", "notation",
    "conjecture", "observation", "fact", "case", "step", "assumption", "assumptions",
    "algorithm", "problem", "require", "input", "output", "hypothesis", "property",
    "figure", "fig", "table", "tab", "listing", "equation", "eq",
];

/// A line that looks like a bibliography/reference entry, an author/affiliation
/// line, a list item, or a prose sentence — NOT a section heading. Section titles
/// are short comma-free phrases with no trailing period and no author markers.
/// Used to gate the full-line heading paths (a bold run-in *lead* is unaffected —
/// only its short prefix is promoted).
fn looks_like_reference(s: &str) -> bool {
    let commas = s.matches(',').count();
    if commas >= 2 || s.ends_with('.') {
        return true;
    }
    if has_year(s) && commas >= 1 {
        return true;
    }
    // A mid-line author initial ("Levoy, M." / "Michael R. Fellows"): an uppercase
    // letter + '.' that is NOT the first token. A leading "I."/"A." is a roman /
    // section label, not an author initial, so require a non-space token before it.
    let b = s.as_bytes();
    for i in 1..b.len() {
        if b[i].is_ascii_uppercase() && i + 1 < b.len() && b[i + 1] == b'.' && b[i - 1] == b' ' && s[..i - 1].trim_end().chars().next_back().is_some_and(|c| !c.is_whitespace()) {
            return true;
        }
    }
    false
}

/// Paragraph-aware header detection. A header is a numbered section line, a
/// canonical top-level name, a standalone bold/larger line, or a **bold run-in
/// lead** at the top of a paragraph. Returns `(level, n_leading_runs)`: when
/// `n_leading_runs < line.runs.len()` the header is a run-in lead and the
/// remaining runs continue as that paragraph's body. Level comes from the
/// section number when present, else from font size.
fn detect_header(line: &Line, body: f32) -> Option<(u8, usize)> {
    let txt = line.text();
    let trimmed = txt.trim();
    if trimmed.is_empty() {
        return None;
    }
    let words = trimmed.split_whitespace().count();
    let ratio = line.size / body.max(1.0);
    let nonspace: Vec<&Run> = line.runs.iter().filter(|r| !r.text.trim().is_empty()).collect();
    if nonspace.is_empty() {
        return None;
    }
    let all_bold = nonspace.iter().all(|r| r.bold);
    let first_bold = nonspace[0].bold;
    let low = trimmed.to_lowercase();

    // Not headers: pure-number/symbol lines (bold table cells like "86.6 86.3"),
    // the arXiv sidebar id, running page heads / reference-list entries (author
    // initials "J. Cui, H. Yan", a publication year, or an "et al." author list).
    if trimmed.chars().filter(|c| c.is_alphabetic()).count() < 2
        || low.contains("et al")
        || low.starts_with("arxiv")
        || has_year(trimmed)
        || initials_count(trimmed) >= 2
    {
        return None;
    }

    // Display equations ("T ∆² ≤ Õ(H d (β+Γ)),", "V̄(R) ≡ Ē(R) − 2M,") are sometimes
    // bold/large and get mistaken for headings. They carry a relational/assignment
    // operator and almost no real words — a heading is words, not a relation.
    let real_words = trimmed.split_whitespace().filter(|w| w.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 2).count();
    if real_words <= 1 && trimmed.chars().any(|c| matches!(c, '=' | '≤' | '≥' | '≡' | '≈' | '←' | '→' | '∝' | '≪' | '≫' | '∑' | '∈')) {
        return None;
    }

    // Theorem-environment labels ("Theorem 1.1.", "Lemma 2.3", "Definition",
    // "Proof.", "Remark 2.4") are bold leads, not section headers — exclude them.
    let first_word: String = trimmed.split(|c: char| !c.is_alphabetic()).next().unwrap_or("").to_lowercase();
    if THEOREM_ENV.contains(&first_word.as_str()) {
        let rest = trimmed[first_word.len()..].trim_start();
        if rest.is_empty()
            || rest.starts_with(|c: char| c.is_ascii_digit())
            || trimmed.ends_with(['.', ':'])
        {
            return None;
        }
    }

    // 1. Numbered section header — level from numbering depth. The "N.N Title"
    //    shape (number, space, capitalised title with a real word) is the signal;
    //    no bold needed (many heading faces aren't flagged bold). This also rejects
    //    inline math like "1 N".
    if words <= 12 && !looks_like_reference(trimmed) {
        if let Some(lvl) = numbered_level(trimmed).or_else(|| roman_section(trimmed)) {
            let title = trimmed.splitn(2, char::is_whitespace).nth(1).unwrap_or("");
            let title_upper = title.chars().find(|c| c.is_alphabetic()).map_or(false, |c| c.is_uppercase());
            let alpha = title.chars().filter(|c| c.is_alphabetic()).count();
            // Footnotes ("2 We do not show halo … because …") carry a leading number
            // like a section, but are set SMALLER than body text; a real numbered
            // heading is body-size or larger. Reject sub-body-size numbered lines.
            if title_upper && alpha >= 3 && ratio >= 0.92 {
                // "N. **Bold lead.** body" (a labelled limitation/claim list item):
                // keep only the bold run-in lead as the heading and let the body
                // split off — the whole sentence is not the heading.
                if let Some(fb) = line.runs.iter().position(|r| !r.text.trim().is_empty() && r.bold) {
                    let mut e = fb;
                    while e < line.runs.len() && (line.runs[e].text.trim().is_empty() || line.runs[e].bold) {
                        e += 1;
                    }
                    let lead: String = line.runs[..e].iter().map(|r| r.text.as_str()).collect();
                    if e < line.runs.len() && matches!(lead.trim_end().chars().next_back(), Some('.') | Some(':')) {
                        return Some((lvl, e));
                    }
                }
                // A wrapped prose line that a DECIMAL VALUE merely opened is not a
                // numbered section: "0.3 F1 behind fine-tuning the entire model. This …"
                // (wrapping after "… which is only"). Two signals together, both
                // required so real section titles stay headings: the leading number is
                // "0.x" (a value — section numbers start at 1, never 0), AND the title
                // reads as TWO sentences (an internal ". " + capital). Real numbered
                // sections ("2.1 …", "3.3 …") start ≥1 so are untouched.
                let internal_sentence = title.split(". ").nth(1).and_then(|r| r.chars().find(|c| c.is_alphabetic())).is_some_and(|c| c.is_uppercase());
                if trimmed.starts_with('0') && internal_sentence {
                    return None;
                }
                return Some((lvl, line.runs.len()));
            }
        }
    }
    // 2. Canonical unnumbered top-level header. A line that is *exactly* a
    //    canonical name (≤2 words, trailing '.'/':'/'—' allowed) is unambiguously
    //    a heading even at body size & non-bold (common in math/physics styles).
    let canon = low.trim_end_matches(['.', ':', '—', '-', ' ']);
    if trimmed.split_whitespace().count() <= 2 && TOP_HEADS.contains(&canon) {
        return Some((1, line.runs.len()));
    }
    // "Appendix A", "Appendix A. Title", "Appendix B: ..." → section level. But not
    // a sentence that merely opens with the word ("Appendix B shows that …") nor an
    // inline cross-reference ("…see Appendix A), β = …"): require a clean appendix id
    // followed by end / separator / a capitalised title, never a lowercase
    // continuation or a stray bracket.
    if low.starts_with("appendix ") && words <= 16 {
        let rest = trimmed["appendix ".len()..].trim_start();
        let id_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let id = &rest[..id_end];
        let after = rest[id_end..].trim_start();
        let clean_id = !id.is_empty() && id.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | ':' | '-'));
        let title_after = after.is_empty()
            || after.starts_with([':', '.', '—'])
            || after.chars().next().is_some_and(|c| c.is_uppercase());
        if clean_id && title_after {
            // A long sentence after the id ("Appendix E. The resulting … are arranged
            // …") is body text merged onto the appendix label — keep "Appendix <id>"
            // as the heading and split the sentence off as a paragraph.
            let after_sentence = after.split_whitespace().count() > 6
                && (after.contains(',')
                    || after.split_whitespace().any(|w| matches!(w, "is" | "are" | "was" | "were")));
            if after_sentence {
                let prefix_len = "appendix ".len() + (trimmed["appendix ".len()..].len() - rest.len()) + id_end;
                let target = (txt.len() - txt.trim_start().len()) + prefix_len;
                let mut acc = 0usize;
                for (ri, r) in line.runs.iter().enumerate() {
                    acc += r.text.len();
                    if acc >= target {
                        return Some((1, ri + 1));
                    }
                }
            }
            return Some((1, line.runs.len()));
        }
    }
    // 3. Standalone bold (or visibly larger) short line.
    if words <= 12
        && (all_bold || ratio >= 1.18)
        && !trimmed.ends_with(['.', ',', ';', ':'])
        && !looks_like_reference(trimmed)
        && trimmed.chars().next().is_some_and(|c| !c.is_lowercase())
    {
        return Some((size_level(ratio), line.runs.len()));
    }
    // 4. Bold run-in lead: short bold prefix, then non-bold body on the same line
    //    ("Encoder: The encoder …", "Task #1: Masked LM Before feeding …").
    if first_bold && !all_bold {
        let mut k = 0usize;
        while k < line.runs.len() {
            if line.runs[k].text.trim().is_empty() {
                k += 1;
                continue;
            }
            if line.runs[k].bold {
                k += 1;
            } else {
                break;
            }
        }
        let lead: String = line.runs[..k].iter().map(|r| r.text.as_str()).collect();
        let lead = lead.trim();
        let lead_words = lead.split_whitespace().count();
        // A real run-in head is a phrase (>=2 words) or a labelled lead ending in
        // ':'. This rejects drop-cap initials ("B", "T") and inline bold tokens
        // ("BERTBASE") that aren't section heads.
        // A real run-in head is a phrase (>=2 words) or a single labelled word
        // ending ':' or '.' ("Benchmarks.", "Discussions."). Rejects drop-cap
        // initials ("B.") and inline bold tokens ("BERTBASE").
        let labelled = lead.ends_with(':') || lead.ends_with('.');
        // A run-in head starts capitalised; a lowercase lead is a wrapped continuation
        // of an inline bold phrase ("…de Evidență / a Procurilor și a Revocărilor…"),
        // not a heading.
        let cap_lead = lead.chars().next().is_some_and(|c| !c.is_lowercase());
        if cap_lead
            && ((2..=8).contains(&lead_words)
                || (labelled && lead.chars().filter(|c| c.is_alphabetic()).count() >= 2))
        {
            return Some((3, k));
        }
    }
    None
}

/// "Figure 2 shows …", "Table 1 summarises …", "Fig. 3 plots …" — an in-text
/// cross-reference, NOT a caption. The figure/table number is followed by a verb
/// or pronoun (the sentence's predicate), whereas a real caption is followed by a
/// descriptive title or a ':' / '.' separator. Such a reference must stay body
/// prose, not become a phantom `<figure id=fig-N>` (which also duplicates the real
/// figure's id).
fn is_inline_xref(text: &str) -> bool {
    let low = text.trim_start().to_lowercase();
    let rest = ["figure", "fig.", "fig", "table", "tab."]
        .iter()
        .find_map(|p| low.strip_prefix(p));
    let rest = match rest {
        Some(r) => r.trim_start(),
        None => return false,
    };
    // Must be followed by a NUMBER ("Figure 2 …"). Consume the whole number token
    // incl. a letter suffix / sub-label ("4b", "S12", "3.2") so the predicate test
    // sees what really follows the reference number.
    if !rest.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let after_num = rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c.is_ascii_alphabetic());
    // A real CAPTION is introduced by ':' or a descriptive Title; an in-text REFERENCE
    // keeps the number inside a sentence — followed by a comma ("Figure 1, the arrows
    // …"), or (after an optional ')') by a sentence predicate ("Fig 4b) shows …",
    // "Table 3 confirms …").
    if after_num.trim_start().starts_with(',') {
        return true;
    }
    let after = after_num.trim_start_matches([')', ' ']);
    const PREDICATE: &[&str] = &[
        "show", "summari", "present", "plot", "compar", "illustrat", "depict", "report",
        "list", "give", "provide", "denote", "display", "demonstrat", "describe", "confirm",
        "indicat", "suggest", "impl", "exhibit", "contain", "yield", "mark", "reveal",
        "we ", "it ", "is ", "are ", "can ", "highlight", "visuali", "plots",
    ];
    PREDICATE.iter().any(|v| after.starts_with(v))
}

/// If a line is a figure/table caption ("Figure 3: …", "Fig. 2 …", "Table 1 …"),
/// return (is_figure, number). Used to opportunistically anchor a caption to a
/// figure/table region — never required for the region to be emitted.
fn caption_label(text: &str) -> Option<(bool, String)> {
    let t = text.trim_start();
    let low = t.to_lowercase();
    let (is_fig, rest) = if low.starts_with("figure") {
        (true, &t[6..])
    } else if low.starts_with("fig.") {
        (true, &t[4..])
    } else if low.starts_with("table") {
        (false, &t[5..])
    } else {
        return None;
    };
    let rest = rest.trim_start();
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() {
        return None;
    }
    Some((is_fig, num))
}

/// Gather a (possibly multi-line) caption block starting at line `idx`: the
/// caption sentence plus its continuation lines, stopping at an indented new
/// paragraph, a vertical gap, a heading/list, or another caption. Tiny stray
/// superscripts (footnote markers) are skipped, not treated as a break. Returns
/// the rendered caption HTML and the line indices consumed.
fn gather_caption(lines: &[Line], idx: usize, body: f32) -> (String, Vec<usize>) {
    let base_x = lines[idx].x0;
    let base_sz = lines[idx].size;
    let mut html = render_runs(&lines[idx].runs);
    let mut used = vec![idx];
    let mut prev_y = lines[idx].y;
    let mut k = idx + 1;
    while k < lines.len() && used.len() < 14 {
        let l = &lines[k];
        // Skip a tiny stray superscript (e.g. a footnote marker) interleaved in
        // the caption's y-range without ending the caption.
        if l.size < base_sz * 0.75 && l.text().trim().chars().count() <= 3 {
            used.push(k);
            k += 1;
            continue;
        }
        let dy = prev_y - l.y; // reading order is top->down: a small positive step
        if dy < -l.size || dy > l.size * 1.8 {
            break; // new column/region, or a paragraph-sized gap
        }
        if l.x0 - base_x > l.size * 0.6 {
            break; // indented => a new paragraph, not caption continuation
        }
        if detect_header(l, body).is_some() || list_kind(&l.text()).is_some() || caption_label(&l.text()).is_some() {
            break;
        }
        append_piece(&mut html, &render_runs(&l.runs));
        used.push(k);
        prev_y = l.y;
        k += 1;
    }
    (html, used)
}

/// Render a positioned table. A caption (when present) is emitted as the table's
/// own `<caption>` and the anchor id goes on the `<table>` — a table is tabular
/// data, not a figure, so it is NOT wrapped in `<figure>`.
fn table_html(t: &PosTable, cap: Option<(&str, &str, bool)>) -> String {
    let mut tbl = match cap {
        Some((num, _, _)) => format!("<table id=\"tab-{num}\">"),
        None => String::from("<table>"),
    };
    // Detached grouped/multi-level header rows (mapped onto the data column grid with
    // colspans) render first as <th>. When present, the data grid is ALL <td> (its row
    // 0 is data, not the header). When absent, fall back to treating grid row 0 as <th>.
    for hrow in &t.header {
        tbl.push_str("<tr>");
        for (text, span) in hrow {
            if *span > 1 {
                tbl.push_str(&format!("<th colspan=\"{span}\">{}</th>", esc(text.trim())));
            } else {
                tbl.push_str(&format!("<th>{}</th>", esc(text.trim())));
            }
        }
        tbl.push_str("</tr>");
    }
    let has_header = !t.header.is_empty();
    for (ri, row) in t.grid.iter().enumerate() {
        tbl.push_str("<tr>");
        let tag = if ri == 0 && !has_header { "th" } else { "td" };
        for cell in row {
            tbl.push_str(&format!("<{tag}>{}</{tag}>", esc(cell.trim())));
        }
        tbl.push_str("</tr>");
    }
    tbl.push_str("</table>");
    match cap {
        // Pair the table with its caption as a sibling block `<div>`, not an HTML
        // `<caption>`: a `<caption>` is locked to the (often narrow) table width,
        // cramping the text, whereas a sibling block uses the full page reading-column
        // width. No class (the HTML is kept thin/semantic) — a `<div>` is emitted ONLY
        // here, so it unambiguously marks a table caption. Keep the side it sits on in
        // the source (a caption often sits BELOW the table, e.g. BERT Table 1). Not a
        // `<figure>`/`<figcaption>` — that would conflate data tables with image figures
        // and trip the figure gates.
        Some((_, caption, below)) => {
            let capdiv = format!("<div>{caption}</div>");
            if below {
                format!("{tbl}{capdiv}")
            } else {
                format!("{capdiv}{tbl}")
            }
        }
        None => tbl,
    }
}

/// Mark which lines belong to a page-bottom footnote block (see emit_lines). A run of
/// >=2 consecutive footnote-sized lines (`size < body*0.86`) confined to the bottom ~45%
/// of the content, capped at 12 lines so a small-font reference list isn't swallowed.
fn footnote_region_mask(lines: &[&Line], body: f32) -> Vec<bool> {
    let mut mark = vec![false; lines.len()];
    if lines.len() < 2 {
        return mark;
    }
    let (lo, hi) = lines.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), l| (a.min(l.y), b.max(l.y)));
    let bottom_cut = lo + (hi - lo) * 0.45; // footnotes sit below this y
    let small = |l: &Line| !l.text().trim().is_empty() && l.size < body * 0.86;
    let mut i = 0;
    while i < lines.len() {
        if !small(lines[i]) {
            i += 1;
            continue;
        }
        let a = i;
        while i < lines.len() && small(lines[i]) {
            i += 1;
        }
        // lines[a] is the run's first (highest-y) line; require the whole run low on page
        if (2..=12).contains(&(i - a)) && lines[a].y < bottom_cut {
            mark[a..i].iter_mut().for_each(|m| *m = true);
        }
    }
    mark
}

/// Render a footnote block's lines as `<p>` items inside the caller's `<aside>`: a lone
/// marker number ("1") begins a new footnote and is joined to the definition that
/// follows ("1." + "https://…"); wrapped continuation lines fold in (de-hyphenated).
fn emit_footnotes(lines: &[&Line], out: &mut String) {
    // A footnote begins with its marker number: either a lone "1" line, or a number
    // glued to the start of the definition ("3In all cases…"). Split the marker off and
    // begin a new <p>; a line with no leading marker (a wrapped definition line, or a URL
    // on its own line) continues the current footnote.
    let lead_marker = |t: &str| -> Option<(String, String)> {
        let t = t.trim_start();
        let n: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        if n.is_empty() || n.len() > 2 {
            return None;
        }
        let rest = t[n.len()..].trim_start();
        // lone marker, or a marker glued to text that starts a word/quote (a real
        // footnote), not a numeric continuation like "4H, i.e., …"
        if rest.is_empty() || rest.chars().next().is_some_and(|c| c.is_alphabetic() || "“\"'".contains(c)) {
            Some((n, rest.to_string()))
        } else {
            None
        }
    };
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut String| {
        if !cur.trim().is_empty() {
            out.push_str(&format!("<p>{}</p>", cur.trim()));
            cur.clear();
        }
    };
    for l in lines {
        match lead_marker(&l.text()) {
            Some((num, rest)) => {
                flush(&mut cur, out);
                cur.push_str(&num);
                cur.push_str(". ");
                cur.push_str(&rest);
            }
            None => append_piece(&mut cur, render_runs(&l.runs).trim()),
        }
    }
    flush(&mut cur, out);
}

/// Emit a run of consecutive text lines as headings / paragraphs / lists / code.
fn emit_lines(lines: &[&Line], body: f32, title_sz: f32, out: &mut String) {
    let mut i = 0;
    // The currently-open paragraph. It is NOT flushed at a column-wrap block
    // boundary — a paragraph that wraps from the bottom of one column to the top
    // of the next stays one <p> (and the line-break hyphen de-hyphenates across
    // the wrap). It is flushed only at a real paragraph start, or a header/list/
    // mono boundary, or end of input.
    let mut para = String::new();
    macro_rules! flush_para {
        () => {
            if !para.trim().is_empty() {
                out.push_str(&format!("<p>{}</p>", para.trim()));
                para.clear();
            }
        };
    }
    // Progress invariant / crash guard: every iteration must advance `i` (consume ≥1
    // line). Checked at the loop TOP so it covers every `continue` path. If a branch
    // ever leaves `i` unchanged, force-advance instead of spinning forever — an
    // un-advanced loop here also appends to `out` each turn, so a stall is not a benign
    // CPU spin but unbounded memory growth → OOM. This degrades that whole bug class to
    // at worst one skipped line (which the tests catch), never a machine crash.
    // Footnote regions: a run of >=2 consecutive footnote-sized lines (visibly smaller
    // than the body) confined to the BOTTOM of the page is the footnote block. Marked
    // here and emitted as one <aside> below, instead of loose, fragmented <p>s. Guards:
    // the run sits in the bottom ~45% (so page-1 affiliations at the top don't qualify),
    // and is short (<=12 lines, so a small-font reference list — which fills the page —
    // is not swallowed). An inline subscript doesn't count: its LINE is body-sized.
    let foot = footnote_region_mask(lines, body);

    let mut watchdog = usize::MAX;
    while i < lines.len() {
        if i == watchdog {
            i += 1;
            continue;
        }
        watchdog = i;
        if foot[i] {
            flush_para!();
            let a = i;
            while i < lines.len() && foot[i] {
                i += 1;
            }
            out.push_str("<aside>");
            emit_footnotes(&lines[a..i], out);
            out.push_str("</aside>");
            continue;
        }
        let ln = lines[i];
        let txt = ln.text();
        // header (paragraph-aware: standalone line or bold run-in lead). Checked
        // before lists so a capitalised "N. Title" / "N.N Title" reads as a section
        // (section faces are often body-size & non-bold — numbering is the signal);
        // a lowercase or sentence "N." falls through to the list branch below.
        // EXCEPT a tight enumerated run ("1. … 2. … 3. …") is a list, not a run of
        // section headings — route those numbered lines to the list branch. ALSO except
        // a COLON-INTRODUCED numbered run (the prose announces it with a "…:" lead-in,
        // e.g. BERT C.1 "…the following questions:" → "1. Question: … / 2. Question: …"):
        // those colon-labelled items form a list, not a sequence of section headings.
        if !in_enumerated_run(lines, i) && !colon_introduced_list(lines, i) {
        if let Some((lvl, k)) = detect_header(ln, body) {
            // HTML heading tag: reserve <h1> for the document title (the largest
            // text). Sections (logical level 1) become <h2>, subsections <h3>,
            // etc., so the outline nests under a single <h1>.
            flush_para!(); // a heading ends any open paragraph
            let tag = if title_sz > body * 1.25 && ln.size >= title_sz - 0.5 {
                1
            } else {
                (lvl + 1).min(6)
            };
            out.push_str(&format!("<h{tag}>{}</h{tag}>", render_runs(&ln.runs[..k])));
            if k < ln.runs.len() {
                // Run-in lead ("Model Architecture BERT's model architec-"): the rest
                // of THIS line begins the body. Seed the paragraph accumulator with it
                // (don't close it as its own <p>) so the wrapped continuation on the
                // next line flows in and a line-break hyphen rejoins ("architec-" +
                // "ture" → "architecture"). A real paragraph break after it still
                // flushes via the usual indent/gap rules in the paragraph branch.
                let rest = render_runs(&ln.runs[k..]);
                if !rest.trim().is_empty() {
                    append_piece(&mut para, rest.trim());
                }
            }
            // Standalone header: consume just the heading line and let the body
            // flow back through the paragraph branch (which splits on indentation).
            i += 1;
            continue;
        }
        }
        // list — but a numbered "list" of author-initial / year-bearing entries is
        // a bibliography, not a list; let it fall through to a paragraph.
        if list_kind(&txt).is_some() && !(has_year(&txt) || initials_count(&txt) >= 2) {
            flush_para!();
            let ordered = list_kind(&txt).unwrap();
            let tag = if ordered { "ol" } else { "ul" };
            out.push_str(&format!("<{tag}>"));
            // Each <li> is its marker line PLUS any wrapped continuation lines (no
            // marker, indented past the marker, same column, small gap). Keeping the
            // continuations inside the item — and the list open across them — stops a
            // wrapped bullet from splitting into one-item-per-list + stray <p>s.
            while i < lines.len()
                && list_kind(&lines[i].text()).is_some()
                && (in_enumerated_run(lines, i) || colon_introduced_list(lines, i) || detect_header(lines[i], body).is_none())
            {
                let marker_x = lines[i].x0;
                let mut item = strip_marker(&lines[i].text());
                let mut prev_y = lines[i].y;
                i += 1;
                while i < lines.len() {
                    let l = lines[i];
                    let cont = list_kind(&l.text()).is_none()
                        && !l.mono
                        && detect_header(l, body).is_none()
                        && l.x0 >= marker_x - body * 0.3 // not dedented past the marker (flush or hanging-indent wrap)
                        && l.y <= prev_y + body * 0.5 // same column, flowing downward
                        && (prev_y - l.y) < body * 1.6; // tight line spacing — a paragraph gap ends the item
                    if !cont {
                        break;
                    }
                    item.push(' ');
                    item.push_str(l.text().trim());
                    prev_y = l.y;
                    i += 1;
                }
                out.push_str(&format!("<li>{}</li>", esc(item.trim())));
            }
            out.push_str(&format!("</{tag}>"));
            continue;
        }
        // code / monospace block
        if ln.mono {
            flush_para!();
            out.push_str("<pre><code>");
            while i < lines.len() && lines[i].mono && list_kind(&lines[i].text()).is_none() {
                out.push_str(&esc(&lines[i].text()));
                out.push('\n');
                i += 1;
            }
            out.push_str("</code></pre>");
            continue;
        }
        // paragraph block: gather consecutive normal lines. The starting line is
        // consumed UNCONDITIONALLY (it already passed header/list/mono, or was a
        // citation-like line the list branch declined) — only *subsequent* lines
        // break the run, so `i` always advances (no infinite loop).
        let start = i;
        i += 1;
        while i < lines.len() {
            let l = lines[i];
            // Stop the body block at the footnote region so it is emitted as its own
            // <aside> (handled at the loop top) rather than swallowed into this paragraph.
            if foot[i] || l.mono || list_kind(&l.text()).is_some() || detect_header(l, body).is_some() {
                break;
            }
            // Column wrap: reading order goes top-to-bottom within a column, so y
            // jumping back UP means we've crossed into the next column. Break the
            // block there so the per-block indent baseline stays single-column
            // (else left-margin x0=72 and right-column x0=307 mix and mis-split).
            if l.y > lines[i - 1].y + body * 0.5 {
                break;
            }
            i += 1;
        }
        let block = &lines[start..i];
        // Split the block into paragraphs by first-line indentation. `base` is the
        // column's left edge; paragraph starts are the MINORITY indent cluster —
        // which is the indented first-lines of normal prose, or (when most lines
        // are indented) the out-dented first-lines of a hanging-indent reference
        // list. Blocks with no indentation variation stay a single paragraph.
        let base = block.iter().map(|l| l.x0).fold(f32::INFINITY, f32::min);
        let thr = (body * 0.35).max(2.5);
        let indented = block.iter().filter(|l| l.x0 > base + thr).count();
        let hanging = indented * 2 > block.len();
        // Also split on a vertical gap larger than the block's normal line spacing
        // (paragraph spacing) — catches paragraph breaks that aren't indented, e.g.
        // the two unindented paragraphs of an abstract.
        let mut gaps: Vec<f32> = (1..block.len()).map(|j| block[j - 1].y - block[j].y).filter(|g| *g > 0.0).collect();
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let line_gap = gaps.get(gaps.len() / 2).copied().unwrap_or(0.0); // median
        for (j, l) in block.iter().enumerate() {
            let at_base = l.x0 <= base + thr;
            let big_gap = j > 0 && line_gap > 0.0 && (block[j - 1].y - l.y) > line_gap * 1.25 + 0.5;
            let indent_new = if hanging { at_base } else { !at_base };
            // The FIRST line of a block (a fresh column after a wrap) starts a new
            // paragraph only if the indentation says so — otherwise it continues the
            // paragraph open from the previous column. Subsequent lines split on the
            // usual indent / big-gap signals.
            let starts_para = if j == 0 {
                para.trim().is_empty() || indent_new
            } else {
                big_gap || indent_new
            };
            if starts_para {
                flush_para!();
            }
            append_piece(&mut para, &render_runs(&l.runs));
        }
        // Intentionally NOT flushed here: a paragraph may continue in the next
        // column's block (handled by the j==0 continuation rule above).
    }
    flush_para!();
}

/// Append a rendered line to a paragraph, joining a line-break hyphen. The hyphen
/// may be wrapped in trailing close tags (e.g. `<a href="#ref">Rad-</a>` when the
/// split word is also a link anchor); we look past those, drop the hyphen, and
/// join with no space so "Rad-" + "ford et al." reads "Radford et al."
/// First non-whitespace character of a fragment, skipping any leading HTML tags.
fn first_visible(s: &str) -> Option<char> {
    let mut intag = false;
    for c in s.chars() {
        match c {
            '<' => intag = true,
            '>' => intag = false,
            _ if intag => {}
            c if !c.is_whitespace() => return Some(c),
            _ => {}
        }
    }
    None
}

fn append_piece(para: &mut String, piece: &str) {
    if para.is_empty() {
        *para = piece.to_string();
        return;
    }
    // Skip a run of trailing close tags ("</a>", "</b>", "</i>") to find the last
    // visible character.
    let mut end = para.len();
    while para[..end].ends_with('>') {
        match para[..end].rfind("</") {
            Some(lt) if !para[lt + 2..end - 1].contains('<') => end = lt,
            _ => break,
        }
    }
    if para[..end].ends_with('-') {
        // A line-break hyphen is usually a syllable split to remove ("mini-" +
        // "mal" -> "minimal"). But after a complete compounding prefix it is a
        // LEXICAL hyphen that must stay ("self-" + "attention" -> "self-attention",
        // not "selfattention"). Such prefixes are whole words never syllable-split.
        let stem = &para[..end - 1];
        let wstart = stem.rfind([' ', '>']).map(|i| i + 1).unwrap_or(0);
        let prefix = stem[wstart..].trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
        const KEEP: &[&str] = &[
            "self", "non", "semi", "quasi", "multi", "cross", "anti", "pseudo", "intra",
            "inter", "micro", "macro", "meta", "mono", "fine", "high", "low", "left",
            "right", "state", "well", "real", "open", "long", "short", "large", "small",
            "full", "half", "first", "second", "third", "single", "double", "lower", "upper",
        ];
        // A syllable-split continuation is LOWERCASE ("mini-" + "mal"). When the next
        // line starts UPPERCASE it is not that continuation — a proper noun, a new
        // sentence, or (in a mis-ordered two-column region, e.g. a reference list) a
        // different column's text. Treat the hyphen as lexical (keep it) so we never
        // fuse "undercom-" + "Nguyen" into "undercomNguyen"; this also preserves a
        // genuine hard-hyphen compound broken at the line end ("X-" + "Ray").
        let cont_upper = first_visible(piece).is_some_and(|c| c.is_uppercase());
        if KEEP.contains(&prefix.as_str()) || cont_upper {
            para.push_str(piece); // keep the lexical hyphen, no space
        } else {
            para.remove(end - 1); // drop the syllable-break hyphen (keeps close tags)
            para.push_str(piece);
        }
    } else {
        para.push(' ');
        para.push_str(piece);
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
            if s.angle.abs() < 0.01 {
                *hist.entry(s.size.round() as i32).or_insert(0) += 1; // body size ignores rotated labels
            }
        }
        page_spans.push((pno, pid, spans));
    }
    page_spans.sort_by_key(|(pno, _, _)| *pno);
    let body = hist.iter().max_by_key(|(_, c)| **c).map(|(s, _)| *s as f32).unwrap_or(10.0);
    // The document title is the largest text on the FIRST page; reserve <h1> for it.
    // (Scoped to page 1 — a large figure label or display equation on a later page
    // must not inflate the threshold and demote the real title to <h2>.)
    let title_sz = page_spans
        .first()
        .map(|(_, _, s)| s.iter().map(|sp| sp.size).fold(body, f32::max))
        .unwrap_or(body);

    // Hyperlinks, grouped by the page that carries the clickable rectangle. External
    // URIs link out; an internal link prefers its SEMANTIC named destination
    // (#cite.x / #figure.n / #equation.n / #section.x) — resolving to the exact
    // target — and only falls back to "#page-N" when there is no name.
    let mut links_by_page: std::collections::HashMap<u32, Vec<LinkBox>> = std::collections::HashMap::new();
    for lk in links::extract_links(doc) {
        let href = match (&lk.uri, &lk.dest_name, lk.dest_page) {
            (Some(u), _, _) => u.clone(),
            (None, Some(name), _) => format!("#{}", slug(name)),
            (None, None, Some(p)) => format!("#page-{p}"),
            _ => continue,
        };
        links_by_page.entry(lk.page).or_default().push(LinkBox { rect: lk.rect, href });
    }
    let no_links: Vec<LinkBox> = Vec::new();

    // Named-destination targets, grouped by page: each becomes an anchor id at (or
    // near) its position so the semantic links above actually resolve.
    let mut dests_by_page: std::collections::HashMap<u32, Vec<(String, Option<f32>)>> = std::collections::HashMap::new();
    for d in links::named_destinations(doc) {
        dests_by_page.entry(d.page).or_default().push((slug(&d.name), d.y));
    }

    // Lean stylesheet: a centered reading column + images/figures that never overflow
    // it. Deliberately minimal (no per-element slop) — the HTML is consumed by LLMs.
    let mut out = String::from(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <style>\nbody{max-width:48rem;margin:auto;padding:1rem}\n\
         img,svg{max-width:100%;height:auto}\n</style>\n</head>\n<body>\n",
    );
    for (pidx, (pno, _pid, spans)) in page_spans.iter().enumerate() {
        out.push_str(&format!("<section data-page=\"{pno}\" id=\"page-{pno}\">\n"));
        // Anchor targets for this page's named destinations, so the semantic links
        // (#cite.x / #figure.n / #equation.n / #section.x) resolve. Empty <a id> at
        // the section head land the reader on the correct page + the exact target id.
        if let Some(ds) = dests_by_page.get(pno) {
            for (sl, _y) in ds {
                out.push_str(&format!("<a id=\"{sl}\"></a>"));
            }
            out.push('\n');
        }
        let tables = extract::detect_tables_pos(spans);
        let plinks = links_by_page.get(pno).unwrap_or(&no_links);
        let mut lines = lines_of(spans.iter().map(clone_span).collect(), plinks);
        // Drop running page numbers: a line that is just a 1–4 digit number sitting
        // in the top or bottom margin band of the page (a running footer/header
        // number). Structural — keyed on position + lone-number shape, not per-doc.
        if !lines.is_empty() {
            let ymax = lines.iter().map(|l| l.y).fold(f32::MIN, f32::max);
            let ymin = lines.iter().map(|l| l.y).fold(f32::MAX, f32::min);
            let band = (ymax - ymin).max(1.0) * 0.07;
            lines.retain(|l| {
                let t = l.text();
                let t = t.trim();
                let lone_num = !t.is_empty() && t.len() <= 4 && t.bytes().all(|b| b.is_ascii_digit());
                !(lone_num && (l.y >= ymax - band || l.y <= ymin + band))
            });
        }

        // Document title (first page): the topmost PROMINENT block — bold, all-caps,
        // or larger than body — before the authors/abstract. Emit it as the single
        // <h1> and consume those lines (so a section heading below isn't the
        // document's first heading; a multi-line title becomes one <h1>). Titles are
        // often bold-but-not-larger, so prominence — not size alone — is the signal.
        if pidx == 0 && !lines.is_empty() {
            let allcaps = |t: &str| {
                let a = t.chars().filter(|c| c.is_alphabetic());
                let (mut n, mut up) = (0, 0);
                for c in a {
                    n += 1;
                    if c.is_uppercase() {
                        up += 1;
                    }
                }
                n >= 4 && up == n
            };
            // Core: could be a title (a multi-word phrase that isn't a numbered
            // section, an author/reference line, or "Abstract").
            let title_core = |l: &Line| {
                let t = l.text();
                let tt = t.trim();
                tt.split_whitespace().count() >= 2
                    && numbered_level(tt).is_none()
                    && roman_section(tt).is_none()
                    && !looks_like_reference(tt)
                    && !tt.to_lowercase().starts_with("abstract")
            };
            let prominent = |l: &Line| {
                let tt = l.text();
                l.runs.iter().any(|r| !r.text.trim().is_empty() && r.bold) || allcaps(tt.trim()) || l.size > body + 0.5
            };
            let title_ok = |l: &Line| title_core(l) && prominent(l);
            let has_abstract = lines.iter().any(|l| l.text().trim().to_lowercase().starts_with("abstract"));
            let mut order: Vec<usize> = (0..lines.len()).collect();
            order.sort_by(|&a, &b| lines[b].y.partial_cmp(&lines[a].y).unwrap_or(std::cmp::Ordering::Equal));
            // Prefer a prominent (bold/caps/large) title; else, in front matter (an
            // Abstract is present), fall back to the topmost phrase-line — a plain
            // body-size title still sits above the authors/abstract.
            // The title head must START with a capital (a title is not a lowercase
            // body fragment) and its assembled block must not be running prose — a
            // license/copyright banner at the very top of the page is a full
            // sentence and must never be mistaken for the document title.
            let cap_start = |l: &Line| l.text().trim().chars().next().map_or(false, |c| !c.is_lowercase());
            // Running prose (a license/copyright banner at the page top) is a long
            // run of mostly lowercase function words; a title — even a long one — is
            // Title-Case content words. Reject a >12-word candidate that is ≥60%
            // lowercase words (and any candidate that is a full sentence ending '.').
            let is_prose = |t: &str| {
                let words: Vec<&str> = t.split_whitespace().collect();
                if words.len() <= 12 {
                    return false;
                }
                if t.trim_end().ends_with('.') {
                    return true;
                }
                let lc = words.iter().filter(|w| w.chars().next().is_some_and(|c| c.is_lowercase())).count();
                lc * 5 >= words.len() * 3
            };
            let _ = has_abstract;
            let mut heads: Vec<usize> = order.iter().cloned().filter(|&i| title_ok(&lines[i]) && cap_start(&lines[i])).collect();
            // Fallback: a plain body-size title (not bold/large) still sits at the very
            // top of page 1, above the authors/abstract — the topmost cap-start phrase
            // line that isn't a section/reference/prose line is the title.
            heads.extend(order.iter().cloned().filter(|&i| {
                title_core(&lines[i]) && cap_start(&lines[i]) && lines[i].text().split_whitespace().count() >= 4
            }));
            for head in heads {
                let (tsz, hy) = (lines[head].size, lines[head].y);
                // The title block: prominent, same font size, within ~4 line-heights
                // below the head line (a multi-line title is contiguous at the top).
                let set: std::collections::HashSet<usize> = order
                    .iter()
                    .cloned()
                    .filter(|&i| (lines[i].size - tsz).abs() <= 0.5 && lines[i].y <= hy + 0.5 && lines[i].y >= hy - tsz * 4.0 && title_core(&lines[i]))
                    .collect();
                let title = order
                    .iter()
                    .filter(|i| set.contains(i))
                    .map(|&i| lines[i].text())
                    .collect::<Vec<_>>()
                    .join(" ");
                let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
                if title.split_whitespace().count() >= 2 && !is_prose(&title) {
                    out.push_str(&format!("<h1>{}</h1>", esc(&title)));
                    let mut i = 0usize;
                    lines.retain(|_| {
                        let keep = !set.contains(&i);
                        i += 1;
                        keep
                    });
                    break;
                }
            }
        }

        // Mark lines consumed by a table (within its y-range).
        // A line belongs to a table only if it overlaps the table in BOTH axes. The
        // x-overlap is essential on two-column pages: a table in one column must not
        // swallow the OTHER column's prose at the same height (BERT p16: the right-
        // column "Masking Rates" table was dropping the left-column Q&A text). A
        // full-width single-column table still matches every line (x-overlap trivially
        // holds), so its behaviour is unchanged.
        let in_table = |x0: f32, x1: f32, y: f32| {
            tables.iter().any(|t| {
                y <= t.y_top + body && y >= t.y_bottom - body && x1 > t.x_left && x0 < t.x_right
            })
        };

        // Ordered items by y (top -> bottom). Img/T carry an index so a caption
        // can be attached opportunistically (see below).
        enum Item<'a> {
            L(&'a Line),
            T(usize),
            Img(usize),
            Svg(usize), // vector figure transcoded to inline SVG
            Cap(usize), // standalone caption (e.g. a vector figure with no raster)
        }
        let images = img::positioned_images(doc, *_pid);
        // Vector figures (diagrams/plots drawn as paths). A figure's AREA is the
        // detected vector-ink cluster; we drop any that overlap a detected table
        // (tables own their region) so table rules aren't re-emitted as a figure.
        let mut vectors: Vec<vector::PlacedSvg> = vector::positioned_vectors(doc, *_pid)
            .into_iter()
            .filter(|v| {
                !tables.iter().any(|t| {
                    v.x_left < t.x_right && v.x_right > t.x_left && v.y_bottom < t.y_top && v.y_top > t.y_bottom
                })
            })
            .collect();
        // A vector figure's bbox — used to attach its labels and to keep that text
        // out of the body flow (it belongs to the figure, not the prose).
        let fig_boxes: Vec<(f32, f32, f32, f32)> = vectors.iter().map(|v| (v.x_left, v.x_right, v.y_bottom, v.y_top)).collect();
        let in_figure = |x: f32, y: f32| fig_boxes.iter().any(|&(xl, xr, yb, yt)| x >= xl - 4.0 && x <= xr + 4.0 && y >= yb - 4.0 && y <= yt + 4.0);
        // Axis tick labels and axis titles sit just OUTSIDE the plotted ink (left of the
        // y-axis, below the x-axis) — beyond `in_figure`'s tight 4pt margin, so without
        // this they leak into the prose ("84 82 80 78 76", "Pre-training Steps …"). They
        // are SMALL-FONT (tick/label size, well under body); claim small-font text within
        // a wider margin of the figure as a figure label too. The font gate keeps body
        // prose and the body-size "Figure N:" caption (just below the plot) out.
        let axis_margin = body * 2.2;
        let near_fig_label = |x: f32, y: f32, size: f32| {
            size < body * 0.82
                && fig_boxes.iter().any(|&(xl, xr, yb, yt)| {
                    x >= xl - axis_margin && x <= xr + axis_margin && y >= yb - axis_margin && y <= yt + axis_margin
                })
        };
        // Render the figures' text as SVG <text>: a figure's labels are drawn either
        // inside its Form XObject (form_text_spans) OR directly in the page content
        // within the figure's bbox (DAG node labels, plot axis ticks) — both are
        // collected here so the figure shows its labels, and the body-content ones
        // are removed from the prose flow below (see the in_figure filter).
        if !vectors.is_empty() {
            let mk = |s: text::Span| vector::LabelSpan { x: s.x, y: s.y, size: s.size, width: s.width, text: s.text, bold: s.bold, italic: s.italic, angle: s.angle };
            let mut labels: Vec<vector::LabelSpan> = text::form_text_spans(doc, *_pid, raw).into_iter().map(mk).collect();
            for s in spans {
                let (cx, cy) = (s.x + s.width * 0.5, s.y + s.size * 0.5);
                if in_figure(cx, cy) || near_fig_label(cx, cy, s.size) {
                    labels.push(mk(clone_span(s)));
                }
            }
            vector::attach_labels(&mut vectors, &labels);
        }

        // Page x-extent (from text lines), used to give full-width boxes to
        // images whose horizontal position we don't track.
        let (mut px0, mut px1) = (f32::INFINITY, f32::NEG_INFINITY);
        for l in &lines {
            px0 = px0.min(l.x0);
            px1 = px1.max(l.x1);
        }
        if !px0.is_finite() {
            (px0, px1) = (0.0, 1.0);
        }

        // Opportunistic caption linking: each "Figure N"/"Table N" caption line is
        // anchored to its nearest (by y) figure/table on the page. Captions are
        // enrichment — a region with no caption is still emitted as a region.
        // A line beginning "Table N"/"Figure N" is only a CAPTION if it starts a
        // fresh block — not when it is the wrapped continuation of a prose sentence
        // that merely *references* the table (e.g. "…are shown in" ⏎ "Table 6. In this
        // table we report…"). Such a line is adjacent in the same column to a previous
        // prose line that does not end a sentence; treat it as body text, not a caption.
        let ends_sentence = |t: &str| {
            let t = t.trim_end();
            t.ends_with(['.', '!', '?', ':']) || t.ends_with(".\"") || t.ends_with(".)") || t.ends_with(".\u{201d}")
        };
        // Words that introduce an in-text reference ("…are shown in" ⏎ "Table 6"):
        // when the previous line ends on one of these, the "Table N" is a reference,
        // not a caption. Deliberately narrow so real captions (which follow a sentence
        // end or a vertical gap) are never suppressed.
        let ref_lead = |w: &str| {
            matches!(
                w,
                "in" | "see" | "from" | "of" | "to" | "and" | "via" | "using" | "per" | "cf"
                    | "table" | "tables" | "figure" | "figures" | "fig"
                    | "shown" | "presented" | "listed" | "reported" | "summarized" | "given"
            )
        };
        let is_ref_continuation = |idx: usize| -> bool {
            if idx == 0 {
                return false;
            }
            let (prev, cur) = (&lines[idx - 1], &lines[idx]);
            if in_table(prev.x0, prev.x1, prev.y) {
                return false; // a caption right after a table's last row is legitimate
            }
            let dy = prev.y - cur.y; // reading order top->down
            let adjacent = dy > -cur.size && dy < cur.size * 1.8 && (prev.x0 - cur.x0).abs() < cur.size * 1.5;
            if !adjacent {
                return false;
            }
            let pt = prev.text();
            let pt = pt.trim_end();
            if ends_sentence(pt) {
                return false;
            }
            let last: String = pt.rsplit(char::is_whitespace).next().unwrap_or("").chars().filter(|c| c.is_alphabetic()).flat_map(char::to_lowercase).collect();
            ref_lead(&last)
        };
        let cap_lines: Vec<(usize, bool, String)> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !in_table(l.x0, l.x1, l.y))
            .filter_map(|(idx, l)| {
                let t = l.text();
                caption_label(&t).and_then(|(f, n)| (!is_ref_continuation(idx) && !is_inline_xref(&t)).then(|| (idx, f, n)))
            })
            .collect();
        let mut consumed_caption = std::collections::HashSet::new();
        let mut img_cap: Vec<Option<(String, String)>> = vec![None; images.len()]; // (num, html)
        let mut svg_cap: Vec<Option<(String, String)>> = vec![None; vectors.len()]; // (num, html)
        let mut tab_cap: Vec<Option<(String, String, bool)>> = vec![None; tables.len()]; // (num, html, below)
        // Captions with no raster image / detected table (e.g. vector figures) are
        // still figure/table regions — emit the caption standalone so the number +
        // description and its anchor id survive.
        let mut standalone: Vec<(f32, f32, String)> = Vec::new(); // (x0, y, html)
        // Each caption is gathered as its full (multi-line) block and anchored to the
        // nearest figure (image) / table by y; unanchored captions stand alone.
        for &(idx, is_fig, ref num) in &cap_lines {
            if consumed_caption.contains(&idx) {
                continue; // already swallowed as a previous caption's continuation
            }
            let cy = lines[idx].y;
            let (html, used) = gather_caption(&lines, idx, body);
            let anchored = if is_fig {
                // Anchor a "Figure N" caption to the nearest unclaimed figure REGION
                // by y — whether it is a raster <img> OR a vector <svg> (a figure is
                // a figure regardless of how it's drawn). This merges the caption and
                // its graphic into one <figure> instead of leaving a caption-less SVG
                // plus a graphic-less standalone caption.
                // Distance from the caption to a figure is the gap to its NEAREST edge
                // (the figure's [y_bottom, y_top] interval), not to its top: a caption
                // sits just below its figure, so for a tall raster the top edge is far
                // away while a lower figure's top is deceptively near — measuring to the
                // top swaps the two captions. Edge distance binds each caption to the
                // figure it actually abuts (0 when the caption is inside the figure box).
                let edge = |yb: f32, yt: f32| if cy < yb { yb - cy } else if cy > yt { cy - yt } else { 0.0 };
                let img_best = images.iter().enumerate()
                    .filter(|(j, _)| img_cap[*j].is_none())
                    .map(|(j, im)| (j, edge(im.y_bottom, im.y_top)))
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                let svg_best = vectors.iter().enumerate()
                    .filter(|(j, _)| svg_cap[*j].is_none())
                    .map(|(j, v)| (j, edge(v.y_bottom, v.y_top)))
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                match (img_best, svg_best) {
                    (Some((j, di)), Some((k, ds))) => {
                        if di <= ds {
                            img_cap[j] = Some((num.clone(), html.clone()));
                        } else {
                            svg_cap[k] = Some((num.clone(), html.clone()));
                        }
                        true
                    }
                    (Some((j, _)), None) => {
                        img_cap[j] = Some((num.clone(), html.clone()));
                        true
                    }
                    (None, Some((k, _))) => {
                        svg_cap[k] = Some((num.clone(), html.clone()));
                        true
                    }
                    (None, None) => false,
                }
            } else {
                tables.iter().enumerate()
                    .filter(|(j, _)| tab_cap[*j].is_none())
                    .min_by(|(_, a), (_, b)| (a.y_top - cy).abs().partial_cmp(&(b.y_top - cy).abs()).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(j, t)| {
                        let below = cy < (t.y_top + t.y_bottom) * 0.5; // caption sits below the table (y up)
                        tab_cap[j] = Some((num.clone(), html.clone(), below));
                    })
                    .is_some()
            };
            if !anchored {
                // An unanchored FIGURE caption stays a `<figure>` (the figure exists;
                // its graphic was dropped — an honest content-loss marker). An
                // unanchored TABLE caption is NOT a figure: emit it as a caption `<div>`
                // (the table itself was not detected nearby) — same element table
                // captions use, keeping the `tab-N` id so cross-refs still resolve.
                let block = if is_fig {
                    format!("<figure id=\"fig-{num}\"><figcaption>{html}</figcaption></figure>")
                } else {
                    format!("<div id=\"tab-{num}\">{html}</div>")
                };
                standalone.push((lines[idx].x0, cy, block));
            }
            for u in used {
                consumed_caption.insert(u);
            }
        }

        // Build items with bounding boxes, then order them column-aware via the
        // shared XY-cut so two-column pages don't interleave left/right blocks.
        let mut items: Vec<Item> = Vec::new();
        let mut boxes: Vec<text::BBox> = Vec::new();
        for (idx, l) in lines.iter().enumerate() {
            // A line whose text sits inside a vector figure is that figure's label
            // (now rendered as SVG <text>) — keep it out of the prose flow. But never
            // swallow a section heading that merely overlaps a figure's bbox.
            let fig_cx = (l.x0 + l.x1) * 0.5;
            let fig_label = (in_figure(fig_cx, l.y) || near_fig_label(fig_cx, l.y, l.size)) && detect_header(l, body).is_none();
            if !in_table(l.x0, l.x1, l.y) && !consumed_caption.contains(&idx) && !fig_label {
                items.push(Item::L(l));
                boxes.push((l.x0, l.x1.max(l.x0 + 0.1), l.y, l.y + l.size.max(1.0)));
            }
        }
        for (j, t) in tables.iter().enumerate() {
            items.push(Item::T(j));
            boxes.push((t.x_left, t.x_right.max(t.x_left + 0.1), t.y_bottom, t.y_top));
        }
        for (j, im) in images.iter().enumerate() {
            items.push(Item::Img(j));
            boxes.push((px0, px1, im.y_top - 1.0, im.y_top + 1.0)); // full-width separator
        }
        for (j, v) in vectors.iter().enumerate() {
            items.push(Item::Svg(j));
            boxes.push((v.x_left, v.x_right.max(v.x_left + 0.1), v.y_bottom, v.y_top));
        }
        for (j, (cx0, cy, _)) in standalone.iter().enumerate() {
            items.push(Item::Cap(j));
            boxes.push((*cx0, *cx0 + 0.1, *cy, *cy + body.max(1.0)));
        }
        let order = text::xy_cut_order(&boxes, body);
        let items: Vec<&Item> = order.iter().map(|&i| &items[i]).collect();

        // Emit, grouping consecutive lines into text blocks.
        let mut run: Vec<&Line> = Vec::new();
        let flush = |run: &mut Vec<&Line>, out: &mut String| {
            if !run.is_empty() {
                emit_lines(run, body, title_sz, out);
                run.clear();
            }
        };
        for it in &items {
            match it {
                Item::L(l) => run.push(l),
                Item::T(j) => {
                    flush(&mut run, &mut out);
                    match &tab_cap[*j] {
                        Some((num, cap, below)) => out.push_str(&table_html(&tables[*j], Some((num, cap, *below)))),
                        None => out.push_str(&table_html(&tables[*j], None)),
                    }
                }
                Item::Img(j) => {
                    flush(&mut run, &mut out);
                    let uri = &images[*j].uri;
                    match &img_cap[*j] {
                        Some((num, cap)) => out.push_str(&format!(
                            "<figure id=\"fig-{num}\"><img src=\"{uri}\" /><figcaption>{cap}</figcaption></figure>"
                        )),
                        None => out.push_str(&format!("<figure><img src=\"{uri}\" /></figure>")),
                    }
                }
                Item::Svg(j) => {
                    flush(&mut run, &mut out);
                    let svg = vectors[*j].svg();
                    match &svg_cap[*j] {
                        Some((num, cap)) => out.push_str(&format!(
                            "<figure id=\"fig-{num}\">{svg}<figcaption>{cap}</figcaption></figure>"
                        )),
                        None => out.push_str(&format!("<figure>{svg}</figure>")),
                    }
                }
                Item::Cap(j) => {
                    flush(&mut run, &mut out);
                    out.push_str(&standalone[*j].2);
                }
            }
        }
        flush(&mut run, &mut out);
        out.push_str("\n</section>\n");
    }
    out.push_str("</body>\n</html>\n");
    let body = dedup_ids(&merge_adjacent_figures(&merge_math_fragments(&merge_fragmented_lists(&merge_adjacent_links(&demote_running_headings(out))))));
    build_toc(body)
}

/// The document outline parsed from the auto-TOC: `(level, title, page, anchor-id)`
/// per heading, in document order. Drives section navigation/extraction.
pub fn toc(html: &str) -> Vec<(u8, String, u32, String)> {
    let mut out = Vec::new();
    let nav = match (html.find("<nav>"), html.find("</nav>")) {
        (Some(a), Some(b)) if b > a => &html[a..b],
        _ => return out,
    };
    let field = |h: &str, key: &str| -> Option<String> {
        let s = h.find(key)? + key.len();
        let e = h[s..].find('"')?;
        Some(h[s..s + e].to_string())
    };
    for li in nav.split("<li ").skip(1) {
        let level = field(li, "data-level=\"").and_then(|s| s.parse().ok()).unwrap_or(0u8);
        let page = field(li, "data-page=\"").and_then(|s| s.parse().ok()).unwrap_or(0u32);
        let id = field(li, "href=\"#").unwrap_or_default();
        let after_li = li.splitn(2, '>').nth(1).unwrap_or(""); // <a …>Label</a></li>
        let after_a = after_li.splitn(2, '>').nth(1).unwrap_or(""); // Label</a>…
        let label = strip_inline(after_a.split("</a>").next().unwrap_or(""));
        if !id.is_empty() {
            out.push((level, label.trim().to_string(), page, id));
        }
    }
    out
}

/// The HTML of one section: the heading whose id (or title) matches `name`, plus all
/// content up to the next heading of the same or higher level. `name` matches the
/// `sec-…` slug, an id prefix, or a case-insensitive title substring (so `section
/// ("abstract")` works). Returns None if no heading matches.
pub fn section(html: &str, name: &str) -> Option<String> {
    let entries = toc(html);
    let nl = name.to_lowercase();
    let want = {
        let s = format!("sec-{}", slug(&nl));
        s.trim_matches('-').to_string()
    };
    let idx = entries
        .iter()
        .position(|(_, t, _, i)| *i == want || i.starts_with(&want) || t.to_lowercase().contains(&nl))?;
    let level = entries[idx].0;
    let start = {
        let p = html.find(&format!("id=\"{}\"", entries[idx].3))?;
        html[..p].rfind("<h")?
    };
    let end = entries[idx + 1..]
        .iter()
        .find(|(l, _, _, _)| *l <= level)
        .and_then(|(_, _, _, nid)| html.find(&format!("id=\"{nid}\"")).map(|p| html[..p].rfind("<h").unwrap_or(p)))
        .unwrap_or_else(|| html.find("</body>").unwrap_or(html.len()));
    Some(html[start..end].trim().to_string())
}

/// Plain text of a fragment of inline HTML (drop tags, unescape the basic entities).
fn strip_inline(html: &str) -> String {
    let mut s = String::with_capacity(html.len());
    let mut intag = false;
    for c in html.chars() {
        match c {
            '<' => intag = true,
            '>' => intag = false,
            _ if !intag => s.push(c),
            _ => {}
        }
    }
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

/// Give every heading a stable `sec-*` anchor id and prepend an auto table of
/// contents. Page is the primary organiser (headings stay inside their
/// `<section data-page>`), so each TOC entry carries its page. A FLAT `<ol>` (level
/// recorded as `data-level`, not nested sub-lists) keeps the outline queryable and
/// avoids spurious single-item sub-lists. Runs last — after `dedup_ids` — so the ids
/// it mints are deduped against the final id set.
fn build_toc(html: String) -> String {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let b = html.as_bytes();
        let mut i = 0;
        while i + 4 < b.len() {
            if b[i..].starts_with(b"id=\"") {
                let s = i + 4;
                let mut e = s;
                while e < b.len() && b[e] != b'"' {
                    e += 1;
                }
                seen.insert(html[s..e].to_string());
                i = e;
            } else {
                i += 1;
            }
        }
    }
    let b = html.as_bytes();
    let mut out = String::with_capacity(html.len() + 512);
    let mut entries: Vec<(u8, String, u32, String)> = Vec::new(); // level, label, page, id
    let mut page = 0u32;
    let mut i = 0usize;
    let mut copied = 0usize;
    while i < b.len() {
        if b[i..].starts_with(b"data-page=\"") {
            let s = i + 11;
            let mut e = s;
            while e < b.len() && b[e] != b'"' {
                e += 1;
            }
            page = html[s..e].parse().unwrap_or(page);
            i = e;
            continue;
        }
        // A heading open tag `<hL>` (L in 1..=6) — `b[i]=='<'` so `i` is a char boundary.
        if b[i] == b'<' && i + 3 < b.len() && b[i + 1] == b'h' && (b'1'..=b'6').contains(&b[i + 2]) && b[i + 3] == b'>' {
            let level = b[i + 2] - b'0';
            let close = format!("</h{level}>");
            if let Some(rel) = html[i..].find(&close) {
                let inner = &html[i + 4..i + rel];
                let label = strip_inline(inner);
                let label = label.trim();
                if !label.is_empty() {
                    out.push_str(&html[copied..i]);
                    let base = {
                        let s = format!("sec-{}", slug(&label.to_lowercase()));
                        s.trim_matches('-').to_string()
                    };
                    let mut id = base.clone();
                    let mut k = 2;
                    while seen.contains(&id) {
                        id = format!("{base}-{k}");
                        k += 1;
                    }
                    seen.insert(id.clone());
                    entries.push((level, label.to_string(), page, id.clone()));
                    out.push_str(&format!("<h{level} id=\"{id}\">{inner}{close}"));
                    i += rel + close.len();
                    copied = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&html[copied..]);
    if entries.is_empty() {
        return out;
    }
    // Nested TOC: title (h1) and sections (h2) at the top level; subsections (h3)
    // indented under their section; deeper levels (h4+) omitted (too fine for an
    // outline). A `<ul>` (the outline order is positional, not enumerated). Each
    // <li> keeps data-level/data-page so the toc()/section() API still reads the
    // true heading level and page.
    let mut nav = String::from("<nav><ul>");
    let mut li_open = false; // a top-level <li> awaiting its </li>
    let mut sub_open = false; // a nested <ul> (h3 children) is open
    for (level, label, pg, id) in &entries {
        if *level > 3 {
            continue;
        }
        let a = format!("<a href=\"#{id}\">{}</a>", esc(label));
        let li = format!("<li data-level=\"{level}\" data-page=\"{pg}\">{a}");
        if *level <= 2 {
            if sub_open {
                nav.push_str("</ul>");
                sub_open = false;
            }
            if li_open {
                nav.push_str("</li>");
            }
            nav.push_str(&li);
            li_open = true;
        } else if li_open {
            // h3 → nest under the current section
            if !sub_open {
                nav.push_str("<ul>");
                sub_open = true;
            }
            nav.push_str(&li);
            nav.push_str("</li>");
        } else {
            // a stray h3 before any section: keep it at top level
            nav.push_str(&li);
            nav.push_str("</li>");
        }
    }
    if sub_open {
        nav.push_str("</ul>");
    }
    if li_open {
        nav.push_str("</li>");
    }
    nav.push_str("</ul></nav>\n");
    match out.find("<body>\n") {
        Some(p) => {
            let at = p + "<body>\n".len();
            let mut res = String::with_capacity(out.len() + nav.len());
            res.push_str(&out[..at]);
            res.push_str(&nav);
            res.push_str(&out[at..]);
            res
        }
        None => out,
    }
}

/// Merge a graphic-only `<figure>` immediately adjacent to a caption-only `<figure>`
/// (in either order) into one — the literal "graphic and caption split into two
/// figures" defect, resolved regardless of why caption-anchoring missed the pair.
fn merge_adjacent_figures(html: &str) -> String {
    fn take_figure(s: &str) -> Option<(&str, usize)> {
        if !s.starts_with("<figure") {
            return None;
        }
        s.find("</figure>").map(|e| (&s[..e + 9], e + 9))
    }
    // The opening tag attributes (after "<figure", before '>') and inner content.
    fn parts(fig: &str) -> (&str, &str) {
        let open_end = fig.find('>').map(|i| i + 1).unwrap_or(0);
        let attrs = fig["<figure".len()..open_end.saturating_sub(1)].trim();
        let inner = &fig[open_end..fig.len() - 9];
        (attrs, inner)
    }
    let is_graphic = |f: &str| f.contains("<img") || f.contains("<svg");
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while !rest.is_empty() {
        if let Some((f1, l1)) = take_figure(rest) {
            let tail = rest[l1..].trim_start();
            if let Some((f2, l2)) = take_figure(tail) {
                let (g1, c1) = (is_graphic(f1), f1.contains("<figcaption"));
                let (g2, c2) = (is_graphic(f2), f2.contains("<figcaption"));
                // one is graphic-only, the other caption-only → merge graphic + caption
                let pair = (g1 && !c1 && c2 && !g2) || (c1 && !g1 && g2 && !c2);
                if pair {
                    let (graphic, caption) = if g1 { (f1, f2) } else { (f2, f1) };
                    let (gattr, ginner) = parts(graphic);
                    let (cattr, cinner) = parts(caption);
                    let attr = if !cattr.is_empty() { cattr } else { gattr }; // keep the id (on the caption figure)
                    out.push_str("<figure");
                    if !attr.is_empty() {
                        out.push(' ');
                        out.push_str(attr);
                    }
                    out.push('>');
                    out.push_str(ginner);
                    out.push_str(cinner);
                    out.push_str("</figure>");
                    let consumed = (rest.len() - tail.len()) + l2;
                    rest = &rest[consumed..];
                    continue;
                }
            }
            out.push_str(f1);
            rest = &rest[l1..];
            continue;
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn strip_tags_inline(s: &str) -> String {
    let mut o = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => o.push(c),
            _ => {}
        }
    }
    o
}

/// A `<p>` whose content is a fragment of a DISPLAY EQUATION rather than prose: a
/// lone operator/number/punctuation, an equation number "(N)", a single symbol, or
/// a short run with math operators/Greek and ≤1 real word.
fn is_math_fragment(inner: &str) -> bool {
    let t = strip_tags_inline(inner);
    let t = t.trim();
    let n = t.chars().count();
    if t.is_empty() || n > 60 {
        return false;
    }
    let alpha_words = t.split(|c: char| !c.is_alphabetic()).filter(|w| w.chars().count() >= 2).count();
    if alpha_words > 1 {
        return false; // real prose
    }
    const MATH: &str = "=+-−–×÷·≤≥≠≈∝∫∑∏√∈∉⊂⊆∂∇∞()[]{}|/^_<>";
    const GREEK: &str = "αβγδεζηθικλμνξπρστυϕφχψωΓΔΘΛΞΠΣΦΨΩ";
    t.chars().any(|c| MATH.contains(c) || GREEK.contains(c)) || n <= 2 || t.chars().all(|c| !c.is_alphabetic())
}

/// Rejoin a display equation shattered into per-token `<p>`s: merge a RUN of ≥2
/// consecutive math-fragment `<p>`s (only whitespace between) into one `<p>` so the
/// equation is a single block and stray operators/numbers/commas stop being orphan
/// paragraphs. A lone fragment is left alone.
fn merge_math_fragments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with("<p>") {
            let mut frags: Vec<&str> = Vec::new();
            let mut cursor = rest;
            loop {
                let c2 = cursor.trim_start();
                if let Some(body) = c2.strip_prefix("<p>") {
                    if let Some(rel) = body.find("</p>") {
                        let inner = &body[..rel];
                        if is_math_fragment(inner) {
                            frags.push(inner);
                            let adv = (cursor.len() - c2.len()) + 3 + rel + 4;
                            cursor = &cursor[adv..];
                            continue;
                        }
                    }
                }
                break;
            }
            if frags.len() >= 2 {
                out.push_str("<p>");
                out.push_str(&frags.join(" "));
                out.push_str("</p>");
                rest = cursor;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Whether a `<p>` between two same-type lists INTRODUCES the following list (its text
/// ends with ':', e.g. "The second procedure has these steps:"). Such a line is a real
/// separator — the next list is its own list, not a fragment of the previous one — so
/// the two must not be fused. A genuine wrapped continuation of the last item never ends
/// with a colon.
fn introduces_list(inner: &str) -> bool {
    let mut t = String::new();
    let mut depth = 0i32;
    for ch in inner.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = (depth - 1).max(0),
            _ if depth == 0 => t.push(ch),
            _ => {}
        }
    }
    t.trim_end().ends_with(':')
}

/// Rejoin a list fragmented into single-item lists: `…A</li></ul> <p>cont</p>… <ul><li>B…`
/// becomes `…A cont…</li><li>B…`. The intervening `<p>`s are the wrapped continuation
/// of item A that the line loop couldn't attach (flush-left wrap, column break). Only
/// fires for same-type adjacent lists with a few short continuation paragraphs between,
/// and never across a `<p>` that introduces the next list (ends with ':') — so two real
/// lists, the second introduced by a lead-in line, are left separate.
fn merge_fragmented_lists(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["ul", "ol"] {
        let close = format!("</li></{tag}>");
        let open_li = format!("<{tag}><li>");
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i..].starts_with(&close) {
                // After the close: optional whitespace + up to 3 short <p>…</p> blocks
                // (the wrapped continuation), then the SAME-type list reopening.
                let mut k = i + close.len();
                let mut conts: Vec<&str> = Vec::new();
                let mut ok = true;
                loop {
                    while k < s.len() && s.as_bytes()[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    if s[k..].starts_with(&open_li) {
                        break;
                    }
                    if conts.len() < 3 {
                        if let Some(body) = s[k..].strip_prefix("<p>") {
                            if let Some(rel) = body.find("</p>") {
                                // A lead-in line ("… steps:") separates two real lists;
                                // never fold across it.
                                if rel < 400 && !introduces_list(&body[..rel]) {
                                    conts.push(&body[..rel]);
                                    k += 3 + rel + 4;
                                    continue;
                                }
                            }
                        }
                    }
                    ok = false;
                    break;
                }
                if ok && s[k..].starts_with(&open_li) {
                    out.push(' ');
                    out.push_str(&conts.join(" "));
                    out.push_str("</li><li>");
                    i = k + open_li.len();
                    continue;
                }
            }
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        s = out;
    }
    s
}

/// Guarantee unique `id=` attributes: the first use of an id keeps it, later uses
/// are suffixed ("tab-3" → "tab-3-2"). The inline-cross-reference guard removes the
/// common cause (phantom figures), but genuine same-number elements (sub-tables on
/// different pages) can still collide — an HTML document must not repeat an id.
fn dedup_ids(html: &str) -> String {
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("id=\"") {
        out.push_str(&rest[..pos + 4]);
        rest = &rest[pos + 4..];
        let end = match rest.find('"') {
            Some(e) => e,
            None => break,
        };
        let id = &rest[..end];
        let n = seen.entry(id.to_string()).or_insert(0);
        *n += 1;
        if *n == 1 {
            out.push_str(id);
        } else {
            out.push_str(&format!("{id}-{n}"));
        }
        out.push('"');
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Collapse adjacent same-href anchors into one: a citation/URL split across styled
/// runs or line breaks ("Rad"+"ford", a wrapped DOI) emits `…</a><a href="H">…`.
/// When the just-closed `<a>` and the next opening `<a>` share the same href, drop
/// the boundary (keeping any whitespace) so the link is a single atomic anchor.
fn merge_adjacent_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut open_href: Option<String> = None;
    let mut rest = s;
    let read_anchor = |t: &str| -> Option<(String, usize)> {
        // (href, total tag length) for a leading `<a href="...">`
        let body = t.strip_prefix("<a href=\"")?;
        let q = body.find('"')?;
        let gt = body[q..].find('>')?;
        Some((body[..q].to_string(), 9 + q + gt + 1))
    };
    while !rest.is_empty() {
        if let Some((href, len)) = read_anchor(rest) {
            out.push_str(&rest[..len]);
            open_href = Some(href);
            rest = &rest[len..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("</a>") {
            let ws = after.len() - after.trim_start().len();
            if let (Some(cur), Some((href2, len2))) = (&open_href, read_anchor(after[ws..].as_ref())) {
                if &href2 == cur {
                    // merge: keep whitespace, drop the </a> and the reopening <a>
                    out.push_str(&after[..ws]);
                    rest = &after[ws + len2..];
                    continue;
                }
            }
            out.push_str("</a>");
            open_href = None;
            rest = after;
            continue;
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Doc-level pass: a real section title appears once, but a running page header
/// (the paper title or author list repeated atop every page) gets emitted as a
/// heading on each page. Any heading whose text (minus a leading page/section
/// number) recurs 3+ times across the document is a running head — demote those
/// occurrences from `<hN>` to `<p>` so they don't pollute the heading outline.
fn demote_running_headings(html: String) -> String {
    // Collect (range, inner) for every <h1-6>…</h1-6>.
    let bytes = html.as_bytes();
    let mut spans: Vec<(usize, usize, usize, usize, String)> = Vec::new(); // open,close_end,lvl, inner_start,inner
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'h' && matches!(bytes[i + 2], b'1'..=b'6') && bytes[i + 3] == b'>' {
            let lvl = (bytes[i + 2] - b'0') as usize;
            let close = format!("</h{lvl}>");
            if let Some(rel) = html[i..].find(&close) {
                let inner_start = i + 4;
                let inner = html[inner_start..i + rel].to_string();
                spans.push((i, i + rel + close.len(), lvl, inner_start, inner));
                i += rel + close.len();
                continue;
            }
        }
        i += 1;
    }
    // Count normalized keys (strip tags + a leading number/roman/letter token).
    let key = |inner: &str| -> String {
        let text: String = {
            let mut s = String::new();
            let mut intag = false;
            for c in inner.chars() {
                match c {
                    '<' => intag = true,
                    '>' => intag = false,
                    _ if !intag => s.push(c),
                    _ => {}
                }
            }
            s
        };
        let t = text.trim_start();
        // drop a leading "12 ", "3.2.1", "IV.", "A." token
        let t = t.trim_start_matches(|c: char| c.is_alphanumeric() || c == '.' );
        t.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for s in &spans {
        let k = key(&s.4);
        if k.len() >= 4 {
            *counts.entry(k).or_insert(0) += 1;
        }
    }
    // Rebuild, demoting repeated ones to <p>.
    let mut outp = String::with_capacity(html.len());
    let mut pos = 0;
    let mut kept_h1: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (open, close_end, lvl, _is, inner) in &spans {
        let k = key(inner);
        if counts.get(&k).copied().unwrap_or(0) >= 3 {
            // A heading repeated ≥3× is a running page-header — demote to <p>. But the
            // document title legitimately recurs in the running head: keep its first
            // <h1> occurrence (the real title) and demote every other repeat.
            if *lvl == 1 && kept_h1.insert(k) {
                continue;
            }
            outp.push_str(&html[pos..*open]);
            outp.push_str("<p>");
            outp.push_str(inner);
            outp.push_str("</p>");
            pos = *close_end;
        }
    }
    outp.push_str(&html[pos..]);
    outp
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
        angle: s.angle,
    }
}

