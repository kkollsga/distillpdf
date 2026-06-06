//! "AI-ready" HTML conversion: thin, semantic HTML from a PDF.
//!
//! Per-page <section>; headings (font-size clustering), bold/italic (font flags),
//! lists, monospace/code, tables (positioned), inline embedded images. No CSS,
//! no classes, no script — minimal tags for LLM consumption.

use crate::captions::*;
use crate::extract::{self, PosTable};
use crate::frontmatter::{Author, FrontMatter};
use crate::img;
use crate::links;
use crate::nav::*;
use crate::postprocess::*;
use crate::profile::{DocProfile, HeadingTier};
use crate::text::{self, Span};
use crate::vector;
use lopdf::{Document, ObjectId};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

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
pub(crate) fn slug(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect()
}

/// A caption number ("6.2.1", "0.1-1", "1A") slugged for use in an element id: separators
/// become '-' and it is lowercased, so the displayed caption keeps the real "6.2.1" while
/// the id is a clean `fig-6-2-1`. Distinct labels map to distinct ids (no truncation
/// collisions); any residual collision is still handled by dedup_ids.
fn num_id<S: AsRef<str>>(num: S) -> String {
    num.as_ref().chars().map(|c| if c == '.' { '-' } else { c.to_ascii_lowercase() }).collect()
}

/// href of the link whose rectangle contains point `(x, y)`, if any.
fn href_at(x: f32, y: f32, links: &[LinkBox]) -> Option<String> {
    links
        .iter()
        .find(|l| x >= l.rect[0] && x <= l.rect[2] && y >= l.rect[1] && y <= l.rect[3])
        .map(|l| l.href.clone())
}

pub(crate) fn esc(s: &str) -> String {
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

// The line/run layout types are the shared currency between html.rs and the extracted
// feature modules (captions, …), so they're pub(crate).
pub(crate) struct Run {
    pub(crate) text: String,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) href: Option<String>,
    pub(crate) script: i8, // 0 = baseline, 1 = superscript (<sup>), -1 = subscript (<sub>)
}

pub(crate) struct Line {
    pub(crate) y: f32,
    pub(crate) x0: f32,
    pub(crate) x1: f32,
    pub(crate) size: f32,
    pub(crate) mono: bool,
    pub(crate) mono_w: usize,
    pub(crate) tot_w: usize,
    pub(crate) runs: Vec<Run>,
    /// Dominant font-face id (the line's first non-space span); 0 = unknown. Lets the
    /// style profile recognise a heading face that differs from body at the same size.
    pub(crate) font: u32,
}

impl Line {
    pub(crate) fn text(&self) -> String {
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
            // A small glyph sitting among OTHER small glyphs is part of a dense math run
            // (stacked sub/superscripts, an inline/display formula); an ISOLATED script is
            // a lone footnote marker / exponent among base-size text.
            let in_cluster = spans.iter().enumerate().any(|(j, t)| {
                j != i && t.size < base_sz && (t.x - s.x).abs() < avg * 3.0 && (t.y - s.y).abs() < band * 1.5
            });
            // Find the base glyph this script attaches to (nearest horizontally-adjacent
            // base-size glyph in the sub/superscript offset window), recording the host's
            // size and weight (to keep cluster re-basing out of headings).
            let mut best: Option<(f32, f32, f32, bool)> = None; // (|dx|, base y, base size, base bold)
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
                if best.is_none_or(|(bd, _, _, _)| adx < bd) {
                    best = Some((adx, t.y, t.size, t.bold));
                }
            }
            match best {
                // No adjacent base — leave the offset glyph where it is.
                None => s.y,
                // In a dense math cluster, re-base onto the host ONLY when it lives in a
                // FORMULA row (display/inline math: symbol-dominated), so the orphaned
                // index fragments (i, j, K, ∂Σ …) rejoin their equation line instead of
                // floating into a spurious <aside>. NEVER re-base a cluster whose host
                // sits in a WORD-dominated row — that captures section headings the
                // classifier finds structurally even though they are body-sized and
                // unbolded ("H.2. … Suboptimal O(T^{3/4}) Regret from Biased Gradients",
                // whose exponent was being pulled up into the title) as well as ordinary
                // prose. Larger-font / bold hosts are excluded outright. An ISOLATED
                // script keeps the prior, unconditional behaviour (footnote markers).
                Some((_, by, bsz, bbold)) => {
                    let rebase = if in_cluster {
                        if bsz > avg * 1.25 || bbold {
                            false
                        } else {
                            // Is the host's row a prose/heading line (mostly real words)
                            // or a formula line (mostly symbols/digits)? A word is a span
                            // carrying a 2+ letter run.
                            let is_word = |t: &str| {
                                let mut run = 0;
                                t.chars().any(|c| {
                                    if c.is_alphabetic() {
                                        run += 1;
                                        run >= 2
                                    } else {
                                        run = 0;
                                        false
                                    }
                                })
                            };
                            let (mut row_base, mut row_words) = (0usize, 0usize);
                            for t in spans.iter() {
                                if t.size >= base_sz && !t.text.trim().is_empty() && (t.y - by).abs() < band {
                                    row_base += 1;
                                    if is_word(t.text.trim()) {
                                        row_words += 1;
                                    }
                                }
                            }
                            let wordy_row = row_base >= 4 && row_words * 2 >= row_base;
                            !wordy_row
                        }
                    } else {
                        true
                    };
                    if rebase {
                        by
                    } else {
                        s.y
                    }
                }
            }
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
    // Span-level prose ordering: enable the crossing-tolerant column gutter so a centered
    // page number / running header in a tight two-column gutter doesn't force the columns to
    // interleave line-by-line.
    let order = text::xy_cut_order_opt(&boxes, avg.max(1.0), true);
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
            let band_changed = cur_band.is_none_or(|cb| (cb - b).abs() >= 0.5);
            // Break on a huge horizontal gap (a column gutter) so left/right
            // columns at the same y don't merge — split occurs between words,
            // never within one, so no fragmentation.
            let huge_gap = cur_band.is_some() && (s.x - prev_end) > (s.size * 4.0).max(36.0);
            // A script glyph stays on the current line (don't break, don't move the
            // band) so the following baseline glyph rejoins the same line.
            if (band_changed || huge_gap) && script == 0 {
                lines.push(Line { y: s.y, x0: s.x, x1: s.x, size: s.size, mono: false, mono_w: 0, tot_w: 0, runs: Vec::new(), font: s.font });
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
    // A line that is ONLY trailing punctuation (a lone "," / "." left behind when a
    // display equation's glyphs reorder) is never a paragraph of its own — fold it onto
    // the end of the preceding line so it doesn't surface as a stray <p>,</p>.
    let line_text = |l: &Line| l.runs.iter().map(|r| r.text.as_str()).collect::<String>();
    let punct_only = |t: &str| {
        let t = t.trim();
        !t.is_empty() && t.chars().all(|c| ",.;:".contains(c))
    };
    let mut i = 1;
    while i < lines.len() {
        if punct_only(&line_text(&lines[i])) {
            let glued = line_text(&lines[i]).trim().to_string();
            if let Some(r) = lines[i - 1].runs.last_mut() {
                while r.text.ends_with(' ') {
                    r.text.pop();
                }
                r.text.push_str(&glued);
                lines.remove(i);
                continue;
            }
        }
        i += 1;
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
pub(crate) fn render_runs(runs: &[Run]) -> String {
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

pub(crate) fn list_kind(text: &str) -> Option<bool> {
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
    if j == i + 1 || !b.get(j).is_some_and(|c| (*c as char).is_alphabetic()) {
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

/// True when a text run reads as a figure's axis label: numeric-DOMINATED (most
/// whitespace tokens are short numeric ticks) — covering a lone tick (`"1000"`), a full
/// tick row (`"1000 2000 … 7000"`), and an axis title fused to its ticks
/// (`"Vp (m/s) 1000 … 7000"`) — or a lone unit-bearing axis title (`"Vp(m/s)"`, `"BSF(m)"`).
/// Rejects the multi-word `"Figure N:"` caption and ordinary prose (mostly word tokens),
/// so body-size axis labels can be pulled into a figure without swallowing prose.
fn is_axis_label_text(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let toks: Vec<&str> = t.split_whitespace().collect();
    let is_tick = |tok: &str| !tok.is_empty() && tok.len() <= 6 && tok.chars().all(|c| c.is_ascii_digit() || ".,-–%".contains(c));
    let nnum = toks.iter().filter(|tok| is_tick(tok)).count();
    let numeric_row = nnum >= 1 && nnum * 2 >= toks.len();
    // A short unit-bearing axis title — "Vp(m/s)", "BSF(m)", or the spaced "Vp (m/s)".
    let axis_title = toks.len() <= 2 && t.len() <= 14 && t.contains('(') && t.contains(')');
    numeric_row || axis_title
}

/// Paragraph-aware header detection. A header is a numbered section line, a
/// canonical top-level name, a standalone bold/larger line, or a **bold run-in
/// lead** at the top of a paragraph. Returns `(level, n_leading_runs)`: when
/// `n_leading_runs < line.runs.len()` the header is a run-in lead and the
/// remaining runs continue as that paragraph's body. Level comes from the
/// section number when present, else from font size.
/// SEC filing structure anchor: "PART I"–"PART VIII" (→ level 1) or "Item 1"/"Item 1A"/
/// "Item 10" (→ level 2), with or without a trailing title. Returns the heading level.
/// These are the authoritative section markers in 10-K/S-1 filings (which ship no PDF
/// outline), so they are recognised even though their styling matches non-heading lines.
fn sec_anchor(trimmed: &str) -> Option<u8> {
    let low = trimmed.to_lowercase();
    // What follows the anchor token must be empty, a separator, or a capitalised title —
    // never a lowercase sentence continuation ("Part I of Form W-9. If you are …"), which
    // is prose that merely mentions the part, not the heading.
    let title_ok = |rest: &str| {
        let r = rest.trim_start();
        r.is_empty() || r.starts_with([':', '.', '—', '-']) || r.chars().next().is_some_and(|c| !c.is_ascii_lowercase())
    };
    if low.starts_with("part ") {
        let after = trimmed["part ".len()..].trim_start();
        let tl = after.find(char::is_whitespace).unwrap_or(after.len());
        let tok = after[..tl].trim_end_matches([':', '.', '—', '-', ',']);
        if (1..=5).contains(&tok.len()) && tok.chars().all(|c| "IVXivx".contains(c)) && title_ok(&after[tl..]) {
            return Some(1);
        }
    }
    if low.starts_with("item ") {
        let after = trimmed["item ".len()..].trim_start();
        let tl = after.find(|c: char| c.is_whitespace() || c == '.' || c == ':').unwrap_or(after.len());
        let tok = &after[..tl];
        let digits = tok.chars().take_while(|c| c.is_ascii_digit()).count();
        let suffix: String = tok.chars().skip_while(|c| c.is_ascii_digit()).collect();
        if (1..=2).contains(&digits)
            && (suffix.is_empty() || (suffix.len() <= 2 && suffix.chars().all(|c| c.is_ascii_alphabetic())))
            && title_ok(&after[tl..])
        {
            return Some(2);
        }
    }
    None
}

/// A line that reads as a CLAUSE or value phrase, not a heading — used to reject body
/// fragments that happen to match a heading STYLE ("158 GeV at 95% confidence level have
/// been set", "600 GeV), largely due to …"). Signals: a trailing comma, an internal
/// sentence boundary (". " + capital/'('), a mid-line lowercase finite verb, or a leading
/// number+unit followed by a lowercase continuation. Only applied to multi-word lines, so
/// noun-phrase headings ("System Architecture", "Data Set") are untouched.
fn looks_like_clause(trimmed: &str) -> bool {
    if trimmed.ends_with(',') {
        return true;
    }
    // Strip a leading section-number token ("1. ", "1.2 ", "II. ", "A. ") so its
    // separator dot is not mistaken for an internal sentence boundary ("II. METHODS").
    let core = {
        let t = trimmed.trim_start();
        match t.find(char::is_whitespace) {
            Some(sp) if t[..sp].ends_with('.') => {
                let head = t[..sp].trim_end_matches('.');
                let numbery = !head.is_empty()
                    && (head.chars().all(|c| c.is_ascii_digit() || c == '.')
                        || head.chars().all(|c| "IVXLCDM".contains(c))
                        || (head.len() == 1 && head.chars().all(|c| c.is_ascii_uppercase())));
                if numbery { t[sp..].trim_start() } else { t }
            }
            _ => t,
        }
    };
    let chars: Vec<char> = core.chars().collect();
    for w in chars.windows(3) {
        if w[0] == '.' && w[1] == ' ' && (w[2].is_uppercase() || w[2] == '(') {
            return true; // internal sentence boundary
        }
    }
    let toks: Vec<&str> = trimmed.split_whitespace().collect();
    const VERBS: &[&str] = &["is", "are", "was", "were", "have", "has", "had", "been", "be", "set", "shows", "show", "due", "using", "based"];
    if toks.iter().skip(1).any(|t| {
        let w = t.trim_matches(|c: char| !c.is_alphabetic());
        !w.is_empty() && w.chars().all(|c| c.is_lowercase()) && VERBS.contains(&w)
    }) {
        return true; // mid-line lowercase finite verb
    }
    const UNITS: &[&str] = &["gev", "tev", "mev", "kev", "ev", "kg", "mhz", "ghz", "khz", "hz", "nm", "mm", "cm", "km", "ms"];
    if toks.first().is_some_and(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        && toks.get(1).is_some_and(|t| UNITS.contains(&t.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase().as_str()))
        && toks.iter().skip(2).any(|t| t.chars().next().is_some_and(|c| c.is_lowercase()))
    {
        return true; // leading value+unit with a lowercase continuation
    }
    false
}

/// How confidently a line was judged a heading, used to decide whether it must ALSO
/// be positionally isolated before it is emitted. `Reliable` (numbered / canonical /
/// Appendix) and `RunIn` (a bold in-line lead) are pattern-anchored and trustworthy on
/// their own; `Style` (a profiled size/weight tier, or merely bold / larger than body)
/// is the over-firing class — an address block, affiliation line, equation fragment or
/// axis label can match a style without being a heading — so a `Style` verdict is only
/// honoured when the line is positionally isolated (see `header_at`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadingKind {
    Reliable,
    Style,
    RunIn,
}

pub(crate) fn detect_header(line: &Line, body: f32, profile: Option<&DocProfile>) -> Option<(u8, usize, HeadingKind)> {
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
    // An author/collaboration byline ("The ATLAS Collaboration", "LIGO Scientific
    // Consortium") is set like a heading but is not one: a ≥2-word line ending in a
    // collaboration marker.
    let collab_byline = (2..=6).contains(&words)
        && matches!(
            trimmed.split_whitespace().last().unwrap_or("").trim_end_matches(['.', ',', ':']).to_lowercase().as_str(),
            "collaboration" | "collaborations" | "consortium"
        );
    if trimmed.chars().filter(|c| c.is_alphabetic()).count() < 2
        || low.contains("et al")
        || low.starts_with("arxiv")
        || has_year(trimmed)
        || initials_count(trimmed) >= 2
        || collab_byline
    {
        return None;
    }

    // Mis-extracted display MATH promoted to a heading. A heading is real words; an
    // equation fragment is symbols, single letters and digits. `real_words` = tokens
    // with ≥2 ASCII letters.
    //   • ONE real word: reject only on a relation ("T ∆² ≤ Õ(…)") — the original guard.
    //   • NO real words: reject when dominated by math symbols, single-char tokens or
    //     digits ("K ρ c", "∂ t ∂ z ∂ z", "⎢ ⎝350⎠ d ⎥ s"). Gated on real_words==0 so a
    //     numbered heading ("5 Conclusions", real_words=1) is never caught here.
    let real_words = trimmed.split_whitespace().filter(|w| w.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 2).count();
    let relational = trimmed.chars().any(|c| matches!(c, '=' | '≤' | '≥' | '≡' | '≈' | '←' | '→' | '∝' | '≪' | '≫' | '∑' | '∈'));
    // Math operators / fences / Greek (NOT the ascii hyphen, which is a word joiner).
    let math = trimmed.chars().filter(|&c| {
        "=≤≥≡≈≠<>+←→↔⇒↦∝≪≫∑∏∫∂∇√∈∉±×÷·∞⎢⎝⎠⎥⎜⎟|∆−".contains(c)
            || "αβγδεζηθικλμνξπρςστυϕφχψωΓΔΘΛΞΠΣΦΨΩ".contains(c)
    }).count();
    if real_words == 0 {
        // No real word: an equation fragment is symbols, single letters or digits
        // ("K ρ c", "∂ t ∂ z ∂ z", "⎢ ⎝350⎠ d ⎥ s"). (A numbered heading "5 Conclusions"
        // is real_words=1, never reaching here.)
        let toks: Vec<&str> = trimmed.split_whitespace().collect();
        let ntok = toks.len().max(1);
        let single = toks.iter().filter(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).chars().count() <= 1).count();
        let nonspace = trimmed.chars().filter(|c| !c.is_whitespace()).count().max(1);
        let digits = trimmed.chars().filter(|c| c.is_ascii_digit()).count();
        if relational || math >= 1 || single * 2 >= ntok || digits as f32 > nonspace as f32 * 0.4 {
            return None;
        }
    } else if real_words == 1 && relational {
        return None;
    }
    // Symbol-soup equation even WITH a few (often mis-extracted) word-like tokens
    // ("K ρ zT ,ρ= ρ= K − ρρ −+ρ BTe"): math operators dominate over real words. The
    // `>= 3` floor and `>= real_words + 2` margin spare genuine headings that carry a
    // symbol or two ("H → ZZ → 4ℓ channel", "β-decay Measurements").
    if math >= 3 && math >= real_words + 2 {
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

    // 0. SEC filing structure anchors ("PART I", "Item 1A. Risk Factors"). These carry
    //    the real section structure of a 10-K/S-1 that has no PDF outline and whose tier
    //    styling is shared with hundreds of non-heading lines (so the global distrust pass
    //    would otherwise drop them). Recognising them as Reliable keeps them at the right
    //    level and exempts them from the style-frequency distrust.
    if words <= 16 {
        if let Some(lvl) = sec_anchor(trimmed) {
            return Some((lvl, line.runs.len(), HeadingKind::Reliable));
        }
    }
    // 1. Numbered section header — level from numbering depth. The "N.N Title"
    //    shape (number, space, capitalised title with a real word) is the signal;
    //    no bold needed (many heading faces aren't flagged bold). This also rejects
    //    inline math like "1 N".
    if words <= 12 && !looks_like_reference(trimmed) {
        if let Some(lvl) = numbered_level(trimmed).or_else(|| roman_section(trimmed)) {
            let title = trimmed.split_once(char::is_whitespace).map(|x| x.1).unwrap_or("");
            let title_upper = title.chars().find(|c| c.is_alphabetic()).is_some_and(|c| c.is_uppercase());
            let alpha = title.chars().filter(|c| c.is_alphabetic()).count();
            // Footnotes ("2 We do not show halo … because …") carry a leading number
            // like a section, but are set SMALLER than body text; a real numbered
            // heading is body-size or larger. Reject sub-body-size numbered lines.
            // Also reject a measured VALUE mis-read as a section number ("158 GeV at 95%
            // confidence level …", "7 TeV at the LHC …"): the leading "number" is a
            // measurement whose next token is a unit, not a section title. (Such a line,
            // if it is heading-styled at all, is then caught downstream by the style
            // path's clause guard; otherwise it stays body text.)
            let value_unit = {
                let toks: Vec<&str> = trimmed.split_whitespace().collect();
                const UNITS: &[&str] = &["gev", "tev", "mev", "kev", "ev", "kg", "mhz", "ghz", "khz", "hz", "nm", "mm", "cm", "km", "ms"];
                toks.get(1).is_some_and(|t| UNITS.contains(&t.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase().as_str()))
            };
            if title_upper && alpha >= 3 && ratio >= 0.92 && !value_unit {
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
                        return Some((lvl, e, HeadingKind::Reliable));
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
                return Some((lvl, line.runs.len(), HeadingKind::Reliable));
            }
        }
    }
    // 2. Canonical unnumbered top-level header. A line that is *exactly* a
    //    canonical name (≤2 words, trailing '.'/':'/'—' allowed) is unambiguously
    //    a heading even at body size & non-bold (common in math/physics styles).
    let canon = low.trim_end_matches(['.', ':', '—', '-', ' ']);
    if trimmed.split_whitespace().count() <= 2 && TOP_HEADS.contains(&canon) {
        return Some((1, line.runs.len(), HeadingKind::Reliable));
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
                        return Some((1, ri + 1, HeadingKind::Reliable));
                    }
                }
            }
            return Some((1, line.runs.len(), HeadingKind::Reliable));
        }
    }
    // 2.5 Profiled heading tier: a short, capitalised line whose size·weight·font matches
    //     a heading style the document analysis identified — catches faces only slightly
    //     larger than body (or body-size in a distinct font) that the fixed 1.18× ratio
    //     below would miss. A trailing ':' is allowed (a labelled section head); '.'/','/';'
    //     are not. Numbered/canonical paths above still win first.
    if let Some(prof) = profile {
        if words <= 16
            && !trimmed.ends_with(['.', ',', ';'])
            && !looks_like_reference(trimmed)
            && !(real_words >= 2 && looks_like_clause(trimmed))
            && trimmed.chars().next().is_some_and(|c| !c.is_lowercase())
        {
            if let Some(level) = prof.heading_level(line.size, line.font, all_bold) {
                return Some((level, line.runs.len(), HeadingKind::Style));
            }
        }
    }
    // 3. Standalone bold (or visibly larger) short line.
    if words <= 12
        && (all_bold || ratio >= 1.18)
        && !trimmed.ends_with(['.', ',', ';', ':'])
        && !looks_like_reference(trimmed)
        && !(real_words >= 2 && looks_like_clause(trimmed))
        && trimmed.chars().next().is_some_and(|c| !c.is_lowercase())
    {
        return Some((size_level(ratio), line.runs.len(), HeadingKind::Style));
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
        // A lead ending in a comma is a continued enumeration — a bold author name at
        // the head of an author list ("**Chris Dart,** Anne-Lise Lysholm, …"), not a
        // section head. Headings never end in a comma.
        if cap_lead
            && !lead.ends_with(',')
            && ((2..=8).contains(&lead_words)
                || (labelled && lead.chars().filter(|c| c.is_alphabetic()).count() >= 2))
        {
            return Some((3, k, HeadingKind::RunIn));
        }
    }
    None
}

/// The gated heading decision used at the prose/list emission sites. A `Style` verdict
/// from `detect_header` is dropped when its style signature was DISTRUSTED by the global
/// pre-detection pass (an over-used emphasis/label style — see plan_headings).
/// `Reliable` / `RunIn` verdicts (numbered, canonical, SEC `Item`/`PART`, run-ins) bypass
/// the distrust, so real structure survives even in a filing whose headings share styling
/// with hundreds of line items.
fn header_at(lines: &[&Line], i: usize, body: f32, profile: &DocProfile, plan: &HeadingPlan) -> Option<(u8, usize)> {
    let (lvl, k, kind) = detect_header(lines[i], body, Some(profile))?;
    match kind {
        HeadingKind::Reliable | HeadingKind::RunIn => Some((lvl, k)),
        HeadingKind::Style => {
            if plan.distrusted.contains(&style_sig(lines[i])) {
                None
            } else {
                Some((lvl, k))
            }
        }
    }
}

/// True when a whole line is set in a heading style the document profile identified.
fn line_in_heading_tier(l: &Line, profile: &DocProfile) -> bool {
    let all_bold = !l.runs.is_empty() && l.runs.iter().all(|r| r.text.trim().is_empty() || r.bold);
    profile.heading_level(l.size, l.font, all_bold).is_some()
}

/// Fold a heading that wraps across lines into one `Line`. A two-line section title (same
/// heading tier, vertically adjacent, same indent — the second line is the continuation,
/// often lowercase or following a first line that ends ':') becomes a single heading so it
/// is emitted as one `<hN>` rather than a heading plus an orphaned body paragraph. Only
/// heading-tier lines are merged, so body text and separated headings are never joined.
fn merge_heading_continuations(lines: &mut Vec<Line>, profile: &DocProfile) {
    if profile.tiers.is_empty() {
        return;
    }
    let words = |l: &Line| l.text().split_whitespace().count();
    let mut i = 0;
    while i + 1 < lines.len() {
        if line_in_heading_tier(&lines[i], profile) && words(&lines[i]) <= 16 {
            while i + 1 < lines.len() {
                let a = &lines[i];
                let b = &lines[i + 1];
                let same_tier = a.size.round() == b.size.round() && a.font == b.font;
                let adjacent = (a.y - b.y).abs() < a.size * 1.8;
                let same_indent = (b.x0 - a.x0).abs() < a.size * 1.5;
                let next_pagenum = b.text().trim().chars().last().is_some_and(|c| c.is_ascii_digit());
                let combined = words(a) + words(b) <= 16;
                if same_tier && adjacent && same_indent && combined && !next_pagenum && line_in_heading_tier(b, profile) {
                    let nxt = lines.remove(i + 1);
                    if let Some(last) = lines[i].runs.last_mut() {
                        if !last.text.ends_with(' ') {
                            last.text.push(' ');
                        }
                    }
                    lines[i].x1 = lines[i].x1.max(nxt.x1);
                    lines[i].tot_w += nxt.tot_w;
                    lines[i].runs.extend(nxt.runs);
                } else {
                    break;
                }
            }
        }
        i += 1;
    }
}

/// Render a positioned table. A caption (when present) is emitted as the table's
/// own `<caption>` and the anchor id goes on the `<table>` — a table is tabular
/// data, not a figure, so it is NOT wrapped in `<figure>`.
fn table_html(t: &PosTable, cap: Option<(&str, &str, bool)>) -> String {
    let mut tbl = match cap {
        Some((num, _, _)) => format!("<table id=\"tab-{}\">", num_id(num)),
        None => String::from("<table>"),
    };
    // Caption as the table's own `<caption>` (the required first child) so it is
    // semantically LINKED to the table for an LLM reader — a sibling block can't be
    // unambiguously associated. `caption-side:bottom` (inline style; no class — the HTML
    // stays thin) preserves a caption that sits below the table in the source.
    if let Some((_, caption, below)) = cap {
        if below {
            tbl.push_str(&format!("<caption style=\"caption-side:bottom\">{caption}</caption>"));
        } else {
            tbl.push_str(&format!("<caption>{caption}</caption>"));
        }
    }
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
    tbl
}

/// Mark which lines belong to a page-bottom footnote block (see emit_lines). A run of
/// >=2 consecutive footnote-sized lines (`size < body*0.86`) confined to the bottom ~45%
/// > of the content, capped at 12 lines so a small-font reference list isn't swallowed.
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
fn emit_lines(lines: &[&Line], body: f32, title_sz: f32, promote: &[String], profile: &DocProfile, plan: &HeadingPlan, out: &mut String) {
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
        // A line that exactly matches a PDF-outline title for this page is a section
        // title by the author's own bookmark — promote it even when it carries no visual
        // heading cue (some docs set abstract/section titles at body size). Page-scoped,
        // so the contents page's TOC entries (different page) are never affected.
        // A forced match is an author-declared SECTION title — emit it at section level
        // (so it lands in the TOC and the outline link resolves), overriding any lower
        // level `detect_header` would infer from its (often body-size) styling.
        // Cap at 12 words: a longer match is a multi-line/sentence-like title — promoting
        // it would read as a sentence heading. Those stay plain text in the outline nav.
        let forced = !promote.is_empty()
            && txt.split_whitespace().count() <= 12
            && promote.iter().any(|k| *k == title_key(&txt));
        if !in_enumerated_run(lines, i) && !colon_introduced_list(lines, i) {
        if let Some((lvl, k)) = if forced { Some((1, ln.runs.len())) } else { header_at(lines, i, body, profile, plan) } {
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
                && (in_enumerated_run(lines, i) || colon_introduced_list(lines, i) || header_at(lines, i, body, profile, plan).is_none())
            {
                let marker_x = lines[i].x0;
                let mut item = strip_marker(&lines[i].text());
                let mut prev_y = lines[i].y;
                i += 1;
                while i < lines.len() {
                    let l = lines[i];
                    let cont = list_kind(&l.text()).is_none()
                        && !l.mono
                        && header_at(lines, i, body, profile, plan).is_none()
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
            if foot[i] || l.mono || list_kind(&l.text()).is_some() || header_at(lines, i, body, profile, plan).is_some() {
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

pub(crate) fn append_piece(para: &mut String, piece: &str) {
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

/// Output structure for `to_html`.
///
/// - `Page` — pages are first-order: each page's content is wrapped in
///   `<section data-page="N" id="page-N">` and the TOC carries a page per heading.
/// - `Section` — logical sections are first-order: every heading opens its own nested
///   `<section id="sec-…">` wrapper (id on the wrapper, inner heading/paragraphs bare),
///   page identity is dropped, and the TOC has no page numbers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Page,
    Section,
}

const FOOTNOTE_MARKERS: &str = "*†‡§¶";

/// Split an affiliation line `"1Institut …"` / `"* University …"` into its marker key
/// and the organisation text. Returns None if there is no leading digit/footnote key.
fn parse_affiliation(t: &str) -> Option<(String, String)> {
    let t = t.trim();
    let first = t.chars().next()?;
    if first.is_ascii_digit() {
        let key: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        let org = t[key.len()..].trim_start_matches([' ', '.', ')', ':']).trim().to_string();
        if org.is_empty() {
            return None;
        }
        return Some((key, org));
    }
    if FOOTNOTE_MARKERS.contains(first) {
        let org = t[first.len_utf8()..].trim_start_matches([' ', '.', ')', ':']).trim().to_string();
        if org.is_empty() {
            return None;
        }
        return Some((first.to_string(), org));
    }
    None
}

/// A line that is just an affiliation marker on its own (the index digit of a numbered
/// affiliation that the PDF set as a superscript and split onto its own line).
fn is_lone_marker(t: &str) -> bool {
    let t = t.trim();
    let n = t.chars().count();
    ((1..=3).contains(&n) && t.chars().all(|c| c.is_ascii_digit()))
        || (n == 1 && FOOTNOTE_MARKERS.contains(t.chars().next().unwrap()))
}

/// Strip a leading label (`Abstract`, `Keywords`, …) and any trailing separator.
fn strip_label(line: &str, labels: &[&str]) -> String {
    let t = line.trim_start();
    let lo = t.to_lowercase();
    for lab in labels {
        if lo.starts_with(lab) {
            return t[lab.len()..]
                .trim_start_matches([' ', ':', '.', '—', '-', '\u{2014}', '\u{2013}'])
                .to_string();
        }
    }
    t.to_string()
}

/// Finalise one author from the run-scan accumulator into `(name, markers)`.
fn flush_author(name: &mut String, markers: &mut Vec<String>, out: &mut Vec<(String, Vec<String>)>) {
    let n = name.trim();
    if n.chars().any(|c| c.is_alphabetic()) {
        let mut nm = n.to_string();
        // If no superscript markers were captured, recover trailing digit/footnote markers
        // glued to the name ("L. Gernigon 1").
        if markers.is_empty() {
            let tail: String = nm
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit() || FOOTNOTE_MARKERS.contains(*c) || c.is_whitespace())
                .collect();
            let mks: Vec<String> = tail
                .chars()
                .rev()
                .filter(|c| c.is_ascii_digit() || FOOTNOTE_MARKERS.contains(*c))
                .map(|c| c.to_string())
                .collect();
            if !mks.is_empty() {
                let cut = nm.len() - tail.len();
                nm = nm[..cut].trim_end().to_string();
                markers.extend(mks);
            }
        }
        out.push((nm.trim().to_string(), markers.clone()));
    }
    name.clear();
    markers.clear();
}

/// Detect academic front-matter — title, authors (linked to their organisation),
/// abstract, keywords — in the first page's lines. Returns the structured data plus the
/// set of line indices it consumed (the page renderer drops them from the normal flow).
/// The region is the single-column block above the body: title (largest top font) →
/// authors (names + affiliation markers) → affiliations (numbered or `*`-footnote) →
/// `Abstract:` → `Keywords:`.
fn detect_front_matter(lines: &[Line], body: f32) -> (FrontMatter, HashSet<usize>) {
    let mut fm = FrontMatter::default();
    let mut consumed: HashSet<usize> = HashSet::new();
    if lines.is_empty() {
        return (fm, consumed);
    }
    let ymax = lines.iter().map(|l| l.y).fold(f32::MIN, f32::max);
    let ymin = lines.iter().map(|l| l.y).fold(f32::MAX, f32::min);
    let yrange = (ymax - ymin).max(1.0);
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| lines[b].y.partial_cmp(&lines[a].y).unwrap_or(std::cmp::Ordering::Equal));

    let abstract_label = |t: &str| t.trim_start().to_lowercase().starts_with("abstract");
    let keywords_label = |t: &str| {
        let lo = t.trim_start().to_lowercase();
        lo.starts_with("keywords") || lo.starts_with("key words") || lo.starts_with("index terms")
    };
    let is_email = |t: &str| {
        let lo = t.trim_start().to_lowercase();
        t.contains('@') || lo.starts_with("(e-mail") || lo.starts_with("e-mail")
    };

    // --- Title: a distinctly-larger title font wins first (handles comma-heavy titles the
    // reference-guard would reject, e.g. "Extension, crustal structure …, Norwegian
    // margin"); else the prominence heuristic for body-size titles. -------------------
    let (title, title_set) = find_title_sized(lines, body)
        .or_else(|| find_document_title(lines, body))
        .unwrap_or_default();
    let title_min_y = if title_set.is_empty() {
        f32::MAX
    } else {
        fm.title = title;
        consumed.extend(&title_set);
        title_set.iter().map(|&i| lines[i].y).fold(f32::MAX, f32::min)
    };

    // Everything strictly below the title, in reading order.
    let rest: Vec<usize> = order
        .iter()
        .cloned()
        .filter(|&i| !consumed.contains(&i) && lines[i].y < title_min_y - 0.5)
        .collect();

    let author_line = |i: usize| {
        let l = &lines[i];
        let tt = l.text();
        let tt = tt.trim();
        if tt.is_empty() || abstract_label(tt) || keywords_label(tt) || is_email(tt) {
            return false;
        }
        let has_sup = l.runs.iter().any(|r| {
            r.script == 1 && !r.text.trim().is_empty() && r.text.trim().chars().all(|c| c.is_ascii_digit() || FOOTNOTE_MARKERS.contains(c))
        });
        let inits = initials_count(tt);
        let amp = tt.contains('&');
        (has_sup && (inits >= 1 || amp || tt.contains(','))) || inits >= 2 || (amp && inits >= 1)
    };

    // --- Authors (contiguous author lines at the top of the rest). ---------------------
    let mut k = 0usize;
    let mut author_idxs: Vec<usize> = Vec::new();
    while k < rest.len() {
        let i = rest[k];
        let tt = lines[i].text();
        let tt = tt.trim();
        if tt.is_empty() {
            k += 1;
            continue;
        }
        if abstract_label(tt) || keywords_label(tt) {
            break;
        }
        if author_line(i) {
            author_idxs.push(i);
            k += 1;
        } else {
            break;
        }
    }

    // --- Affiliations: numbered/footnote-keyed lines until the abstract; e-mail lines
    // skipped; a lone-digit line keys the org line that follows it. ---------------------
    let mut affs: Vec<(String, String)> = Vec::new();
    let mut pending_key: Option<String> = None;
    while k < rest.len() {
        let i = rest[k];
        let raw = lines[i].text();
        let tt = raw.trim().to_string();
        if tt.is_empty() {
            k += 1;
            continue;
        }
        if abstract_label(&tt) || keywords_label(&tt) {
            break;
        }
        if is_email(&tt) {
            consumed.insert(i);
            k += 1;
            continue;
        }
        if is_lone_marker(&tt) {
            pending_key = Some(tt.trim().to_string());
            consumed.insert(i);
            k += 1;
            continue;
        }
        if let Some((key, org)) = parse_affiliation(&tt) {
            affs.push((key, org));
            consumed.insert(i);
            pending_key = None;
            k += 1;
            continue;
        }
        if let Some(key) = pending_key.take() {
            affs.push((key, tt.clone()));
            consumed.insert(i);
            k += 1;
            continue;
        }
        break;
    }
    // Footnote fallback: affiliations as `*`/`†` lines in the bottom region of page 1.
    if affs.is_empty() {
        for &i in &order {
            if consumed.contains(&i) || lines[i].y > ymin + 0.35 * yrange {
                continue;
            }
            let tt = lines[i].text();
            let tt = tt.trim();
            if is_email(tt) {
                continue;
            }
            if tt.chars().next().is_some_and(|c| FOOTNOTE_MARKERS.contains(c)) {
                if let Some((key, org)) = parse_affiliation(tt) {
                    affs.push((key, org));
                    consumed.insert(i);
                }
            }
        }
    }

    // --- Authors: run-scan into (name, markers); link markers → affiliation. -----------
    if !author_idxs.is_empty() {
        let mut alines = author_idxs.clone();
        alines.sort_by(|&a, &b| lines[b].y.partial_cmp(&lines[a].y).unwrap_or(std::cmp::Ordering::Equal));
        let mut parsed: Vec<(String, Vec<String>)> = Vec::new();
        let mut name_buf = String::new();
        let mut markers: Vec<String> = Vec::new();
        for &li in &alines {
            for r in &lines[li].runs {
                if r.script == 1 {
                    for ch in r.text.trim().chars() {
                        if ch.is_ascii_digit() || FOOTNOTE_MARKERS.contains(ch) {
                            markers.push(ch.to_string());
                        }
                    }
                    continue;
                }
                for ch in r.text.chars() {
                    if ch == ',' || ch == '&' {
                        flush_author(&mut name_buf, &mut markers, &mut parsed);
                    } else {
                        name_buf.push(ch);
                    }
                }
            }
        }
        flush_author(&mut name_buf, &mut markers, &mut parsed);
        consumed.extend(&author_idxs);
        for (name, marks) in parsed {
            if name.trim().is_empty() {
                continue;
            }
            let resolved: Vec<String> = marks
                .iter()
                .filter_map(|m| affs.iter().find(|(k, _)| k == m).map(|(_, o)| o.clone()))
                .collect();
            let affiliation = if resolved.is_empty() { None } else { Some(resolved.join("; ")) };
            fm.authors.push(Author { name: name.trim().to_string(), markers: marks, affiliation });
        }
        fm.affiliations = affs.iter().map(|(_, o)| o.clone()).collect();
    }

    // --- Abstract: from its label down, same size-band, until keywords / a big gap. -----
    if let Some(ap) = rest.iter().position(|&i| !consumed.contains(&i) && abstract_label(lines[i].text().trim())) {
        let ai = rest[ap];
        let asize = lines[ai].size;
        let mut parts: Vec<String> = Vec::new();
        let first = strip_label(&lines[ai].text(), &["abstract"]);
        if !first.trim().is_empty() {
            parts.push(first.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        consumed.insert(ai);
        let mut prev_y = lines[ai].y;
        let mut q = ap + 1;
        while q < rest.len() {
            let i = rest[q];
            if consumed.contains(&i) {
                q += 1;
                continue;
            }
            let tt = lines[i].text();
            let tt = tt.trim().to_string();
            if tt.is_empty() {
                q += 1;
                continue;
            }
            if keywords_label(&tt)
                || lines[i].size > asize + 0.6
                || prev_y - lines[i].y > asize * 3.0
                || numbered_level(&tt).is_some()
                || roman_section(&tt).is_some()
            {
                break;
            }
            parts.push(tt);
            consumed.insert(i);
            prev_y = lines[i].y;
            q += 1;
        }
        let joined = parts.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        if !joined.is_empty() {
            fm.abstract_text = Some(joined);
        }
    }

    // --- Keywords. ---------------------------------------------------------------------
    if let Some(kp) = rest.iter().position(|&i| !consumed.contains(&i) && keywords_label(lines[i].text().trim())) {
        let ki = rest[kp];
        let stripped = strip_label(&lines[ki].text(), &["keywords", "key words", "index terms"]);
        let kws: Vec<String> = stripped
            .split([',', ';'])
            .map(|s| s.trim().trim_end_matches('.').trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !kws.is_empty() {
            fm.keywords = kws;
            consumed.insert(ki);
        }
    }

    (fm, consumed)
}

/// Render the detected front-matter. The `<header>` (class-free, by design) holds the
/// title `<h1>`, authors `<p>` with `<sup>` affiliation markers, and affiliations `<ol>`.
/// The **abstract** and **keywords** are emitted AFTER the header as first-class `<h2>`
/// sections (scientific-paper convention) so they are picked up by the section/TOC
/// machinery and `section("abstract")` resolves to them like any other section.
fn emit_header_block(fm: &FrontMatter, out: &mut String) {
    out.push_str("<header>");
    if !fm.title.is_empty() {
        out.push_str(&format!("<h1>{}</h1>", esc(&fm.title)));
    }
    if !fm.authors.is_empty() {
        out.push_str("<p>");
        for (n, a) in fm.authors.iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            out.push_str(&esc(&a.name));
            if !a.markers.is_empty() {
                out.push_str(&format!("<sup>{}</sup>", esc(&a.markers.join(","))));
            }
        }
        out.push_str("</p>");
    }
    if !fm.affiliations.is_empty() {
        out.push_str("<ol>");
        for org in &fm.affiliations {
            out.push_str(&format!("<li>{}</li>", esc(org)));
        }
        out.push_str("</ol>");
    }
    out.push_str("</header>");
    if let Some(ab) = &fm.abstract_text {
        out.push_str(&format!("<h2>Abstract</h2><p>{}</p>", esc(ab)));
    }
    if !fm.keywords.is_empty() {
        out.push_str(&format!("<h2>Keywords</h2><p>{}</p>", esc(&fm.keywords.join(", "))));
    }
}

/// Document title via the proven PROMINENCE heuristic: the topmost bold / all-caps /
/// larger-than-body phrase before the authors/abstract. Returns the assembled title and
/// the line indices it spans (a multi-line title is one `<h1>`). Titles are often
/// bold-but-not-larger, so prominence — not size alone — is the signal. None if no line
/// qualifies (callers fall back to [`find_title_sized`]).
fn find_document_title(lines: &[Line], body: f32) -> Option<(String, HashSet<usize>)> {
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
    // Core: could be a title (a multi-word phrase that isn't a numbered section, an
    // author/reference line, or "Abstract").
    let title_core = |l: &Line| {
        let t = l.text();
        let tt = t.trim();
        tt.split_whitespace().count() >= 2
            && !tt.contains('@') // an e-mail/contact line is never the title
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
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| lines[b].y.partial_cmp(&lines[a].y).unwrap_or(std::cmp::Ordering::Equal));
    let cap_start = |l: &Line| l.text().trim().chars().next().is_some_and(|c| !c.is_lowercase());
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
    let mut heads: Vec<usize> = order.iter().cloned().filter(|&i| title_ok(&lines[i]) && cap_start(&lines[i])).collect();
    // Fallback: a plain body-size title still sits at the very top of page 1, above the
    // authors/abstract — the topmost cap-start phrase line that isn't a section/reference.
    heads.extend(order.iter().cloned().filter(|&i| {
        title_core(&lines[i]) && cap_start(&lines[i]) && lines[i].text().split_whitespace().count() >= 4
    }));
    for head in heads {
        let (tsz, hy) = (lines[head].size, lines[head].y);
        let set: HashSet<usize> = order
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
            return Some((title, set));
        }
    }
    None
}

/// Size-based title fallback: anchor on the largest multi-word top-region line, then take
/// every line at that size (so a short continuation like "margin" joins the title, and a
/// comma-heavy title the reference-guard rejects is still found). None on no large title.
fn find_title_sized(lines: &[Line], body: f32) -> Option<(String, HashSet<usize>)> {
    if lines.is_empty() {
        return None;
    }
    let ymax = lines.iter().map(|l| l.y).fold(f32::MIN, f32::max);
    let ymin = lines.iter().map(|l| l.y).fold(f32::MAX, f32::min);
    let yrange = (ymax - ymin).max(1.0);
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| lines[b].y.partial_cmp(&lines[a].y).unwrap_or(std::cmp::Ordering::Equal));
    let norm = |i: usize| lines[i].text().split_whitespace().collect::<Vec<_>>().join(" ");
    let is_pub_ref = |t: &str| {
        let lo = t.to_lowercase();
        t.contains('@')
            || lo.contains("journal of")
            || lo.contains("proceedings of")
            || lo.contains("vol.")
            || lo.contains(" pp.")
            || lo.starts_with("pp.")
            || lo.contains("doi:")
            || lo.contains("copyright")
            || t.contains('©')
            || lo.starts_with("printed in")
    };
    let in_top = |i: usize| lines[i].y >= ymax - 0.45 * yrange;
    // An author/affiliation list (mostly capitalised tokens joined by `,`/`and`/`&`) can
    // be the largest text on page 1 — reject it so the size anchor doesn't pick it as the
    // title. A real title (sentence case with lowercase function words) stays well under
    // the cap-ratio threshold.
    let looks_like_names = |i: usize| {
        let t = norm(i);
        if !(t.contains(',') || t.contains(" and ") || t.contains('&')) {
            return false;
        }
        let words: Vec<&str> = t.split_whitespace().filter(|w| w.chars().next().is_some_and(|c| c.is_alphabetic())).collect();
        if words.len() < 2 {
            return false;
        }
        let caps = words.iter().filter(|w| w.chars().next().unwrap().is_uppercase()).count();
        caps * 10 >= words.len() * 7
    };
    // No numbered/roman-section guard here: the font-size gate already discriminates the
    // title, and that guard would reject common titles read as an appendix label — e.g.
    // "A Study of …" / "I Introduction" (a leading "A "/"I " trips numbered_level).
    let ok = |i: usize| in_top(i) && !is_pub_ref(&norm(i)) && !looks_like_names(i);
    let anchor_max = order
        .iter()
        .cloned()
        .filter(|&i| ok(i) && norm(i).split_whitespace().count() >= 2)
        .map(|i| lines[i].size)
        .fold(0.0_f32, f32::max);
    if anchor_max <= body * 1.05 {
        return None;
    }
    let title_set: HashSet<usize> = order
        .iter()
        .cloned()
        .filter(|&i| ok(i) && (lines[i].size - anchor_max).abs() <= 0.5)
        .collect();
    let title = order
        .iter()
        .filter(|i| title_set.contains(i))
        .map(|&i| lines[i].text())
        .collect::<Vec<_>>()
        .join(" ");
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    // Reject a prose/banner "title".
    let w: Vec<&str> = title.split_whitespace().collect();
    let prose = w.len() > 12
        && (title.trim_end().ends_with('.')
            || w.iter().filter(|x| x.chars().next().is_some_and(|c| c.is_lowercase())).count() * 5 >= w.len() * 3);
    if title.split_whitespace().count() >= 2 && !prose {
        Some((title, title_set))
    } else {
        None
    }
}

/// Non-paper first page: emit the detected title as a bare `<h1>` and consume its lines.
/// Uses only the prominence heuristic (the size fallback is reserved for the structured
/// front-matter path) so non-paper output matches the long-standing baseline exactly.
fn emit_document_title(lines: &mut Vec<Line>, body: f32, out: &mut String) {
    if let Some((title, set)) = find_document_title(lines, body) {
        out.push_str(&format!("<h1>{}</h1>", esc(&title)));
        let mut i = 0usize;
        lines.retain(|_| {
            let keep = !set.contains(&i);
            i += 1;
            keep
        });
    }
}

/// Extract the front-matter (title/authors/abstract/keywords) of `doc` from page 1.
/// Standalone path for `pdf.metadata()` — does not run the full HTML pipeline.
pub fn extract_front_matter(doc: &Document, raw: &[u8]) -> FrontMatter {
    let first = match doc.get_pages().into_iter().min_by_key(|(n, _)| *n) {
        Some((_, id)) => id,
        None => return FrontMatter::default(),
    };
    let spans = text::extract_spans(doc, first, raw);
    let mut hist: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
    for s in &spans {
        if s.angle.abs() < 0.01 {
            *hist.entry(s.size.round() as i32).or_insert(0) += 1;
        }
    }
    let body = hist.iter().max_by_key(|(_, c)| **c).map(|(s, _)| *s as f32).unwrap_or(10.0);
    let lines = lines_of(spans, &[]);
    detect_front_matter(&lines, body).0
}

/// True when the detected front-matter is from a real paper (authors present) — only
/// then do we restructure page 1 into a `<header>` and pull the abstract/keywords out
/// of the body. Documents without authors keep their original heading structure.
fn is_paper_front_matter(fm: &FrontMatter) -> bool {
    !fm.authors.is_empty()
}

/// The style signature of a line: rounded size, all-bold, font id — the key the heading
/// tier and the distrust pass share.
fn style_sig(line: &Line) -> (i32, bool, u32) {
    let nonspace: Vec<&Run> = line.runs.iter().filter(|r| !r.text.trim().is_empty()).collect();
    let all_bold = !nonspace.is_empty() && nonspace.iter().all(|r| r.bold);
    (line.size.round() as i32, all_bold, line.font)
}

/// A document-global heading model built once before the parallel render. `distrusted`
/// holds style signatures that produce so MANY heading candidates they are an emphasis /
/// label style (e.g. an SEC filing's bold line-item face matched by hundreds of financial
/// rows), not a heading style — `header_at` drops their `Style` verdicts. Pattern-anchored
/// headings (`Reliable`/`RunIn`: numbered, canonical, SEC `Item`/`PART`, run-ins) bypass
/// the distrust, so real structure survives. Read-only and integer-keyed → deterministic
/// under the parallel render.
#[derive(Default, Clone)]
struct HeadingPlan {
    distrusted: std::collections::HashSet<(i32, bool, u32)>,
}

/// PASS over the whole document's lines: count, per style signature, how many lines yield
/// a surviving `Style` heading verdict, then distrust a signature whose count is far above
/// what real section headings plausibly reach. A genuine document spreads headings across
/// a few tiers with at most a few dozen of any one; an emphasis/label style fires on
/// hundreds. The threshold is an ABSOLUTE floor (not a fraction of doc length — a long
/// filing's flood is still only a few percent of its lines) raised mildly for very long
/// docs. arXiv papers (≤~50 headings, mostly numbered=Reliable) never trip it → no-op.
fn plan_headings(page_spans: &[(u32, ObjectId, Vec<Span>)], body: f32, profile: &DocProfile) -> HeadingPlan {
    let mut counts: HashMap<(i32, bool, u32), usize> = HashMap::new();
    let mut samples: HashMap<(i32, bool, u32), Vec<String>> = HashMap::new();
    let debug = std::env::var("DPDF_HEAD_PROFILE").is_ok();
    let mut doc_lines = 0usize;
    let mut pages = 0usize;
    for (_pno, _id, spans) in page_spans {
        pages += 1;
        let lines = lines_of(spans.iter().map(clone_span).collect(), &[]);
        for ln in &lines {
            doc_lines += 1;
            if let Some((_, _, HeadingKind::Style)) = detect_header(ln, body, Some(profile)) {
                let sig = style_sig(ln);
                *counts.entry(sig).or_insert(0) += 1;
                if debug {
                    let s = samples.entry(sig).or_default();
                    if s.len() < 4 {
                        s.push(ln.text().trim().chars().take(40).collect());
                    }
                }
            }
        }
    }
    let thresh = 90usize.max(pages); // absolute floor 90; ~1 per page for very long docs
    let distrusted: std::collections::HashSet<(i32, bool, u32)> =
        counts.iter().filter(|(_, &c)| c > thresh).map(|(&k, _)| k).collect();
    if debug {
        let mut v: Vec<_> = counts.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("[head-profile] doc_lines={doc_lines} pages={pages} thresh={thresh} style-sigs:");
        for (sig, c) in v.iter().take(10) {
            let dis = if **c > thresh { " DISTRUST" } else { "" };
            eprintln!("  {:?} x{}{}  e.g. {:?}", sig, c, dis, samples.get(*sig).map(|s| s.as_slice()).unwrap_or(&[]));
        }
    }
    HeadingPlan { distrusted }
}

/// Analyse the whole document's typography once and build a [`DocProfile`]: the body
/// size/weight/font, the heading size/weight/font tiers ranked to levels, whether sections
/// are numbered, whether an outline exists, and the column layout. Operates on raw spans
/// (cheap, single pass) — no `lines_of`.
fn build_doc_profile(page_spans: &[(u32, ObjectId, Vec<Span>)], body: f32, title_sz: f32, has_outline: bool) -> DocProfile {
    struct Acc {
        chars: usize,
        spans: usize,
        caption: usize,
        pages: HashSet<u32>,
    }
    let body_i = body.round() as i32;
    let pages = page_spans.len().max(1);
    let mut clusters: HashMap<(i32, bool, u32), Acc> = HashMap::new();
    let mut numbered_hits = 0usize;
    for (pno, _id, spans) in page_spans {
        for s in spans {
            if s.angle.abs() >= 0.01 {
                continue; // ignore rotated labels (matches the body histogram)
            }
            let e = clusters.entry((s.size.round() as i32, s.bold, s.font)).or_insert_with(|| Acc {
                chars: 0,
                spans: 0,
                caption: 0,
                pages: HashSet::new(),
            });
            e.chars += s.text.chars().count();
            e.spans += 1;
            e.pages.insert(*pno);
            let lo = s.text.trim_start().to_lowercase();
            if lo.starts_with("figure") || lo.starts_with("table") || lo.starts_with("fig.") || lo.starts_with("tab.") {
                e.caption += 1;
            }
            // A multi-component number token ("3.2") is a strong numbered-section signal.
            let t = s.text.trim();
            if t.len() >= 3 && t.contains('.') && t.bytes().next().is_some_and(|b| b.is_ascii_digit()) && t.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                numbered_hits += 1;
            }
        }
    }
    // Body cluster = most characters.
    let (body_key, body_chars) = clusters
        .iter()
        .max_by_key(|(_, a)| a.chars)
        .map(|(k, a)| (*k, a.chars.max(1)))
        .unwrap_or(((body_i, false, 0), 1));
    let body_font = body_key.2;
    // The title size, from the CLUSTER histogram — the largest size that carries real
    // text mass. (Not `title_sz` = max single span: a stray large glyph/symbol inflates
    // that and would stop the real title from being excluded from the heading tiers,
    // letting it be merged/mis-promoted.)
    let title_size = clusters
        .iter()
        .filter(|(&(sz, _, _), a)| sz > body_i && a.chars >= 15)
        .map(|(&(sz, _, _), _)| sz)
        .max();
    // Exclude the title (and anything larger) from heading tiers only when it is
    // distinctly larger than body.
    let title_excl = title_size.filter(|&ts| (ts as f32) > body * 1.15);
    // The display/heading font = the font of the title cluster. At an equal heading size
    // this font outranks others (a section in the title face is a level above a subsection
    // in a secondary face).
    let title_font = title_size
        .and_then(|ts| clusters.iter().filter(|(&(sz, _, _), _)| sz == ts).max_by_key(|(_, a)| a.chars).map(|(&(_, _, f), _)| f))
        .unwrap_or(body_font);
    // Heading-candidate clusters: strictly larger than body AND looking like a heading
    // FACE — a distinct font, or bold, or clearly larger (≥1.18×) — so a stray large body
    // word or body-size italic/caption text never becomes a heading tier.
    let mut cands: Vec<(i32, bool, u32, usize)> = clusters
        .iter()
        .filter_map(|(&(sz, bold, font), a)| {
            if (sz, bold, font) == body_key || sz <= body_i {
                return None;
            }
            if font == body_font && !bold && (sz as f32) < body * 1.18 {
                return None; // a slightly-large body word, not a heading face
            }
            if title_excl.is_some_and(|ts| sz >= ts) {
                return None; // the title (or larger) — handled separately, consumed before emit
            }
            // Sparse + not caption-like (the strong guards against a secondary body size,
            // captions, or running heads becoming a heading tier).
            if a.chars as f32 >= body_chars as f32 * 0.4 {
                return None;
            }
            if pages >= 4 && a.pages.len() as f32 > pages as f32 * 0.9 {
                return None; // present on ~every page of a multi-page doc → running head
            }
            if a.caption * 2 >= a.spans {
                return None;
            }
            Some((sz, bold, font, a.chars))
        })
        .collect();
    // Rank: size desc, then the title/display font first, then bold first, then density —
    // so size 10 in the title face (section) outranks size 10 in a secondary face
    // (subsection). Levels 1=h2, 2=h3, …
    cands.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then((b.2 == title_font).cmp(&(a.2 == title_font)))
            .then(b.1.cmp(&a.1))
            .then(b.3.cmp(&a.3))
    });
    let tiers: Vec<HeadingTier> = cands
        .iter()
        .take(4)
        .enumerate()
        .map(|(i, &(sz, bold, font, _))| HeadingTier { size: sz as f32, bold, font, level: (i as u8 + 1).min(4) })
        .collect();
    // Column layout: 2 if at least half the pages carry a central prose gutter.
    let two_col = page_spans.iter().filter(|(_, _, s)| extract::central_gutter(s).is_some()).count();
    let columns = if two_col * 2 >= pages { 2 } else { 1 };

    DocProfile {
        body_size: body_key.0 as f32,
        body_font,
        body_bold: body_key.1,
        title_sz,
        tiers,
        numbered: numbered_hits >= 2,
        has_outline,
        columns,
        running_heads: HashSet::new(),
    }
}

/// `mode`: see [`Mode`] — `Page` keeps per-page `<section data-page>` wrappers; `Section`
/// regroups content into nested `<section id="sec-…">` blocks and drops page info.
///
/// `inline_images`: when true, raster images are emitted as inline `<img src=…>`
/// (base64 data URIs). When false, each is replaced by a lightweight `<image N>`
/// placeholder (N a 1-based document-wide image counter) — the figure/caption
/// wrapper and any `#fig-N` anchor are preserved, only the pixel payload is dropped.
///
/// `include_toc`: when true, an auto-generated `<nav>` table of contents is prepended
/// to `<body>`. When false it is omitted — heading/section `id=` anchors are still
/// assigned (so `#sec-…` links and `section()` keep working), only the visible TOC drops.
pub fn to_html(doc: &Document, raw: &[u8], mode: Mode, inline_images: bool, include_toc: bool) -> String {
    // Optional coarse phase profiler: set DPDF_PROFILE=1 to print per-phase WALL time to
    // stderr. `prof_phase(label, ||…)` times a closure; zero cost when unset.
    let prof = std::env::var_os("DPDF_PROFILE").is_some();
    let prof_start = if prof { Some(std::time::Instant::now()) } else { None };
    let phase = |label: &str, t: std::time::Instant| {
        if prof {
            eprintln!("  {label:<16} {:8.1}ms", t.elapsed().as_secs_f64() * 1e3);
        }
    };

    let pages = doc.get_pages();

    // Document-wide body font size = most common rounded span size. Spans are extracted
    // per page in PARALLEL (each page is independent and read-only on the document); the
    // histogram is folded sequentially afterwards (cheap).
    let t = std::time::Instant::now();
    let mut page_spans: Vec<(u32, ObjectId, Vec<Span>)> = pages
        .par_iter()
        .map(|(&pno, &pid)| (pno, pid, text::extract_spans(doc, pid, raw)))
        .collect();
    page_spans.sort_by_key(|(pno, _, _)| *pno);
    phase("01_spans", t);
    let mut hist: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
    for (_, _, spans) in &page_spans {
        for s in spans {
            if s.angle.abs() < 0.01 {
                *hist.entry(s.size.round() as i32).or_insert(0) += 1; // body size ignores rotated labels
            }
        }
    }
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

    // The PDF's own outline (bookmarks): used both to drive the nav and — per target
    // page — to promote matching lines to headings (so body-size section titles the
    // visual cues miss are still recognised, and the outline TOC links resolve).
    let outline = links::outline(doc);
    let mut promote_by_page: std::collections::HashMap<u32, Vec<String>> = std::collections::HashMap::new();
    for e in &outline {
        if e.page > 0 {
            promote_by_page.entry(e.page).or_default().push(title_key(&e.title));
        }
    }

    // Document-wide style profile: the body/heading size·weight·font tiers, numbering,
    // outline presence and column layout — drives heading classification per-document.
    let profile = build_doc_profile(&page_spans, body, title_sz, !outline.is_empty());
    // Global heading pre-detection: distrust over-used emphasis/label styles so a filing's
    // line-item flood doesn't read as hundreds of headings (see plan_headings).
    let head_plan = plan_headings(&page_spans, body, &profile);

    // Render every page IN PARALLEL into its own (html_fragment, image_uris). Each page
    // is independent and reads the document immutably; image data URIs are deferred as
    // page-LOCAL `\0<idx>\0` sentinels (so the string passes never touch the base64) and
    // remapped to global indices during the sequential merge below.
    let t = std::time::Instant::now();
    let renders: Vec<(String, Vec<String>)> = page_spans
        .par_iter()
        .enumerate()
        .map(|(pidx, (pno, _pid, spans))| {
        let pno = pno;
        let mut out = String::new();
        // Per-page deferred inline-image data URIs (placeholder mode stores empty strings
        // to keep the index aligned for `<image N>` numbering). The leading `\0<idx>\0`
        // sentinel is rewritten to a global index at merge time.
        let mut img_uris: Vec<String> = Vec::new();
        // Page mode wraps each page in its own <section>; section mode emits the page's
        // content bare into the stream and regroups it by heading afterwards.
        if mode == Mode::Page {
            out.push_str(&format!("<section data-page=\"{pno}\" id=\"page-{pno}\">\n"));
        }
        // Anchor targets for this page's named destinations, so the semantic links
        // (#cite.x / #figure.n / #equation.n / #section.x) resolve. Empty <a id> at
        // the section head land the reader on the correct page + the exact target id.
        if let Some(ds) = dests_by_page.get(pno) {
            for (sl, _y) in ds {
                out.push_str(&format!("<a id=\"{sl}\"></a>"));
            }
            out.push('\n');
        }
        let mut tables = extract::detect_tables_pos(spans);
        let mut images = img::positioned_images(doc, *_pid, inline_images);
        let (raw_vectors, weak_vectors) = vector::positioned_vectors(doc, *_pid);
        // Drop FALSE tables — a "table" that is really a figure's own structure, not a data
        // table — BEFORE filtering vectors, so the real plot vector survives the vector
        // filter below while a genuine ruled form table is preserved:
        //   (a) a region largely covered by a raster image (a plot's data scatter/heatmap
        //       reads as a grid), or
        //   (b) a thin strip mostly inside a MUCH larger COMPOSITE-PLOT vector — a vector
        //       that itself contains a substantial raster (the plot's data scatter/heatmap),
        //       so the strip is the plot's axis-number row / legend. Requiring the vector to
        //       contain a raster is what protects a ruled form (e.g. the IRS W-9): its cell
        //       borders are a large vector with NO raster, so its real table is never dropped.
        // Left in, such a false table both consumes the figure's labels as cells AND
        // suppresses the overlapping vector, fragmenting a raster+vector plot (a Vp-depth
        // crossplot) into a lone raster plus loose axis text.
        tables.retain(|t| {
            let ta = ((t.x_right - t.x_left) * (t.y_top - t.y_bottom)).max(1.0);
            let raster_covered = images.iter().any(|im| {
                let ia = ((im.x_right - im.x_left) * (im.y_top - im.y_bottom)).max(1.0);
                let ox = (t.x_right.min(im.x_right) - t.x_left.max(im.x_left)).max(0.0);
                let oy = (t.y_top.min(im.y_top) - t.y_bottom.max(im.y_bottom)).max(0.0);
                ia >= ta * 0.15 && ox * oy >= ia * 0.5
            });
            let strip_in_plot = raw_vectors.iter().any(|v| {
                let va = ((v.x_right - v.x_left) * (v.y_top - v.y_bottom)).max(1.0);
                let ox = (v.x_right.min(t.x_right) - v.x_left.max(t.x_left)).max(0.0);
                let oy = (v.y_top.min(t.y_top) - v.y_bottom.max(t.y_bottom)).max(0.0);
                if !(ox * oy >= ta * 0.6 && ta < va * 0.5) {
                    return false;
                }
                // …and the vector is a composite plot: it contains a substantial raster.
                images.iter().any(|im| {
                    let ia = ((im.x_right - im.x_left) * (im.y_top - im.y_bottom)).max(1.0);
                    let iox = (v.x_right.min(im.x_right) - v.x_left.max(im.x_left)).max(0.0);
                    let ioy = (v.y_top.min(im.y_top) - v.y_bottom.max(im.y_bottom)).max(0.0);
                    iox * ioy >= ia * 0.5
                })
            });
            !(raster_covered || strip_in_plot)
        });
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

        // Fold a heading that wraps onto a second line (same heading tier, adjacent) into
        // one Line, so a two-line section title — incl. a first line ending ':' — becomes a
        // single <h2> instead of a heading plus an orphaned body paragraph.
        merge_heading_continuations(&mut lines, &profile);

        // Document front-matter (first page only): title, authors+organisations,
        // abstract, keywords. Emitted as a semantic <header>; the lines it consumes are
        // dropped from the normal page flow so the body intro still renders, and the
        // title <h1> stays out of the section/TOC machinery (build_* skip <header>).
        // Front-matter (first page only). A real paper (authors detected) is restructured
        // into a semantic <header> — title/authors+orgs/abstract/keywords — with those
        // lines consumed. Anything else keeps the proven title-only heuristic: the topmost
        // prominent phrase becomes a bare <h1> that flows into the section/TOC machinery.
        if pidx == 0 && !lines.is_empty() {
            let (fm, consumed) = detect_front_matter(&lines, body);
            if is_paper_front_matter(&fm) {
                emit_header_block(&fm, &mut out);
                let mut i = 0usize;
                lines.retain(|_| {
                    let keep = !consumed.contains(&i);
                    i += 1;
                    keep
                });
            } else {
                emit_document_title(&mut lines, body, &mut out);
            }
        }

        // Ordered items by y (top -> bottom). Img/T carry an index so a caption
        // can be attached opportunistically (see below).
        enum Item<'a> {
            L(&'a Line),
            T(usize),
            Img(usize),
            Svg(usize), // vector figure transcoded to inline SVG
            Cap(usize), // standalone caption (e.g. a vector figure with no raster)
        }
        // Vector figures (diagrams/plots drawn as paths). A figure's AREA is the
        // detected vector-ink cluster; we drop any that overlap a detected table
        // (tables own their region) so table rules aren't re-emitted as a figure.
        // A figure overlapping a (remaining, real) table is dropped — a ruled table's
        // borders read as vector ink and the table owns its region. The false tables that
        // would wrongly suppress a real plot vector were already removed above, so this
        // simple any-overlap test no longer fragments raster+vector crossplots.
        let not_in_table = |v: &vector::PlacedSvg| {
            !tables.iter().any(|t| v.x_left < t.x_right && v.x_right > t.x_left && v.y_bottom < t.y_top && v.y_top > t.y_bottom)
        };
        let mut vectors: Vec<vector::PlacedSvg> = raw_vectors.into_iter().filter(&not_in_table).collect();
        // Caption-aware recovery: a small vector diagram below the figure filter's strong bar
        // (a few ellipse curves, a TikZ sketch) is held aside as a WEAK candidate. Promote one
        // into `vectors` only when a FIGURE CAPTION sits right next to it AND no strong figure
        // already serves that caption — so a stray mark with no caption is never resurrected,
        // and a caption with a real graphic isn't given a spurious second figure. Done here,
        // before fig_boxes/labels/emit are built, so a promoted candidate flows through the
        // identical machinery as a strong figure.
        let mut weak: Vec<vector::PlacedSvg> = weak_vectors.into_iter().filter(&not_in_table).collect();
        if !weak.is_empty() {
            let gap = body * 6.0; // a caption sits within a few lines of its figure
            let mut claimed = vec![false; weak.len()];
            let mut promote: Vec<usize> = Vec::new();
            for l in &lines {
                let t = l.text();
                let is_fig = matches!(caption_label(&t), Some((true, _)));
                if !is_fig || is_inline_xref(&t) || caption_is_continued(&t) || dotleader_tail(&t) {
                    continue;
                }
                let cy = l.y;
                let edge = |yb: f32, yt: f32| if cy < yb { yb - cy } else if cy > yt { cy - yt } else { 0.0 };
                let strong_near = images
                    .iter()
                    .map(|im| edge(im.y_bottom, im.y_top))
                    .chain(vectors.iter().map(|v| edge(v.y_bottom, v.y_top)))
                    .fold(f32::INFINITY, f32::min);
                if strong_near <= gap {
                    continue; // caption already has a (strong) graphic
                }
                let mut best: Option<(usize, f32)> = None;
                for (j, v) in weak.iter().enumerate() {
                    if claimed[j] {
                        continue;
                    }
                    let e = edge(v.y_bottom, v.y_top);
                    let x_overlap = v.x_right > l.x0 - body && v.x_left < l.x1 + body;
                    if e <= gap && x_overlap && best.is_none_or(|(_, be)| e < be) {
                        best = Some((j, e));
                    }
                }
                if let Some((j, _)) = best {
                    claimed[j] = true;
                    promote.push(j);
                }
            }
            promote.sort_unstable();
            promote.dedup();
            for &j in promote.iter().rev() {
                vectors.push(weak.remove(j));
            }
        }
        // Mark lines consumed by a table (within its y-range). A line belongs to a table
        // only if it overlaps in BOTH axes — the x-overlap is essential on two-column pages
        // so a table in one column doesn't swallow the other column's prose. Defined after
        // all table filtering so it sees the final table set.
        let in_table = |x0: f32, x1: f32, y: f32| {
            tables.iter().any(|t| {
                y <= t.y_top + body && y >= t.y_bottom - body && x1 > t.x_left && x0 < t.x_right
            })
        };
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
        let near_fig_label = |x: f32, y: f32, size: f32, text: &str| {
            let near = fig_boxes.iter().any(|&(xl, xr, yb, yt)| {
                x >= xl - axis_margin && x <= xr + axis_margin && y >= yb - axis_margin && y <= yt + axis_margin
            });
            if !near {
                return false;
            }
            // Small-font text just outside the ink is a tick/label (the common case); a
            // BODY-size run is a label only when it is axis-shaped (a numeric tick row or a
            // unit-bearing axis title — see `is_axis_label_text`), which excludes the
            // multi-word "Figure N:" caption and ordinary prose.
            size < body * 0.82 || is_axis_label_text(text)
        };
        // A body-size, multi-word line that merely OVERLAPS a figure's ink box (a
        // description paragraph the box happens to span — e.g. a location map with a
        // descriptive blurb beside it) is PROSE, not a figure label: keep it in the body
        // flow and out of the SVG. Real map/axis labels are short or small-font, so they
        // are unaffected.
        let mut prose_rows: Vec<(f32, f32, f32, f32)> = Vec::new();
        for l in &lines {
            if l.size >= body * 0.95
                && l.text().split_whitespace().count() > 5
                && !is_axis_label_text(&l.text()) // a body-size numeric tick row is a figure label, not prose
                && detect_header(l, body, Some(&profile)).is_none()
                && in_figure((l.x0 + l.x1) * 0.5, l.y)
            {
                prose_rows.push((l.x0, l.x1, l.y - 1.0, l.y + l.size + 1.0));
            }
        }
        let in_prose = |x: f32, y: f32| {
            prose_rows.iter().any(|&(x0, x1, y0, y1)| x >= x0 - 2.0 && x <= x1 + 2.0 && y >= y0 && y <= y1)
        };
        // A figure caption ("Figure N …", "Table N …") sits just outside the figure ink,
        // within `near_fig_label`'s margin. Its numeric fragments (the "5.6"/"-" in
        // "Figure 5.6: …") otherwise read as axis ticks and get scooped INTO the SVG — and
        // duplicated, since the caption is also emitted normally. Exclude any span on a
        // caption line from figure-label capture.
        let caption_bands: Vec<(f32, f32, f32, f32)> = lines
            .iter()
            .filter(|l| caption_label(&l.text()).is_some())
            .map(|l| (l.x0, l.x1, l.y - 1.0, l.y + l.size + 1.0))
            .collect();
        let in_caption = |x: f32, y: f32| {
            caption_bands.iter().any(|&(x0, x1, y0, y1)| x >= x0 - 2.0 && x <= x1 + 2.0 && y >= y0 && y <= y1)
        };
        // Render the figures' text as SVG <text>: a figure's labels are drawn either
        // inside its Form XObject (form_text_spans) OR directly in the page content
        // within the figure's bbox (DAG node labels, plot axis ticks) — both are
        // collected here so the figure shows its labels, and the body-content ones
        // are removed from the prose flow below (see the in_figure filter).
        if !vectors.is_empty() {
            let mk = |s: text::Span| vector::LabelSpan { x: s.x, y: s.y, size: s.size, width: s.width, text: s.text, bold: s.bold, italic: s.italic, angle: s.angle };
            let mut labels: Vec<vector::LabelSpan> = text::form_text_spans(doc, *_pid, raw)
                .into_iter()
                .filter(|s| {
                    let (cx, cy) = (s.x + s.width * 0.5, s.y + s.size * 0.5);
                    !in_prose(cx, cy) && !in_caption(cx, cy)
                })
                .map(mk)
                .collect();
            for s in spans {
                let (cx, cy) = (s.x + s.width * 0.5, s.y + s.size * 0.5);
                if (in_figure(cx, cy) || near_fig_label(cx, cy, s.size, &s.text)) && !in_prose(cx, cy) && !in_caption(cx, cy) {
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
                caption_label(&t).and_then(|(f, n)| {
                    // Drop multi-page "Figure N—Continued" markers — re-emitting them would
                    // duplicate the original figure's id and pollute the output with empty
                    // continuation captions.
                    (!is_ref_continuation(idx) && !is_inline_xref(&t) && !caption_is_continued(&t) && !is_dotleader_toc(&lines, idx)).then_some((idx, f, n))
                })
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
            let (html, used) = gather_caption(&lines, idx, body, &profile);
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
                // its graphic was dropped — an honest content-loss marker, and it keeps the
                // caption as a semantic `<figcaption>`). An unanchored TABLE caption is NOT a
                // figure: emit it as a caption `<div>` (the table itself was not detected
                // nearby) — same element table captions use, keeping the `tab-N` id so
                // cross-refs still resolve. Spurious empties (List-of-Figures entries) and
                // recoverable small vectors are already handled upstream (dot-leader
                // suppression + caption-aware weak-vector promotion), so what remains here is
                // a genuine figure whose graphic we could not extract.
                let nid = num_id(num);
                let block = if is_fig {
                    format!("<figure id=\"fig-{nid}\"><figcaption>{html}</figcaption></figure>")
                } else {
                    format!("<div id=\"tab-{nid}\">{html}</div>")
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
            // An axis tick/title is part of the figure even when it's bold/short enough to
            // look like a heading (a plot's "Vp (m/s)" title); a merely-contained line is a
            // figure label only when it isn't a real section heading.
            let axis_label = near_fig_label(fig_cx, l.y, l.size, &l.text());
            let fig_label = (in_figure(fig_cx, l.y) || axis_label)
                && !in_prose(fig_cx, l.y)
                && (axis_label || detect_header(l, body, Some(&profile)).is_none());
            if !in_table(l.x0, l.x1, l.y) && !consumed_caption.contains(&idx) && !fig_label {
                items.push(Item::L(l));
                boxes.push((l.x0, l.x1.max(l.x0 + 0.1), l.y, l.y + l.size.max(1.0)));
            }
        }
        for (j, t) in tables.iter().enumerate() {
            items.push(Item::T(j));
            boxes.push((t.x_left, t.x_right.max(t.x_left + 0.1), t.y_bottom, t.y_top));
        }
        // Pair an overlapping raster + vector into ONE composite figure (only inline, so
        // the raster actually renders). The direction depends on which mostly contains the
        // other — and BOTH are common:
        //  - vector mostly inside raster (a location map: a base photo with vector lines/
        //    labels on top) → the raster is the base, the vector overlays it.
        //  - raster mostly inside vector (a plot whose data points are a raster within the
        //    axes/legend frame, e.g. a Vp-depth crossplot) → the vector is the base, the
        //    raster is embedded in its SVG.
        // Either way the composite is one `<svg>` (raster `<image>` + vector ink + labels),
        // registered pixel-for-pixel in the figure's own coordinate space.
        let mut vec_owner: Vec<Option<usize>> = vec![None; vectors.len()]; // vector → base image
        let mut img_overlays: Vec<Vec<usize>> = vec![Vec::new(); images.len()]; // image → overlay vectors
        let mut img_owner: Vec<Option<usize>> = vec![None; images.len()]; // image → base vector
        let mut svg_rasters: Vec<Vec<usize>> = vec![Vec::new(); vectors.len()]; // vector → embedded images
        if inline_images {
            for (vi, v) in vectors.iter().enumerate() {
                let varea = ((v.x_right - v.x_left) * (v.y_top - v.y_bottom)).max(1.0);
                for (ii, im) in images.iter().enumerate() {
                    if vec_owner[vi].is_some() || img_owner[ii].is_some() {
                        continue;
                    }
                    let ox = (v.x_right.min(im.x_right) - v.x_left.max(im.x_left)).max(0.0);
                    let oy = (v.y_top.min(im.y_top) - v.y_bottom.max(im.y_bottom)).max(0.0);
                    let overlap = ox * oy;
                    let imarea = ((im.x_right - im.x_left) * (im.y_top - im.y_bottom)).max(1.0);
                    if overlap / varea > 0.6 {
                        vec_owner[vi] = Some(ii); // vector mostly inside image → raster base
                        img_overlays[ii].push(vi);
                    } else if overlap / imarea > 0.6 {
                        img_owner[ii] = Some(vi); // image mostly inside vector → vector base
                        svg_rasters[vi].push(ii);
                    }
                }
            }
        }
        for (j, im) in images.iter().enumerate() {
            items.push(Item::Img(j));
            boxes.push((px0, px1, im.y_top - 1.0, im.y_top + 1.0)); // full-width separator
        }
        for (j, v) in vectors.iter().enumerate() {
            if vec_owner[j].is_some() {
                continue; // overlaid onto its image, not emitted separately
            }
            items.push(Item::Svg(j));
            boxes.push((v.x_left, v.x_right.max(v.x_left + 0.1), v.y_bottom, v.y_top));
        }
        for (j, (cx0, cy, _)) in standalone.iter().enumerate() {
            items.push(Item::Cap(j));
            boxes.push((*cx0, *cx0 + 0.1, *cy, *cy + body.max(1.0)));
        }
        let order = text::xy_cut_order(&boxes, body);
        let items: Vec<&Item> = order.iter().map(|&i| &items[i]).collect();

        // Emit, grouping consecutive lines into text blocks. `page_promote` lists the
        // PDF-outline titles whose target page is this one, so body-size section titles
        // still become headings.
        let page_promote: &[String] = promote_by_page.get(pno).map(|v| v.as_slice()).unwrap_or(&[]);
        let mut run: Vec<&Line> = Vec::new();
        let flush = |run: &mut Vec<&Line>, out: &mut String| {
            if !run.is_empty() {
                emit_lines(run, body, title_sz, page_promote, &profile, &head_plan, out);
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
                    // This raster is embedded inside a larger vector figure (a plot whose
                    // data points are a raster within the axes) — emitted by that vector's
                    // Item::Svg composite, not here.
                    if img_owner[*j].is_some() {
                        continue;
                    }
                    // A raster overlaid by EXACTLY ONE vector figure (a location map: a base
                    // photo with vector lines/labels on top), in inline mode, is composited
                    // into ONE `<svg>` with the raster embedded as an `<image>` in the
                    // figure's own coordinate space. Both share that space, so the polygons
                    // register with the raster pixel-for-pixel, and the union viewBox keeps
                    // the marginal axis labels the CSS-overlay path used to clip.
                    if inline_images && img_overlays[*j].len() == 1 {
                        let idx = img_uris.len();
                        img_uris.push(std::mem::take(&mut images[*j].uri));
                        let href = format!("\u{0}{idx}\u{0}");
                        let im = &images[*j];
                        let vi = img_overlays[*j][0];
                        let svg = vectors[vi].composite_svg(&[(&href, (im.x_left, im.x_right, im.y_bottom, im.y_top), im.ctm)]);
                        // Caption may have attached to the image OR its overlay vector.
                        let cap = img_cap[*j].as_ref().or(svg_cap[vi].as_ref());
                        match cap {
                            Some((num, cap)) => out.push_str(&format!("<figure id=\"fig-{}\">{svg}<figcaption>{cap}</figcaption></figure>", num_id(num))),
                            None => out.push_str(&format!("<figure>{svg}</figure>")),
                        }
                        continue;
                    }
                    // Both the inline data URI (often megabytes) and the `<image N>`
                    // number are DEFERRED behind a tiny page-local `\0<idx>\0` sentinel:
                    // the string post-processing passes never re-scan the payload, and the
                    // global index/number is resolved once at the very end (after the
                    // sequential merge fixes up page offsets). See substitute_images.
                    let idx = img_uris.len();
                    let graphic = if inline_images {
                        img_uris.push(std::mem::take(&mut images[*j].uri));
                        format!("<img src=\"\u{0}{idx}\u{0}\" />")
                    } else {
                        img_uris.push(String::new()); // keep the index aligned for numbering
                        format!("<image \u{0}{idx}\u{0}>")
                    };
                    // Vector overlays annotating this image, each positioned over the exact
                    // page region it occupies WITHIN the image (so the polygons line up
                    // with the raster), and clipped to the vector ink so stray prose the
                    // figure picked up below the map doesn't render.
                    let im = &images[*j];
                    let iw = (im.x_right - im.x_left).max(1.0);
                    let ih = (im.y_top - im.y_bottom).max(1.0);
                    let overlays: String = img_overlays[*j]
                        .iter()
                        .map(|&vi| {
                            let v = &vectors[vi];
                            let style = format!(
                                "position:absolute;left:{:.2}%;top:{:.2}%;width:{:.2}%;height:{:.2}%",
                                (v.x_left - im.x_left) / iw * 100.0,
                                (im.y_top - v.y_top) / ih * 100.0,
                                (v.x_right - v.x_left) / iw * 100.0,
                                (v.y_top - v.y_bottom) / ih * 100.0,
                            );
                            v.overlay_svg(&style)
                        })
                        .collect();
                    let rel = if overlays.is_empty() { "" } else { " style=\"position:relative\"" };
                    match &img_cap[*j] {
                        Some((num, cap)) => out.push_str(&format!(
                            "<figure{rel} id=\"fig-{nid}\">{graphic}{overlays}<figcaption>{cap}</figcaption></figure>",
                            nid = num_id(num)
                        )),
                        None => out.push_str(&format!("<figure{rel}>{graphic}{overlays}</figure>")),
                    }
                }
                Item::Svg(j) => {
                    flush(&mut run, &mut out);
                    // A vector frame containing raster image(s) (a plot whose data points
                    // are a raster within the axes): composite into ONE `<svg>` with each
                    // raster embedded as an `<image>` in the figure's coordinate space.
                    let svg = if !svg_rasters[*j].is_empty() {
                        let rasters: Vec<(String, (f32, f32, f32, f32), Option<[f32; 6]>)> = svg_rasters[*j]
                            .iter()
                            .map(|&ii| {
                                let idx = img_uris.len();
                                img_uris.push(std::mem::take(&mut images[ii].uri));
                                let im = &images[ii];
                                (format!("\u{0}{idx}\u{0}"), (im.x_left, im.x_right, im.y_bottom, im.y_top), im.ctm)
                            })
                            .collect();
                        let refs: Vec<(&str, (f32, f32, f32, f32), Option<[f32; 6]>)> = rasters.iter().map(|(h, r, m)| (h.as_str(), *r, *m)).collect();
                        vectors[*j].composite_svg(&refs)
                    } else {
                        vectors[*j].svg()
                    };
                    // Caption may have attached to the vector OR to one of its embedded
                    // rasters — use whichever has it so the composite keeps its caption.
                    let cap = svg_cap[*j].as_ref().or_else(|| svg_rasters[*j].iter().find_map(|&ii| img_cap[ii].as_ref()));
                    match cap {
                        Some((num, cap)) => out.push_str(&format!(
                            "<figure id=\"fig-{nid}\">{svg}<figcaption>{cap}</figcaption></figure>",
                            nid = num_id(num)
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
        if mode == Mode::Page {
            out.push_str("\n</section>\n");
        }
        (out, img_uris)
        })
        .collect();
    phase("02_render", t);

    // Sequential merge (page order): concatenate the per-page fragments, rewriting each
    // page's local `\0<idx>\0` image sentinels to global indices, and concatenating the
    // per-page URI lists into one global list.
    let t = std::time::Instant::now();
    let mut out = String::from(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <style>\nbody{max-width:48rem;margin:auto;padding:1rem}\n\
         img,svg{max-width:100%;height:auto}\n</style>\n</head>\n<body>\n",
    );
    let mut img_uris: Vec<String> = Vec::new();
    for (frag, uris) in renders {
        append_with_img_offset(&mut out, &frag, img_uris.len());
        img_uris.extend(uris);
    }
    out.push_str("</body>\n</html>\n");
    phase("03_merge", t);

    let t = std::time::Instant::now();
    let body = dedup_ids(&merge_adjacent_figures(&merge_math_fragments(&merge_fragmented_lists(&merge_adjacent_links(&demote_running_headings(out))))));
    // Page mode: id + TOC on headings, keyed by page. Section mode: regroup content into
    // nested <section id="sec-…"> wrappers with a pageless TOC.
    let result = match mode {
        Mode::Page => build_toc(body, include_toc),
        Mode::Section => build_sections(body, include_toc),
    };
    // Prefer the PDF's OWN table of contents (the `/Outlines` bookmarks) for the nav when
    // the document supplies one — it is the author's clean TOC, vs. our heading-detected
    // approximation. Only the visible `<nav>` is swapped; heading/section anchors are
    // unchanged (the outline links to them).
    let result = if include_toc && !outline.is_empty() {
        nav_from_outline(result, &outline, mode)
    } else {
        result
    };
    // Splice the deferred image URIs / `<image N>` numbers into their sentinels.
    let result = substitute_images(result, &img_uris, inline_images);
    phase("04_assemble", t);
    if let Some(t0) = prof_start {
        eprintln!("[DPDF_PROFILE] {} pages, total {:.1}ms", page_spans.len(), t0.elapsed().as_secs_f64() * 1e3);
    }
    result
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
        font: s.font,
    }
}

