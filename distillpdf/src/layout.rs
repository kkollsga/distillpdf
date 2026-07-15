//! The shared line/run layout model: group positioned text spans into visual lines
//! (`Line`) of style runs (`Run`), column-aware via the same XY-cut as the text path,
//! and render a line's runs to inline HTML. These types are the cross-module currency
//! between html.rs and the feature modules (captions, headings, frontmatter, …).
//! Extracted from html.rs.

use crate::html::esc;
use crate::text::{self, Span};

/// A link's rectangle (PDF user space) plus its resolved href, for hit-testing
/// text spans during HTML emission. Internal links point at `#page-N`.
pub(crate) struct LinkBox {
    pub(crate) rect: [f32; 4],
    pub(crate) href: String,
}

/// href of the link whose rectangle contains point `(x, y)`, if any.
fn href_at(x: f32, y: f32, links: &[LinkBox]) -> Option<String> {
    links
        .iter()
        .find(|l| x >= l.rect[0] && x <= l.rect[2] && y >= l.rect[1] && y <= l.rect[3])
        .map(|l| l.href.clone())
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
pub(crate) fn lines_of(mut spans: Vec<Span>, links: &[LinkBox]) -> Vec<Line> {
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
