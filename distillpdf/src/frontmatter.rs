//! Front-matter of an academic paper: the title, authors (linked to their
//! organisation), abstract, and keywords extracted from page 1. Plain data only —
//! `lib.rs` turns this into the `pdf.metadata()` dict. The detection that fills these
//! (it reads the html.rs line/run model) lives below the data types.

#[derive(Default, Clone)]
pub struct FrontMatter {
    pub title: String,
    pub authors: Vec<Author>,
    /// Distinct affiliation/organisation lines, in marker order (index ~ the `<sup>`
    /// marker on the authors that reference them).
    pub affiliations: Vec<String>,
    pub abstract_text: Option<String>,
    pub keywords: Vec<String>,
}

#[derive(Default, Clone)]
pub struct Author {
    pub name: String,
    /// The affiliation markers attached to the name (`"1"`, `"2"`, `"*"`, …) — drive
    /// the `<sup>` in the rendered author list.
    pub markers: Vec<String>,
    /// The resolved organisation text (multiple joined with `"; "`), if linkable.
    pub affiliation: Option<String>,
}

use crate::html::{esc, initials_count, looks_like_reference, numbered_level, roman_section, FOOTNOTE_MARKERS};
use crate::layout::{lines_of, Line};
use crate::text;
use lopdf::Document;
use std::collections::HashSet;

/// Split an affiliation line `"1Institut …"` / `"* University …"` into its marker key
/// and the organisation text. Returns None if there is no leading digit/footnote key.
pub(crate) fn parse_affiliation(t: &str) -> Option<(String, String)> {
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
pub(crate) fn is_lone_marker(t: &str) -> bool {
    let t = t.trim();
    let n = t.chars().count();
    ((1..=3).contains(&n) && t.chars().all(|c| c.is_ascii_digit()))
        || (n == 1 && FOOTNOTE_MARKERS.contains(t.chars().next().unwrap()))
}

/// Strip a leading label (`Abstract`, `Keywords`, …) and any trailing separator.
pub(crate) fn strip_label(line: &str, labels: &[&str]) -> String {
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
pub(crate) fn flush_author(name: &mut String, markers: &mut Vec<String>, out: &mut Vec<(String, Vec<String>)>) {
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
pub(crate) fn detect_front_matter(lines: &[Line], body: f32) -> (FrontMatter, HashSet<usize>) {
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
pub(crate) fn emit_header_block(fm: &FrontMatter, out: &mut String) {
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
pub(crate) fn find_document_title(lines: &[Line], body: f32) -> Option<(String, HashSet<usize>)> {
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
pub(crate) fn find_title_sized(lines: &[Line], body: f32) -> Option<(String, HashSet<usize>)> {
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
pub(crate) fn emit_document_title(lines: &mut Vec<Line>, body: f32, out: &mut Vec<crate::html::PageElement>) {
    if let Some((title, set)) = find_document_title(lines, body) {
        // The title's bbox is the union of the lines it consumed (in PDF user space).
        let bbox = set.iter().filter_map(|&j| lines.get(j)).fold(None, |acc, l| {
            crate::html::bbox_union(acc, Some([l.x0, l.y, l.x1.max(l.x0), l.y + l.size.max(0.0)]))
        });
        out.push(crate::html::PageElement::at(crate::html::ElKind::Heading { level: 1, id: String::new(), text: esc(&title) }, bbox));
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
pub(crate) fn extract_front_matter(doc: &Document, raw: &[u8]) -> FrontMatter {
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
pub(crate) fn is_paper_front_matter(fm: &FrontMatter) -> bool {
    !fm.authors.is_empty()
}
