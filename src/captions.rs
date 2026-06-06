//! Figure/table caption detection: tell a real caption ("Figure 3: …") from an inline
//! cross-reference, a "Continued" marker, or a List-of-Figures dot-leader entry; gather a
//! multi-line caption block. Extracted from html.rs.

use crate::headings::detect_header;
use crate::html::{append_piece, list_kind};
use crate::layout::{render_runs, Line};
use crate::profile::DocProfile;

/// "Figure 2 shows …", "Table 1 summarises …", "Fig. 3 plots …" — an in-text
/// cross-reference, NOT a caption. The figure/table number is followed by a verb
/// or pronoun (the sentence's predicate), whereas a real caption is followed by a
/// descriptive title or a ':' / '.' separator. Such a reference must stay body
/// prose, not become a phantom `<figure id=fig-N>` (which also duplicates the real
/// figure's id).
pub(crate) fn is_inline_xref(text: &str) -> bool {
    let low = text.trim_start().to_lowercase();
    let rest = ["figure", "fig.", "fig", "cuadro", "table", "tab.", "box"]
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
/// Parse a caption label: its kind (figure vs table), its FULL number token, and the
/// remainder of the line after the number. The number grammar is
/// `DIGITS ( ('.'|'-') DIGITS )* LETTER?` — so a hierarchical "6.2.1"/"0.1-1" or a
/// sub-label "1A" is captured whole (a separator only between two digit groups; a single
/// trailing letter only when not followed by another letter, so "1 Plot" keeps just "1").
/// Prefixes: "Figure"/"Fig."/"Fig "(no dot)/"Table"/"Cuadro"(Spanish)/"Box ".
pub(crate) fn caption_parts(text: &str) -> Option<(bool, String, &str)> {
    let t = text.trim_start();
    let low = t.to_lowercase();
    let (is_fig, after) = if low.starts_with("figure") {
        (true, &t[6..])
    } else if low.starts_with("fig.") {
        (true, &t[4..])
    } else if low.starts_with("cuadro") {
        (false, &t[6..])
    } else if low.starts_with("table") {
        (false, &t[5..])
    } else if low.starts_with("fig ") || low.starts_with("fig\t") {
        (true, &t[3..])
    } else if low.starts_with("box ") || low.starts_with("box\t") {
        (true, &t[3..])
    } else {
        return None;
    };
    let rest = after.trim_start();
    let b = rest.as_bytes();
    if b.is_empty() || !b[0].is_ascii_digit() {
        return None;
    }
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    while i + 1 < b.len() && (b[i] == b'.' || b[i] == b'-') && b[i + 1].is_ascii_digit() {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && b[i].is_ascii_alphabetic() && (i + 1 >= b.len() || !b[i + 1].is_ascii_alphabetic()) {
        i += 1;
    }
    Some((is_fig, rest[..i].to_string(), &rest[i..]))
}

/// If a line is a figure/table caption, return (is_figure, full number).
pub(crate) fn caption_label(text: &str) -> Option<(bool, String)> {
    caption_parts(text).map(|(f, n, _)| (f, n))
}

/// A multi-page CONTINUATION marker ("Figure 5.—Continued", "Table 2 (continued)"): the
/// label's tail, after separators, is ONLY a "continued" token. It must not be emitted as a
/// new caption (it would duplicate the original figure's number). A real caption that merely
/// begins "Figure 5. Continuation of the survey …" is NOT matched (the tail has more words).
pub(crate) fn caption_is_continued(text: &str) -> bool {
    match caption_parts(text) {
        // Strip ANY non-alphanumeric edge chars so the separator is irrelevant — a dash,
        // colon, dot, paren, or a mojibake C1 byte (reportlab/WinAnsi em-dash 0x97).
        Some((_, _, tail)) => {
            let core = tail.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'');
            matches!(core.to_lowercase().as_str(), "continued" | "cont" | "cont'd")
        }
        None => false,
    }
}

/// Does `text` END in a DOT-LEADER run (≥4 leader dots, tolerating spaces between them and
/// an optional trailing page number)? This is the signature of a List-of-Figures / Table-of-
/// Contents entry: "Overview of the system … . . . . . 95". A real caption never carries a
/// 4+ leader-dot run.
pub(crate) fn dotleader_tail(text: &str) -> bool {
    // Drop a trailing page-number token (and the whitespace before it), then count the
    // consecutive leader dots at the new end.
    let mut tail = text.trim_end();
    let without_pageno = tail.trim_end_matches(|c: char| c.is_ascii_digit());
    if without_pageno.len() < tail.len() {
        tail = without_pageno.trim_end();
    }
    let mut dots = 0usize;
    for c in tail.chars().rev() {
        match c {
            '.' | '…' | '·' => dots += if c == '…' { 3 } else { 1 },
            ' ' | '\t' => continue,
            _ => break,
        }
    }
    dots >= 4
}

/// A List-of-Figures / Table-of-Contents entry that merely LOOKS like a caption
/// ("Figure 3.1: Overview …"), so it would otherwise emit an empty `<figure>` shell. Its
/// dot-leader tail may sit on the caption's first line OR, when the title wraps, on a
/// following continuation line — so scan the caption line plus a few tight continuation
/// lines below it. Per-line signal only (no "List of Figures" heading gate needed).
pub(crate) fn is_dotleader_toc(lines: &[Line], idx: usize) -> bool {
    if caption_parts(&lines[idx].text()).is_none() {
        return false; // only relevant for caption-shaped lines
    }
    let base_sz = lines[idx].size.max(1.0);
    let mut prev_y = lines[idx].y;
    for k in idx..(idx + 4).min(lines.len()) {
        if k > idx {
            // stop if the next line isn't a tight continuation just below this one
            let dy = prev_y - lines[k].y;
            if dy < 0.0 || dy > base_sz * 2.0 {
                break;
            }
        }
        if dotleader_tail(&lines[k].text()) {
            return true;
        }
        prev_y = lines[k].y;
    }
    false
}

/// Gather a (possibly multi-line) caption block starting at line `idx`: the
/// caption sentence plus its continuation lines, stopping at an indented new
/// paragraph, a vertical gap, a heading/list, or another caption. Tiny stray
/// superscripts (footnote markers) are skipped, not treated as a break. Returns
/// the rendered caption HTML and the line indices consumed.
pub(crate) fn gather_caption(lines: &[Line], idx: usize, body: f32, profile: &DocProfile) -> (String, Vec<usize>) {
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
        if detect_header(l, body, Some(profile)).is_some() || list_kind(&l.text()).is_some() || caption_label(&l.text()).is_some() {
            break;
        }
        append_piece(&mut html, &render_runs(&l.runs));
        used.push(k);
        prev_y = l.y;
        k += 1;
    }
    (html, used)
}
