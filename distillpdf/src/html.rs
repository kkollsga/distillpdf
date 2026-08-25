//! "AI-ready" HTML conversion: thin, semantic HTML from a PDF.
//!
//! Per-page <section>; headings (font-size clustering), bold/italic (font flags),
//! lists, monospace/code, tables (positioned), inline embedded images. No CSS,
//! no classes, no script — minimal tags for LLM consumption.

use crate::captions::*;
use crate::extract;
use crate::frontmatter::*;
use crate::geom::{self, Rect};
use crate::headings::*;
use crate::img;
use crate::layout::*;
use crate::links;
use crate::nav::*;
use crate::postprocess::*;
use crate::profile::{DocProfile, HeadingTier};
use crate::table::TableAnalysis;
use crate::text::{self, SourceId, SourceSlice, Span};
use crate::vector;
use lopdf::ObjectId;
#[cfg(test)]
use lopdf::Document;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};

/// Canonical per-page lookup from one painted occurrence to its accepted table intervals.
/// Built once from the final winning table set; prose-span splitting never rescans tables.
struct TableClaimIndex {
    by_source: BTreeMap<SourceId, Vec<(u32, u32)>>,
}

impl TableClaimIndex {
    fn new(tables: &[crate::table::PositionedTableAnalysis]) -> Self {
        let mut by_source: BTreeMap<SourceId, Vec<(u32, u32)>> = BTreeMap::new();
        for claim in tables.iter().flat_map(|table| &table.claim.slices) {
            by_source
                .entry(claim.source())
                .or_default()
                .push((claim.char_start(), claim.char_end()));
        }
        for ranges in by_source.values_mut() {
            ranges.sort_unstable();
            let mut union: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
            for &(start, end) in ranges.iter() {
                if let Some((_, previous_end)) = union.last_mut() {
                    if start <= *previous_end {
                        *previous_end = (*previous_end).max(end);
                        continue;
                    }
                }
                union.push((start, end));
            }
            *ranges = union;
        }
        Self { by_source }
    }

    fn overlapping(&self, source: SourceSlice) -> &[(u32, u32)] {
        let Some(ranges) = self.by_source.get(&source.source()) else {
            return &[];
        };
        let first = ranges.partition_point(|&(_, end)| end <= source.char_start());
        let tail = &ranges[first..];
        let count = tail.partition_point(|&(start, _)| start < source.char_end());
        &tail[..count]
    }
}

/// Push only the unclaimed fragments of one painted span into the page's prose input.
/// Geometry advances along the span's baseline, so this is valid for both page-space rotated
/// text and display-space upright prose. Untouched spans remain value-for-value clones; a
/// wholly claimed span returns without cloning, visiting, or allocating.
fn push_unclaimed_fragments(
    span: &Span,
    claims: &TableClaimIndex,
    out: &mut Vec<Span>,
) {
    let source = span.source;
    let overlapping = claims.overlapping(source);
    if overlapping.is_empty() {
        out.push(clone_span(span));
        return;
    }
    if overlapping.len() == 1
        && overlapping[0].0 <= source.char_start()
        && overlapping[0].1 >= source.char_end()
    {
        return;
    }

    let chars: Vec<char> = span.text.chars().collect();
    debug_assert_eq!(source.char_len() as usize, chars.len());
    let mut cuts = vec![source.char_start(), source.char_end()];
    for &(start, end) in overlapping {
        cuts.push(start.max(source.char_start()));
        cuts.push(end.min(source.char_end()));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let width = if span.width > 0.1 {
        span.width
    } else {
        chars.len() as f32 * span.size * 0.5
    };
    let per_char = if chars.is_empty() { 0.0 } else { width / chars.len() as f32 };
    let (dx, dy) = (span.angle.cos() * per_char, span.angle.sin() * per_char);
    for range in cuts.windows(2) {
        let (start, end) = (range[0], range[1]);
        if start == end {
            continue;
        }
        let from = (start - source.char_start()) as usize;
        let to = (end - source.char_start()) as usize;
        let owned = overlapping.iter().any(|&(lo, hi)| start >= lo && end <= hi);
        if owned {
            continue;
        }
        let text: String = chars[from..to].iter().collect();
        out.push(Span {
            x: span.x + dx * from as f32,
            y: span.y + dy * from as f32,
            width: per_char * (to - from) as f32,
            text,
            source: source.sub_slice(from, to),
            ..clone_span(span)
        });
    }
}

fn prose_spans_without_table_claims(
    spans: &[Span],
    claims: &TableClaimIndex,
) -> Vec<Span> {
    if claims.by_source.is_empty() {
        return spans.iter().map(clone_span).collect();
    }
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        push_unclaimed_fragments(span, claims, &mut out);
    }
    out
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

pub(crate) use crate::textutil::esc;

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

pub(crate) fn numbered_level(text: &str) -> Option<u8> {
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
pub(crate) fn roman_section(text: &str) -> Option<u8> {
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
pub(crate) fn has_year(t: &str) -> bool {
    t.as_bytes().windows(4).any(|w| {
        ((w[0] == b'1' && w[1] == b'9') || (w[0] == b'2' && w[1] == b'0'))
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
    })
}

/// Count author-initial tokens (" X." — capital then period at a word start).
/// Two or more signals a citation / running-head author list, not a section.
pub(crate) fn initials_count(t: &str) -> usize {
    let b = t.as_bytes();
    (0..b.len())
        .filter(|&i| {
            b[i].is_ascii_uppercase()
                && b.get(i + 1) == Some(&b'.')
                && (i == 0 || b[i - 1] == b' ')
        })
        .count()
}


pub(crate) const TOP_HEADS: &[&str] = &[
    "abstract", "references", "acknowledgements", "acknowledgments",
    "introduction", "conclusion", "conclusions", "appendix",
];

// Theorem-environment labels: bold leads, not section headers. "denition" /
// "proposition" etc. cover the fi-ligature-dropped spelling some PDFs emit.
pub(crate) const THEOREM_ENV: &[&str] = &[
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
pub(crate) fn looks_like_reference(s: &str) -> bool {
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
/// The page-level intermediate representation: the ordered sequence of typed content
/// elements a single page's render produces. This is THE in-memory IR of the single-stream
/// architecture (see docs/datamodel-design.md): `render_doc` materializes it per page,
/// [`emit_page_elements`] renders it to HTML (a pure function of the IR — byte-identical to
/// the legacy string-building it replaced), and the model build path projects it into
/// queryable [`crate::model::Block`]s. Each variant carries (a) the exact HTML fragment it
/// emits — so emission is a faithful concatenation — plus (b) the structured projection the
/// blocks need (inline-markup text, table cells, caption/label, figure asset + SVG, bbox).
///
/// Inline-markup encoding: a text-bearing element's `text` is the element's INNER HTML — i.e.
/// it carries the minimal inline vocabulary the renderer emits (`<b>/<i>/<a>/<sup>/<sub>/
/// <code>`), HTML-escaped otherwise. This is the pragmatic faithful choice: `emit_page_elements`
/// reproduces the fragment from `text` directly (no re-escaping, no re-markup), so the
/// blocks→HTML inverse in `crate::model::render` is byte-exact.
//
// The structured figure/caption fields (`id`/`caption`/`image`/`svg`/`text`/`is_figure`) are
// populated by the emit walk (this commit) and READ by the block projection in
// `crate::model::build` (the next stage of the single-stream refactor). Allowed dead until the
// projection lands — same staged-wiring pattern `model::mod` uses.
/// A bounding box in PDF user space `[x0, y0, x1, y1]` (origin bottom-left, points), threaded
/// from the render walk's positioned items onto the element it produced and unioned through the
/// cross-page merges, so the block projection can carry it. `None` for page-chrome elements
/// (dest anchors, the front-matter header) and constructs the walk produced from no positioned
/// line.
pub(crate) type Bbox = [f32; 4];

/// Union two bounding boxes (the enclosing box). `None` is the identity, so a run of merged
/// elements unions to the box covering all of them; merging a positioned element with an
/// unpositioned one keeps the positioned box.
pub(crate) fn bbox_union(a: Option<Bbox>, b: Option<Bbox>) -> Option<Bbox> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Rect::from(a).union(Rect::from(b)).into()),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// One page-body element: its kind-specific payload ([`ElKind`]) plus the bounding box of the
/// page region it came from (when the walk could attribute one). The bbox travels with the
/// element through the cross-page transforms (unioned on merge) so the block projection records
/// each block's position.
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct PageElement {
    pub(crate) kind: ElKind,
    pub(crate) bbox: Option<Bbox>,
}

impl PageElement {
    /// A page-chrome / unpositioned element (no bbox).
    pub(crate) fn new(kind: ElKind) -> PageElement {
        PageElement { kind, bbox: None }
    }
    /// A positioned element carrying the page region it was emitted from.
    pub(crate) fn at(kind: ElKind, bbox: Option<Bbox>) -> PageElement {
        PageElement { kind, bbox }
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub(crate) enum ElKind {
    /// `<a id="…"></a>` named-destination anchor(s) emitted at the page head (one string,
    /// possibly several anchors concatenated, ending in the `\n` the head emits).
    DestAnchors(String),
    /// The first-page semantic `<header>` front-matter block (title/authors/abstract/keywords)
    /// — opaque for projection (front-matter is reconstructed from `metadata`, not blocks).
    Header(String),
    /// A heading `<hN id="sec-…">inner</hN>`. `level` is the HTML tag number (1..6); `text` is
    /// the inner HTML (inline markup preserved); `id` is the `sec-…`/empty id on the tag.
    Heading { level: u8, id: String, text: String },
    /// A paragraph `<p>inner</p>`; `text` is the inner HTML.
    Para { text: String },
    /// A `<ul>`/`<ol>` list. `ordered` picks the tag; `items` are the per-`<li>` inner HTML
    /// strings (already escaped, no inline markup — list items are emitted via `esc`).
    List { ordered: bool, items: Vec<String> },
    /// A `<pre><code>…</code></pre>` monospace block; `text` is the escaped inner (with the
    /// trailing `\n` per line the emitter produced).
    Code { text: String },
    /// A footnote `<aside>` wrapping one `<p>` per note; `notes` are the per-note inner HTML.
    Footnotes { notes: Vec<String> },
    /// A `<table>` (with optional `<caption>`). Carries the full cell structure for projection:
    /// `header` rows preserve detached cells/colspans, while `header_rows` records how many
    /// leading rows of `header + grid` render as `<th>`. `caption` is `(num, html, below)`.
    Table(TableAnalysis),
    /// A `<figure>` (raster `<img>`/`<image N>`, vector `<svg>`, or composite). `html` is the
    /// exact fragment; `id` is the `fig-N` number (or empty); `caption` the figcaption inner;
    /// `image` the asset id when a raster placeholder is present; `svg` the inline SVG markup
    /// when the figure is/contains a vector graphic. The structured fields let the block carry
    /// the figure's identity + caption + asset reference; `html` lets emission stay byte-exact.
    Figure { html: String, id: String, caption: Option<String>, image: Option<String>, svg: Option<String> },
    /// A standalone caption (`<figure id="fig-N"><figcaption>…` with no graphic, or a table
    /// caption `<div id="tab-N">…`). `html` is the fragment; `text` the caption inner; `id` the
    /// `fig-N`/`tab-N`; `is_figure` distinguishes the two shells.
    Caption { html: String, id: String, text: String, is_figure: bool },
}

impl PageElement {
    /// The exact HTML fragment this element contributes to the page body. Concatenating these
    /// in order reproduces the legacy string-built page body byte-for-byte.
    pub(crate) fn html(&self) -> String {
        self.kind.html()
    }
}

impl ElKind {
    /// The exact HTML fragment this element contributes to the page body.
    pub(crate) fn html(&self) -> String {
        use ElKind::*;
        match self {
            DestAnchors(s) | Header(s) => s.clone(),
            Code { text } => text.clone(),
            Heading { level, id, text } => {
                if id.is_empty() {
                    format!("<h{level}>{text}</h{level}>")
                } else {
                    format!("<h{level} id=\"{id}\">{text}</h{level}>")
                }
            }
            Para { text } => format!("<p>{text}</p>"),
            List { ordered, items } => {
                let tag = if *ordered { "ol" } else { "ul" };
                let mut s = format!("<{tag}>");
                for it in items {
                    s.push_str(&format!("<li>{it}</li>"));
                }
                s.push_str(&format!("</{tag}>"));
                s
            }
            Footnotes { notes } => {
                let mut s = String::from("<aside>");
                for n in notes {
                    s.push_str(&format!("<p>{n}</p>"));
                }
                s.push_str("</aside>");
                s
            }
            Table(table) => table_html(table),
            Figure { html, .. } | Caption { html, .. } => html.clone(),
        }
    }
}

/// Render an ordered page-element list to its page body HTML — the pure projection
/// `IR → HTML`. Used by both the parse path (after [`render_doc`] builds the list) and the
/// model-only re-render (which rebuilds the list from blocks), so the two share ONE emit path.
pub(crate) fn emit_page_elements(els: &[PageElement]) -> String {
    let mut out = String::new();
    for e in els {
        out.push_str(&e.html());
    }
    out
}

/// The table-emit core, over the bare parts (`header` rows of `(text, colspan)`, the data
/// `grid`, optional caption). The parse and model paths now both construct the canonical
/// analysis before reaching this emitter, so a `<table>` renders byte-identically whichever
/// side built it.
fn table_html(table: &TableAnalysis) -> String {
    if let Some(html) = table.fidelity_html() {
        return html.to_string();
    }
    let cap = table.caption.as_ref().map(|c| (c.number.as_str(), c.html.as_str(), c.below));
    let provenance = if table.has_proven_leading_tier() {
        " data-dpdf-proven-leading-tier"
    } else {
        ""
    };
    let semantic_spans = if table.has_semantic_spans() {
        " data-dpdf-semantic-spans"
    } else {
        ""
    };
    let mut tbl = match cap {
        Some((num, _, _)) => {
            format!("<table id=\"tab-{}\"{provenance}{semantic_spans}>", num_id(num))
        }
        None => format!("<table{provenance}{semantic_spans}>"),
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
    // Detached rows render before the regular grid, but ownership and semantics are distinct:
    // `header` preserves the visible cell sequence/colspans while `header_rows` alone decides
    // which leading rows are `<th>`. This permits exact no-header and multi-tier declarations,
    // and lets inference reclassify over-attached rows without moving or dropping content.
    let mut emit_row = |row: &[crate::table::CellAnalysis], semantic_header: bool| {
        tbl.push_str("<tr>");
        for cell in row {
            if cell.covered {
                continue;
            }
            let tag = if semantic_header { "th" } else { "td" };
            let mut attrs = String::new();
            let colspan = cell.colspan.max(1);
            let rowspan = cell.rowspan.max(1);
            if semantic_header {
                let scope = if colspan > 1 { "colgroup" } else { "col" };
                attrs.push_str(&format!(" scope=\"{scope}\""));
            }
            if colspan > 1 {
                attrs.push_str(&format!(" colspan=\"{colspan}\""));
            }
            if rowspan > 1 {
                attrs.push_str(&format!(" rowspan=\"{rowspan}\""));
            }
            tbl.push_str(&format!("<{tag}{attrs}>{}</{tag}>", esc(cell.text.trim())));
        }
        tbl.push_str("</tr>");
    };
    for (ri, hrow) in table.header.iter().enumerate() {
        emit_row(hrow, ri < table.header_rows);
    }
    for (ri, row) in table.grid.iter().enumerate() {
        emit_row(row, table.header.len() + ri < table.header_rows);
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

/// A footnote block's lines as the per-note inner HTML strings (each a `<p>` body in the
/// caller's `<aside>`): a lone marker number ("1") begins a new footnote and is joined to the
/// definition that follows ("1." + "https://…"); wrapped continuation lines fold in
/// (de-hyphenated). Returns the notes so the caller can build one [`PageElement::Footnotes`].
fn footnote_notes(lines: &[&Line]) -> Vec<String> {
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
    let mut notes: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, notes: &mut Vec<String>| {
        if !cur.trim().is_empty() {
            notes.push(cur.trim().to_string());
            cur.clear();
        }
    };
    for l in lines {
        match lead_marker(&l.text()) {
            Some((num, rest)) => {
                flush(&mut cur, &mut notes);
                cur.push_str(&num);
                cur.push_str(". ");
                cur.push_str(&rest);
            }
            None => append_piece(&mut cur, render_runs(&l.runs).trim()),
        }
    }
    flush(&mut cur, &mut notes);
    notes
}

/// A single text line's PDF-user-space bbox `[x0, y0, x1, y1]` (the glyph baseline `y` to
/// `y + size`). The page-element bbox the block projection records is unioned from these.
fn box_of_line(l: &Line) -> Bbox {
    [l.x0, l.y, l.x1.max(l.x0), l.y + l.size.max(0.0)]
}

/// The union bbox of a run of lines (None for an empty run).
fn box_of_lines(lines: &[&Line]) -> Option<Bbox> {
    lines.iter().fold(None, |acc, l| bbox_union(acc, Some(box_of_line(l))))
}

/// Emit a run of consecutive text lines as headings / paragraphs / lists / code, pushing one
/// [`PageElement`] per emitted construct (a heading, a paragraph, a list, a code block, a
/// footnote aside). Headings carry NO id here — ids are minted later by the `assemble` tail,
/// matching the legacy bare-`<hN>` emit.
/// Everything `emit_lines` needs about the PAGE it is emitting, gathered so the emitter keeps
/// a three-argument signature as the prose rules grow: the body size and title size the
/// heading tiers are read against, the PDF-outline titles targeting this page, the document
/// typography profile and heading plan, and the accepted tables' rects (widened by one body
/// height, the same band `in_table` uses) for the grid-residue heading guard.
#[derive(Clone, Copy)]
struct ProseCtx<'a> {
    body: f32,
    title_sz: f32,
    promote: &'a [(String, u8)],
    profile: &'a DocProfile,
    plan: &'a HeadingPlan,
    table_interiors: &'a [(f32, f32, f32, f32)],
}

fn emit_lines(lines: &[&Line], ctx: ProseCtx<'_>, out: &mut Vec<PageElement>) {
    let ProseCtx { body, title_sz, promote, profile, plan, table_interiors } = ctx;
    // A line lying WHOLLY inside an accepted table's rect is grid residue — text the table
    // detector saw inside its own ruled area but placed in no cell. It is emitted (deleting it
    // was the text loss `fix(tables): enforce exclusive prose ownership` set out to end), but it
    // is never a SECTION HEADING: a section title does not live inside a grid. Containment is
    // strict on both axes, so prose beside or below a table — the case that commit exists for —
    // is untouched.
    let residue_of_table = |l: &Line| {
        table_interiors
            .iter()
            .any(|&(x0, x1, y0, y1)| l.y >= y0 && l.y <= y1 && l.x1 > x0 && l.x0 < x1)
    };
    let mut i = 0;
    // The currently-open paragraph. It is NOT flushed at a column-wrap block
    // boundary — a paragraph that wraps from the bottom of one column to the top
    // of the next stays one <p> (and the line-break hyphen de-hyphenates across
    // the wrap). It is flushed only at a real paragraph start, or a header/list/
    // mono boundary, or end of input.
    let mut para = String::new();
    // The bbox accumulator for the open paragraph: the union of every line folded into it
    // (across column wraps), threaded onto the emitted `Para` so the block carries its region.
    let mut para_box: Option<Bbox> = None;
    macro_rules! flush_para {
        () => {
            if !para.trim().is_empty() {
                out.push(PageElement::at(ElKind::Para { text: para.trim().to_string() }, para_box));
                para.clear();
                para_box = None;
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
            out.push(PageElement::at(ElKind::Footnotes { notes: footnote_notes(&lines[a..i]) }, box_of_lines(&lines[a..i])));
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
        // A forced match is an author-declared title — emit it at the level the AUTHOR
        // nested it at (so it lands in the TOC and the outline link resolves), overriding
        // any level `detect_header` would infer from its (often body-size) styling. The
        // outline's `level` is 0-based nesting depth; the logical scale here starts at 1
        // for a section (rendered <h2> via `(lvl + 1)` below), so depth d → logical d + 1.
        // Clamped at 5 so the tag stays within <h6>.
        // Cap at 12 words: a longer match is a multi-line/sentence-like title — promoting
        // it would read as a sentence heading. Those stay plain text in the outline nav.
        let forced: Option<u8> = if promote.is_empty() || txt.split_whitespace().count() > 12 {
            None
        } else {
            let key = title_key(&txt);
            promote.iter().find(|(k, _)| *k == key).map(|(_, d)| d.saturating_add(1).min(5))
        };
        if !in_enumerated_run(lines, i) && !colon_introduced_list(lines, i) {
        if let Some((lvl, k)) = if let Some(flvl) = forced {
            Some((flvl, ln.runs.len()))
        } else if residue_of_table(ln) {
            None
        } else {
            header_at(lines, i, body, profile, plan)
        } {
            // HTML heading tag: reserve <h1> for the document title (the largest
            // text). Sections (logical level 1) become <h2>, subsections <h3>,
            // etc., so the outline nests under a single <h1>.
            flush_para!(); // a heading ends any open paragraph
            let tag = if title_sz > body * 1.25 && ln.size >= title_sz - 0.5 {
                1
            } else {
                (lvl + 1).min(6)
            };
            out.push(PageElement::at(ElKind::Heading { level: tag, id: String::new(), text: render_runs(&ln.runs[..k]) }, Some(box_of_line(ln))));
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
                    // The run-in body line seeds the open paragraph's box too.
                    para_box = bbox_union(para_box, Some(box_of_line(ln)));
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
            let mut item_htmls: Vec<String> = Vec::new();
            let mut list_box: Option<Bbox> = None;
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
                list_box = bbox_union(list_box, Some(box_of_line(lines[i])));
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
                    list_box = bbox_union(list_box, Some(box_of_line(l)));
                    prev_y = l.y;
                    i += 1;
                }
                item_htmls.push(esc(item.trim()));
            }
            out.push(PageElement::at(ElKind::List { ordered, items: item_htmls }, list_box));
            continue;
        }
        // code / monospace block
        if ln.mono {
            flush_para!();
            let mut inner = String::new();
            let mut code_box: Option<Bbox> = None;
            while i < lines.len() && lines[i].mono && list_kind(&lines[i].text()).is_none() {
                inner.push_str(&esc(&lines[i].text()));
                inner.push('\n');
                code_box = bbox_union(code_box, Some(box_of_line(lines[i])));
                i += 1;
            }
            out.push(PageElement::at(ElKind::Code { text: format!("<pre><code>{inner}</code></pre>") }, code_box));
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
            para_box = bbox_union(para_box, Some(box_of_line(l)));
        }
        // Intentionally NOT flushed here: a paragraph may continue in the next
        // column's block (handled by the j==0 continuation rule above).
    }
    // Final flush: emit any paragraph still open at end of input (no reset needed — we return).
    if !para.trim().is_empty() {
        out.push(PageElement::at(ElKind::Para { text: para.trim().to_string() }, para_box));
    }
}

/// Append a rendered line to a paragraph, joining a line-break hyphen. The hyphen
/// may be wrapped in trailing close tags (e.g. `<a href="#ref">Rad-</a>` when the
/// split word is also a link anchor); we look past those, drop the hyphen, and
/// join with no space so "Rad-" + "ford et al." reads "Radford et al."
/// First non-whitespace character of a fragment, skipping any leading HTML tags.
fn first_visible(s: &str) -> Option<char> {
    crate::textutil::visible_chars(s, crate::textutil::TagBreak::Join).find(|c| !c.is_whitespace())
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

pub(crate) const FOOTNOTE_MARKERS: &str = "*†‡§¶";
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
    // BTreeMap, not HashMap: three consumers below read this map IN ITERATION ORDER —
    // `max_by_key` for the body cluster and the title font (both return the LAST maximum),
    // and the `cands` collect whose later `sort_by` is STABLE, so equal-ranking heading
    // tiers keep iteration order and `take(4)` picks by it. `HashMap`'s order varies per
    // map instance (`RandomState`), so any tie made the document's heading tiers — and the
    // rendered HTML — differ run to run. Ordered by (size, bold, font), ties now resolve to
    // the largest size / bold / highest font id, deterministically.
    let mut clusters: BTreeMap<(i32, bool, u32), Acc> = BTreeMap::new();
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
pub fn to_html(
    access: &dyn crate::access::DocumentAccess,
    mode: Mode,
    inline_images: bool,
    include_toc: bool,
) -> String {
    let (body, img_uris, outline) = render_doc(access, mode, inline_images);
    assemble(body, mode, include_toc, &outline, &img_uris, inline_images)
}

/// One page's post-transform element IR: the page number, the typed [`PageElement`] list (after
/// the cross-page transforms), and the page-LOCAL deferred image-URI list (`\0idx\0`-indexed).
/// This is the single-stream document IR — `to_html` emits + merges it; `crate::model::build`
/// projects it into blocks; both see EXACTLY the same post-transform elements.
pub(crate) type PageIR = (u32, Vec<PageElement>, Vec<String>);

/// The render-pipeline HEAD up to the element IR: all the analysis + per-page render + the
/// cross-page element transforms, producing the post-transform per-page [`PageIR`] list, the
/// PDF's own outline. [`render_doc`] emits + merges this into the PRE-id, PRE-nav body; the model
/// build path ([`crate::model::build`]) projects the SAME elements into blocks. Splitting the IR
/// from the emit is what lets HTML and the model derive from one materialized structure.
pub(crate) fn render_doc_elements(
    access: &dyn crate::access::DocumentAccess,
    mode: Mode,
    inline_images: bool,
) -> (Vec<PageIR>, Vec<links::OutlineEntry>) {
    // Optional coarse phase profiler: set DPDF_PROFILE=1 to print per-phase WALL time to
    // stderr. `prof_phase(label, ||…)` times a closure; zero cost when unset.
    let prof = std::env::var_os("DPDF_PROFILE").is_some();
    let prof_start = if prof { Some(std::time::Instant::now()) } else { None };
    let phase = |label: &str, t: std::time::Instant| {
        if prof {
            eprintln!("  {label:<16} {:8.1}ms", t.elapsed().as_secs_f64() * 1e3);
        }
    };

    let pages = access.pages_or_empty();

    // Document-wide body font size = most common rounded span size. Spans are extracted
    // per page in PARALLEL (each page is independent and read-only on the document); the
    // histogram is folded sequentially afterwards (cheap).
    let t = std::time::Instant::now();
    let mut page_spans: Vec<(u32, ObjectId, Vec<Span>)> = pages
        .par_iter()
        .map(|page| {
            (
                page.number,
                page.id,
                text::extract_spans(access, page.id).unwrap_or_default(),
            )
        })
        .collect();
    page_spans.sort_by_key(|(pno, _, _)| *pno);
    phase("01_spans", t);
    // BTreeMap: `max_by_key` below returns the LAST maximum in iteration order, so with a
    // `HashMap` two equally-common body sizes picked a different winner run to run — and
    // the body size drives every heading/paragraph decision downstream. Ascending key order
    // makes the tie-break "the larger size wins", deterministically.
    let mut hist: BTreeMap<i32, usize> = BTreeMap::new();
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
    for lk in links::extract_links(access) {
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
    for d in links::named_destinations(access) {
        dests_by_page.entry(d.page).or_default().push((slug(&d.name), d.y));
    }

    // The PDF's own outline (bookmarks): used both to drive the nav and — per target
    // page — to promote matching lines to headings (so body-size section titles the
    // visual cues miss are still recognised, and the outline TOC links resolve).
    let outline = links::outline(access);
    // Each entry carries the author's own nesting depth (`OutlineEntry::level`, 0-based)
    // alongside the match key, so a promoted line lands at the level the author declared
    // instead of being flattened to a top-level section.
    let mut promote_by_page: std::collections::HashMap<u32, Vec<(String, u8)>> = std::collections::HashMap::new();
    for e in &outline {
        if e.page > 0 {
            promote_by_page.entry(e.page).or_default().push((title_key(&e.title), e.level));
        }
    }

    // Document-wide style profile: the body/heading size·weight·font tiers, numbering,
    // outline presence and column layout — drives heading classification per-document.
    let mut profile = build_doc_profile(&page_spans, body, title_sz, !outline.is_empty());
    // Document-wide page chrome (running headers / footers / page numbers): detected once
    // over the same materialized spans, then filtered out per page at the SPAN level below —
    // before table detection, the footnote mask and paragraph assembly, the stages that
    // were previously absorbing chrome into `<td>`s, `<aside>`s and glued paragraphs.
    let chrome = crate::chrome::plan_chrome(access, &page_spans, body);
    profile.running_heads = chrome.running_heads.clone();
    // L0 evidence: the tables the document DECLARES (`/StructTreeRoot`), keyed by page. Read
    // once for the whole document — the tree is document-wide and a table may straddle a page
    // break — and empty for the untagged majority.
    let declared_tables = crate::structtree::declared_tables(access);
    // Global heading pre-detection: distrust over-used emphasis/label styles so a filing's
    // line-item flood doesn't read as hundreds of headings (see plan_headings).
    let head_plan = plan_headings(&page_spans, body, &profile);

    // Render every page IN PARALLEL into its own (html_fragment, image_uris). Each page
    // is independent and reads the document immutably; image data URIs are deferred as
    // page-LOCAL `\0<idx>\0` sentinels (so the string passes never touch the base64) and
    // remapped to global indices during the sequential merge below.
    let t = std::time::Instant::now();
    let owner_diagnostics_enabled = extract::table_owner_diagnostics_enabled();
    let renders: Vec<(u32, Vec<PageElement>, Vec<String>)> = page_spans
        .par_iter()
        .enumerate()
        .map(|(pidx, (pno, _pid, spans))| {
        let pno = pno;
        // The page's typed element IR — the single-stream in-memory representation. The walk
        // below pushes one [`PageElement`] per construct; the page body HTML is then a pure
        // `emit_page_elements` of this list (byte-identical to the legacy string-building),
        // and the model build path projects the same list into blocks.
        let mut els: Vec<PageElement> = Vec::new();
        // Per-page deferred inline-image data URIs (placeholder mode stores empty strings
        // to keep the index aligned for `<image N>` numbering). The leading `\0<idx>\0`
        // sentinel is rewritten to a global index at merge time.
        let mut img_uris: Vec<String> = Vec::new();
        // Anchor targets for this page's named destinations, so the semantic links
        // (#cite.x / #figure.n / #equation.n / #section.x) resolve. Empty <a id> at
        // the section head land the reader on the correct page + the exact target id.
        if let Some(ds) = dests_by_page.get(pno) {
            let mut anchors = String::new();
            for (sl, _y) in ds {
                anchors.push_str(&format!("<a id=\"{sl}\"></a>"));
            }
            anchors.push('\n');
            els.push(PageElement::new(ElKind::DestAnchors(anchors)));
        }
        // The page's `/Rotate`, as the page→display map. EVERYTHING this closure reasons
        // about geometrically — spans, lines, tables, figure boxes, link rects, the reading
        // order — is expressed in DISPLAY space from here on, i.e. the way a viewer sees the
        // page. On an upright page (`is_identity`) that is literally the page space it always
        // was, unchanged value for value.
        //
        // This is the boundary the turn belongs at, and it is not the one `vector.rs` chose
        // for the figure path. There, the walk and its clustering thresholds stay page-space
        // and the turn happens at the page→SVG-local emit, because those thresholds are
        // orientation-sensitive and folding the turn in earlier would change which clusters
        // become figures. Here the orientation-sensitivity is the POINT: `lines_of` bands by
        // y and orders by x, so a quarter-turned page's body only groups into lines at all
        // once it is read in display orientation — and every consumer of a `Line` compares it
        // against a table/figure/image box, so those must be turned with it or the two
        // spaces meet. Hence: one turn, applied to every page-space quantity this closure
        // touches, and the page-space originals kept only for the SVG emitters (which do
        // their own turn from page space — see `PlacedSvg::rot`).
        let pbox = crate::pdfobj::page_box(access, *_pid).unwrap_or([0.0, 0.0, crate::pdfobj::DEFAULT_PAGE_PTS.0, crate::pdfobj::DEFAULT_PAGE_PTS.1]);
        let turn = geom::PageTurn::new(crate::pdfobj::page_rotation(access, *_pid), pbox);
        // Display-space spans. `turned` owns them only on a turned page; upright, `dspans` IS
        // `spans`, so no page in any upright document allocates or copies anything here.
        let turned: Option<Vec<Span>> = (!turn.is_identity()).then(|| spans.iter().map(|s| turn_span(turn, s)).collect());
        let dspans: &[Span] = turned.as_deref().unwrap_or(spans.as_slice());
        // Chrome rows (running headers/footers/page numbers) removed before ANY consumer
        // reads the spans; a chrome-free page keeps its borrows, allocation-free. The SAME
        // mask filters both space representations — `spans` (page space, what the SVG
        // emitters are handed) and `dspans` (display space) correspond POSITIONALLY, and
        // the figure-label pass zips them, so filtering only one would misalign every
        // pairing after the first dropped span.
        let chrome_mask = chrome.drop_mask(dspans, turn.rect(pbox[0], pbox[2], pbox[1], pbox[3]));
        let keep = |src: &[Span], mask: &[bool]| -> Vec<Span> {
            src.iter().zip(mask).filter(|(_, d)| !**d).map(|(s, _)| clone_span(s)).collect()
        };
        let dechromed_p: Option<Vec<Span>> = chrome_mask.as_ref().map(|m| keep(spans, m));
        let dechromed_d: Option<Vec<Span>> = chrome_mask.as_ref().map(|m| keep(dspans, m));
        let spans: &[Span] = dechromed_p.as_deref().unwrap_or(spans.as_slice());
        let dspans: &[Span] = dechromed_d.as_deref().unwrap_or(dspans);
        // A vector figure's / raster's box in display space. Every layout comparison below
        // goes through these; `v.x_left`/`im.x_left` stay page-space for the SVG emitters.
        let dvbox = |v: &vector::PlacedSvg| turn.rect(v.x_left, v.x_right, v.y_bottom, v.y_top);
        let dibox = |im: &img::Placed| turn.rect(im.x_left, im.x_right, im.y_bottom, im.y_top);
        let mut images = img::positioned_images(access, *_pid, inline_images);
        // One vector walk, two answers: the figures, and the page's RULING — L1's second
        // evidence source for tables (`extract::detect_tables_pos`). The ruling arrives in
        // page space like every other geometry the walk produces, so it takes the same turn
        // the spans did, or a rotated page's lattice lands nowhere.
        let (raw_vectors, weak_vectors, page_rules) = vector::positioned_vectors_ruled(access, *_pid);
        let page_rules = turn_rules(turn, page_rules);
        let mut tables = extract::detect_tables_pos(dspans, &page_rules);
        // Vector figures that carry a "Figure N" caption — their *internal* text (a diagram's
        // node labels: "E[CLS] E1 … EN", "Trm Trm … Trm") now lands in the page span stream
        // (extract_spans recurses into Form XObjects), where detect_tables_pos reads the
        // regular label grid as a data TABLE. That false table overlaps the figure and the
        // not_in_table filter below would then drop the diagram, leaving an empty <figure>.
        // We mark these figure regions so a label grid sitting inside one is dropped as a
        // false table (below) — letting the diagram render as SVG with its labels intact.
        // The "Figure N" caption is the discriminator that keeps a real ruled FORM safe: a
        // form's table cells sit inside a large border vector too, but that vector carries no
        // figure caption (it's a "Table N"/uncaptioned form region), so its table survives.
        let captioned_fig_boxes: Vec<(f32, f32, f32, f32)> = {
            let cap_lines = lines_of(dspans.iter().map(clone_span).collect(), &no_links);
            raw_vectors
                .iter()
                .map(&dvbox)
                .filter(|&(xl, xr, yb, yt)| {
                    cap_lines.iter().any(|l| {
                        let cx = (l.x0 + l.x1) * 0.5;
                        opens_figure_caption(&l.text())
                            && !caption_is_continued(&l.text())
                            && (l.y - yb).abs().min((l.y - yt).abs()) < body * 4.0
                            && cx >= xl - 40.0
                            && cx <= xr + 40.0
                    })
                })
                .collect()
        };
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
        //   (c) a label grid sitting inside a CAPTIONED vector figure — a diagram's own node
        //       labels, read as a table now that Form-XObject text reaches the span stream.
        //       The captioned-figure marker (not a raster) is what tells this from a ruled form.
        // Left in, such a false table both consumes the figure's labels as cells AND
        // suppresses the overlapping vector, fragmenting a raster+vector plot (a Vp-depth
        // crossplot) into a lone raster plus loose axis text.
        tables.retain(|t| {
            let tr = t.bbox;
            let ta = tr.area().max(1.0);
            let raster_covered = images.iter().any(|im| {
                let (ixl, ixr, iyb, iyt) = dibox(im);
                let ir = Rect::new(ixl, iyb, ixr, iyt);
                let ia = ir.area().max(1.0);
                ia >= ta * 0.15 && tr.overlap_area(ir) >= ia * 0.5
            });
            let strip_in_plot = raw_vectors.iter().any(|v| {
                let (vxl, vxr, vyb, vyt) = dvbox(v);
                let vr = Rect::new(vxl, vyb, vxr, vyt);
                let va = vr.area().max(1.0);
                if !(vr.overlap_area(tr) >= ta * 0.6 && ta < va * 0.5) {
                    return false;
                }
                // …and the vector is a composite plot: it contains a substantial raster.
                images.iter().any(|im| {
                    let (ixl, ixr, iyb, iyt) = dibox(im);
                    let ir = Rect::new(ixl, iyb, ixr, iyt);
                    vr.overlap_area(ir) >= ir.area().max(1.0) * 0.5
                })
            });
            // A label grid belonging to a captioned vector figure — a diagram's node labels
            // / a scatter's point legend — read as a data table now that Form-XObject text
            // reaches the span stream. It is the figure's own content, so it must NOT block
            // the figure (the not_in_table filter would otherwise drop the diagram). Two
            // shapes, both keyed on the figure CARRYING A CAPTION (the discriminator that
            // keeps a real ruled FORM — no figure caption — safe):
            //   • the table sits inside the figure's ink box (center within it), or
            //   • the table is column-aligned with the figure (its x-center within the figure's
            //     x-span) and vertically OVERLAPS the figure's ink — i.e. label text the
            //     vector-ink bbox didn't quite cover (a scatter's top-edge legend).
            // A REAL data table that merely sits near a figure is column-offset or vertically
            // separated from the ink, so neither shape matches and it survives.
            let (tcx, tcy) = ((t.bbox.x0 + t.bbox.x1) * 0.5, (t.bbox.y0 + t.bbox.y1) * 0.5);
            let label_grid_in_fig = captioned_fig_boxes.iter().any(|&(xl, xr, yb, yt)| {
                let fr = Rect::new(xl, yb, xr, yt);
                let center_in = fr.contains(tcx, tcy);
                let v_overlap = yt.min(t.bbox.y1) > yb.max(t.bbox.y0);
                let x_aligned = tcx >= xl && tcx <= xr;
                let va = fr.area().max(1.0);
                // The table blankets the figure horizontally: its x-extent covers most of the
                // figure's width within the figure's y-band (the figure's labels mis-clustered
                // into a row wider than the figure, often spanning into the neighbouring column).
                // Bound the height so a genuine full-column data table that merely crosses the
                // figure's y-band isn't swallowed.
                let fw = fr.width().max(1.0);
                let xcov = fr.overlap_w(tr) / fw;
                let blankets = xcov >= 0.6 && v_overlap && tr.height() < fr.height() * 1.2;
                center_in || (x_aligned && v_overlap && ta < va) || blankets
            });
            !(raster_covered || strip_in_plot || label_grid_in_fig)
        });
        // ── L0: the tables this page DECLARES ──────────────────────────────────────────
        // A tagged PDF states its table structure in `/StructTreeRoot`; where a declaration
        // resolves to content the page actually paints, it is exact evidence and outranks
        // every heuristic above. It engages only inside its own region: the declared table
        // replaces whatever inference found there, and the rest of the page is detected
        // exactly as it was — a page carrying one declared and one undeclared table (IRS
        // f1040 p1, measured) keeps both. A document with no structure tree reaches an empty
        // map and nothing below this point runs, which is why L0 cannot move an untagged
        // document by so much as a byte.
        //
        // Placed AFTER the false-table filters above deliberately. Those filters answer "is
        // this grid of text really a figure's own labels", a question the declaration has
        // already answered; running them over a declared table would let a heuristic overrule
        // the file's own statement.
        if let Some(decl) = declared_tables.get(_pid) {
            let annots: Vec<(ObjectId, Rect)> = crate::walker::annot_rects(access, *_pid)
                .into_iter()
                .map(|(id, r)| {
                    let (x0, x1, y0, y1) = turn.rect(r.x0, r.x1, r.y0, r.y1);
                    (id, Rect::new(x0, y0, x1, y1))
                })
                .collect();
            let found = extract::declared_pos_tables(decl, dspans, &annots);
            // Per-page L0 trace, off unless asked for (`DPDF_L0=1`), same idiom as
            // `DPDF_PROFILE`: which declarations were accepted and which the trust rule
            // refused, and why. A refusal that is never visible is a refusal nobody audits.
            if std::env::var_os("DPDF_L0").is_some() {
                eprintln!("  L0 page {pno}: declared={} accepted={} refused={:?}", decl.len(), found.tables.len(), found.refused);
            }
            if !found.tables.is_empty() {
                let regions: Vec<Rect> = found.tables.iter().map(|t| t.bbox).collect();
                // An inferred table that substantially coincides with a declared region is
                // the same table read worse — the USGS grid inference splits six ways is
                // declared once — so it goes. "Substantially" is measured against the
                // SMALLER of the two, which is what makes the rule work in both directions:
                // inference over-splits a declared table (fragments inside the region) and it
                // also over-merges one (a declared block plus its neighbours read as a single
                // wide grid). Either way the declaration owns the structure there. A table
                // that merely clips a corner of the region is a different table and survives.
                tables.retain(|t| {
                    let tr = t.bbox;
                    !regions.iter().any(|r| r.overlap_area(tr) >= 0.5 * tr.area().min(r.area()).max(1.0))
                });
                tables.extend(found.tables);
            }
        }
        // Complete ruled continuation evidence against the FINAL accepted owner set. Caption
        // text deliberately remains unclaimed here, so it is an exact boundary barrier shared
        // by raw, rich and rendered grouping without caption-label interpretation.
        crate::table::finalize_continuation_proofs(*pno, dspans, &mut tables, turn, true);
        // Link rectangles are page-space too, and they are hit-tested against the spans in
        // `lines_of` — so they take the same turn, or a turned page's links land nowhere.
        let page_links = links_by_page.get(pno).unwrap_or(&no_links);
        let turned_links: Option<Vec<LinkBox>> = (!turn.is_identity()).then(|| {
            page_links
                .iter()
                .map(|l| {
                    let (x0, x1, y0, y1) = turn.rect(l.rect[0], l.rect[2], l.rect[1], l.rect[3]);
                    LinkBox { rect: [x0, y0, x1, y1], href: l.href.clone() }
                })
                .collect()
        });
        let plinks: &[LinkBox] = turned_links.as_deref().unwrap_or(page_links.as_slice());
        // The final accepted table set owns exact painted source intervals. Remove precisely
        // that union before prose line assembly: a partial ruled-cell cut keeps its unclaimed
        // Unicode fragments, while neighbouring prose inside the table bbox/margin survives.
        // This happens only after declared-table replacement and false-table filtering, so a
        // rejected/stale candidate can never consume a surface.
        let table_claims = TableClaimIndex::new(&tables);
        debug_assert!(
            tables.iter().all(|table| !table.claim.slices.is_empty()),
            "every accepted table must carry exact source ownership"
        );
        let prose_spans = prose_spans_without_table_claims(dspans, &table_claims);
        let mut lines = lines_of(prose_spans, plinks);
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
                let mut hdr = String::new();
                emit_header_block(&fm, &mut hdr);
                els.push(PageElement::new(ElKind::Header(hdr)));
                let mut i = 0usize;
                lines.retain(|_| {
                    let keep = !consumed.contains(&i);
                    i += 1;
                    keep
                });
            } else {
                emit_document_title(&mut lines, body, &mut els);
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
        //
        // …with one proportionality guard. The rule's whole justification is that the ink IS
        // the table's rules, so it only holds while the table can plausibly ACCOUNT for the
        // ink. `geology_usgs_fs.pdf` p1 is the counterexample: its cover map draws place
        // names one glyph at a time, and two of those label grids (5% and 9% of the map's
        // area) were deleting a 1,574-path map — county boundary, shaded study areas, both
        // pie charts, every city label. The page still emitted an `<svg>` (the USGS banner),
        // so no count could see the loss.
        //
        // So: a table that covers less than HALF of a figure that carries GRAPHIC INK
        // (curves or slanted lines — `vector::has_graphic_ink`, which no ruled table, TOC
        // dot-leader row or filing-chrome card can produce) is a label grid sitting *on* the
        // figure, not the figure's source. It no longer suppresses it. A real ruled form is
        // untouched twice over: its vector has no graphic ink, and its table coincides with
        // that vector rather than covering a corner of it.
        //
        // Half, not a quarter: `geology_usgs_volcanic_hazards_california.pdf` p30's Cal OES
        // county map has a label grid over 38% of it. A quarter was tuned on one document
        // and a fraction of a large diagram is still not that diagram's ink; the conjunction
        // with graphic ink is what carries the rule, not the size of the share.
        let not_in_table = |v: &vector::PlacedSvg| {
            let (vxl, vxr, vyb, vyt) = dvbox(v);
            let vr = Rect::new(vxl, vyb, vxr, vyt);
            let va = vr.area().max(1.0);
            !tables.iter().any(|t| {
                if !(vxl < t.bbox.x1 && vxr > t.bbox.x0 && vyb < t.bbox.y1 && vyt > t.bbox.y0) {
                    return false;
                }
                let tr = t.bbox;
                !(v.graphic_ink() && vr.overlap_area(tr) < va * 0.5)
            })
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
                    .map(|im| {
                        let (_, _, yb, yt) = dibox(im);
                        edge(yb, yt)
                    })
                    .chain(vectors.iter().map(|v| {
                        let (_, _, yb, yt) = dvbox(v);
                        edge(yb, yt)
                    }))
                    .fold(f32::INFINITY, f32::min);
                if strong_near <= gap {
                    continue; // caption already has a (strong) graphic
                }
                let mut best: Option<(usize, f32)> = None;
                for (j, v) in weak.iter().enumerate() {
                    if claimed[j] {
                        continue;
                    }
                    let (vxl, vxr, vyb, vyt) = dvbox(v);
                    let e = edge(vyb, vyt);
                    let x_overlap = vxr > l.x0 - body && vxl < l.x1 + body;
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
                y <= t.bbox.y1 + body && y >= t.bbox.y0 - body && x1 > t.bbox.x0 && x0 < t.bbox.x1
            })
        };
        // Caption recognition must see a label just outside the drawn frame even though the
        // broader body-sized ownership band consumes ordinary grid residue. A label whose
        // baseline is actually inside the frame remains table content, never a caption.
        let inside_table_frame = |x0: f32, x1: f32, y: f32| {
            tables.iter().any(|t| {
                y <= t.bbox.y1 && y >= t.bbox.y0 && x1 > t.bbox.x0 && x0 < t.bbox.x1
            })
        };
        // A vector figure's bbox — used to attach its labels and to keep that text
        // out of the body flow (it belongs to the figure, not the prose).
        let fig_boxes: Vec<(f32, f32, f32, f32)> = vectors.iter().map(&dvbox).collect();
        let in_figure = |x: f32, y: f32| fig_boxes.iter().any(|&(xl, xr, yb, yt)| x >= xl - 4.0 && x <= xr + 4.0 && y >= yb - 4.0 && y <= yt + 4.0);
        // A vector region that is really a FRAMED TEXT BLOCK — a bordered certificate / form
        // whose frame and rules are vector ink but whose content is a wall of body text — must
        // not scoop that text into the SVG. Tell it from a real chart/diagram by text density:
        // a chart carries a handful of short labels, a framed document many lines of multi-word
        // prose (often in smaller-than-body print). Mark such boxes so all their text stays in
        // the prose flow regardless of font size (these forms are routinely set below the
        // document body size, which the body-size prose gate below would otherwise miss).
        const FRAMED_DOC_MIN_PROSE_LINES: usize = 8;
        // Precompute, once per line, whether it is a multi-word non-axis "prose line" plus its
        // center — so the per-box scan below is a cheap point/flag test, not a fresh `l.text()`
        // build for every (box, line) pair (that nested rebuild dominated render time on a
        // form-heavy doc with many vector boxes).
        let line_is_prose: Vec<bool> = lines
            .iter()
            .map(|l| l.text().split_whitespace().count() >= 4 && !is_axis_label_text(&l.text()))
            .collect();
        let line_centers: Vec<(f32, f32)> = lines.iter().map(|l| ((l.x0 + l.x1) * 0.5, l.y)).collect();
        let framed_doc_boxes: Vec<(f32, f32, f32, f32)> = fig_boxes
            .iter()
            .copied()
            .filter(|&(xl, xr, yb, yt)| {
                // A CAPTIONED figure ("Figure N") is a diagram, never a framed certificate —
                // its many short node labels must NOT make it read as a wall of prose (which
                // would scoop them into the body and out of the SVG). Only an UNcaptioned
                // bordered text block qualifies as a framed document.
                let captioned = captioned_fig_boxes
                    .iter()
                    .any(|&(cxl, cxr, cyb, cyt)| (xl - cxl).abs() < 1.0 && (xr - cxr).abs() < 1.0 && (yb - cyb).abs() < 1.0 && (yt - cyt).abs() < 1.0);
                !captioned
                    && line_centers
                        .iter()
                        .zip(&line_is_prose)
                        .filter(|&(&(cx, cy), &prose)| prose && cx >= xl && cx <= xr && cy >= yb && cy <= yt)
                        .count()
                        >= FRAMED_DOC_MIN_PROSE_LINES
            })
            .collect();
        let in_framed_doc = |x: f32, y: f32| {
            framed_doc_boxes
                .iter()
                .any(|&(xl, xr, yb, yt)| x >= xl - 4.0 && x <= xr + 4.0 && y >= yb - 4.0 && y <= yt + 4.0)
        };
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
            let cx = (l.x0 + l.x1) * 0.5;
            let body_prose = l.size >= body * 0.95
                && l.text().split_whitespace().count() > 5
                && !is_axis_label_text(&l.text()) // a body-size numeric tick row is a figure label, not prose
                && detect_header(l, body, Some(&profile)).is_none()
                && in_figure(cx, l.y);
            // Inside a framed text block, keep ALL multi-word text as prose — including the
            // small print these forms use, which the body-size gate above would miss.
            let framed_prose = in_framed_doc(cx, l.y)
                && l.text().split_whitespace().count() >= 2
                && !is_axis_label_text(&l.text());
            if body_prose || framed_prose {
                prose_rows.push((l.x0, l.x1, l.y - 1.0, l.y + l.size + 1.0));
            }
        }
        // The y-band one line occupies, as the point tests below want it.
        let band_of = |l: &Line| (l.x0, l.x1, l.y - 1.0, l.y + l.size + 1.0);
        let in_prose = |x: f32, y: f32| {
            prose_rows.iter().any(|&(x0, x1, y0, y1)| x >= x0 - 2.0 && x <= x1 + 2.0 && y >= y0 && y <= y1)
        };
        // A line OUTSIDE the figure's ink is claimed whole or not at all.
        //
        // `near_fig_label` decides span by span, and a span is not a unit of meaning. On
        // `econ_EM_2606_02234.pdf` p25 the figure is the 58pt band of a booktabs table's header
        // rules; the margin below it reaches the first two data rows, and of each row it takes
        // only the numeric spans — every `0.33`, every `0.50` — while "Sim1a CIA-∇D only (α = 1,
        // γ = 2)" stays in the body, because a scenario name is not axis-shaped. The reader gets
        // an SVG of Bias/RMSE numbers with no row context, which is worse than either half alone.
        //
        // So: find the lines that lie outside every figure's ink and are NOT label lines when
        // judged AS A WHOLE, and refuse every span on that row. Judging the whole line is the
        // point — `is_axis_label_text` says yes to "0.33" and no to the row it belongs to.
        //
        // The refusal spans the CLAIMING FIGURE'S WIDTH, not just the rejected line's own. A
        // table row breaks into one line per column group the moment its cells are far enough
        // apart, so the numeric cells to the right of "Sim1a CIA-∇D only …" are lines of their
        // own and would each pass the whole-line test. What the figure may not do is take part
        // of a row it cannot take all of — so one rejected fragment closes the whole row, across
        // that figure's own x-span and no further (a second column's labels are untouched).
        //
        // Spans that belong to no line keep today's behaviour, so this only ever removes a claim
        // the line-level reading contradicts.
        let split_rows: Vec<(f32, f32, f32, f32)> = lines
            .iter()
            .filter(|l| {
                let cx = (l.x0 + l.x1) * 0.5;
                !in_figure(cx, l.y) && !near_fig_label(cx, l.y, l.size, &l.text())
            })
            .map(|l| {
                let (mut x0, mut x1, y0, y1) = band_of(l);
                for &(xl, xr, yb, yt) in &fig_boxes {
                    let in_margin = l.y >= yb - axis_margin && l.y <= yt + axis_margin;
                    let overlaps = l.x1 > xl - axis_margin && l.x0 < xr + axis_margin;
                    if in_margin && overlaps {
                        x0 = x0.min(xl);
                        x1 = x1.max(xr);
                    }
                }
                (x0, x1, y0, y1)
            })
            .collect();
        let on_split_row = |x: f32, y: f32| {
            split_rows.iter().any(|&(x0, x1, y0, y1)| x >= x0 - 2.0 && x <= x1 + 2.0 && y >= y0 && y <= y1)
        };
        // The same question asked of a whole LINE, by overlap rather than by centre. The body
        // side works in lines and the figure side in spans, and a centre test answers the two
        // differently for a line that straddles the band's edge — the figure refuses its spans
        // while the body still drops it as a label, and the row is emitted NEITHER place.
        let line_on_split_row = |l: &Line| {
            split_rows
                .iter()
                .any(|&(x0, x1, y0, y1)| l.x1 >= x0 - 2.0 && l.x0 <= x1 + 2.0 && l.y >= y0 && l.y <= y1)
        };
        // A figure caption ("Figure N …", "Table N …") sits just outside the figure ink,
        // within `near_fig_label`'s margin. Its numeric fragments (the "5.6"/"-" in
        // "Figure 5.6: …") otherwise read as axis ticks and get scooped INTO the SVG — and
        // duplicated, since the caption is also emitted normally. Exclude any span on a
        // caption line from figure-label capture.
        //
        // A caption is a PARAGRAPH, not a line. Excluding only the "Figure N …" line left its
        // continuation lines claimable, and the neighbouring figure ate them — `cs_LG_2606_02576`
        // p1 rendered "continual tuning streams." (Figure 1's last caption line) across the top of
        // Figure 2's SVG, and `cs_DS_2606_02492` p34 swept two whole lines of Figure 3's caption
        // into the figure. So the band runs from the caption line through the lines that continue
        // it: same type size, the next baseline down, and horizontally inside the caption's own
        // column. The first line that breaks any of those ends the caption, which is what keeps
        // the following body paragraph — and a second caption — out of the band.
        let mut caption_bands: Vec<(f32, f32, f32, f32)> = Vec::new();
        for l in lines.iter().filter(|l| caption_label(&l.text()).is_some()) {
            let mut block = vec![band_of(l)];
            let mut prev = l;
            // Walk DOWN the caption by nearest baseline rather than by index: `lines` is in
            // reading order, but a page header or a neighbouring column can sit between two
            // lines of the same caption, and an index walk would stop at the first of them.
            loop {
                let next = lines
                    .iter()
                    .filter(|nxt| {
                        let drop = prev.y - nxt.y;
                        (nxt.size - l.size).abs() < 0.6
                            && drop > 0.5
                            // Within 1.6 line-heights — a caption's own leading. The gap to
                            // whatever follows it is larger: `physics_higgs_cms_discovery.pdf`
                            // p11 sets its caption at a 13.4pt pitch and then leaves 19.3pt
                            // before the table's header row, and at a looser bound the block
                            // ran straight on into the table and took its column headings out
                            // of the figure.
                            && drop < prev.size * 1.6
                            // A caption's FIRST line is indented by its marker ("Figure 3 "),
                            // so the lines that continue it start further LEFT, not further
                            // right — `cs_DS_2606_02492` p34 hangs its caption by 14pt. Bound
                            // the overhang rather than forbidding it, and hold the right edge
                            // to the caption's own column so a wider block below is not taken.
                            && nxt.x0 >= l.x0 - prev.size * 4.0
                            && nxt.x1 <= l.x1.max(nxt.x0) + prev.size * 2.0
                            && caption_label(&nxt.text()).is_none()
                            && detect_header(nxt, body, Some(&profile)).is_none()
                    })
                    .max_by(|a, b| a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal));
                match next {
                    Some(n) => {
                        block.push(band_of(n));
                        prev = n;
                    }
                    None => break,
                }
            }
            // Every line of the block gets the block's FULL column width. A caption's last
            // line often ends mid-column with a display fragment set beside it (a `⋁` and a
            // `kPunch[u, v]` on `cs_DS_2606_02492` p34 are lines of their own, to the right of
            // the text they belong to); keyed on the line's own x-extent, those fragments fall
            // outside the band and get scooped onto the figure alone.
            let bx0 = block.iter().map(|b| b.0).fold(f32::INFINITY, f32::min);
            let bx1 = block.iter().map(|b| b.1).fold(f32::NEG_INFINITY, f32::max);
            caption_bands.extend(block.into_iter().map(|(_, _, y0, y1)| (bx0, bx1, y0, y1)));
        }
        let in_caption = |x: f32, y: f32| {
            caption_bands.iter().any(|&(x0, x1, y0, y1)| x >= x0 - 2.0 && x <= x1 + 2.0 && y >= y0 && y <= y1)
        };
        // Render the figures' text as SVG <text>: a figure's labels (axis ticks, DAG
        // node labels) are the spans that fall within its bbox — whether drawn in the
        // page content OR inside a Form XObject (`extract_spans` now captures both into
        // `spans`). They're collected here and removed from the prose flow below (the
        // in_figure / fig_label filter), so each shows on its figure, not in the body.
        if !vectors.is_empty() {
            let mk = |s: text::Span| vector::LabelSpan { x: s.x, y: s.y, size: s.size, width: s.width, text: s.text, bold: s.bold, italic: s.italic, angle: s.angle };
            // A captioned figure's labels routinely sit just OUTSIDE its (tight) vector-ink
            // box — node labels above/beside a DAG, a pipeline diagram's stage captions. Claim
            // them onto the SVG within the same margin axis ticks use, so a label split between
            // form-internal text (inside the box) and page-direct text (just outside) is
            // reunited on the figure instead of leaking to a body <aside>.
            let cap_m = axis_margin;
            let in_captioned_fig_pt = |x: f32, y: f32| {
                captioned_fig_boxes
                    .iter()
                    .any(|&(xl, xr, yb, yt)| x >= xl - cap_m && x <= xr + cap_m && y >= yb - cap_m && y <= yt + cap_m)
            };
            let mut labels: Vec<vector::LabelSpan> = Vec::new();
            // Parallel to `labels`: does this span sit inside a table the page emits? A
            // figure whose text is almost entirely a table's cells is a callout panel
            // REPRODUCING that table, and must not print the numbers a second time — see
            // `vector::PlacedSvg::attach`. Display space, like every other box test here.
            let mut label_in_table: Vec<bool> = Vec::new();
            let in_table = |x: f32, y: f32| {
                tables.iter().any(|t| x >= t.bbox.x0 && x <= t.bbox.x1 && y >= t.bbox.y0 && y <= t.bbox.y1)
            };
            // Which spans a figure claims is decided in DISPLAY space (`ds`), against the
            // display-space figure boxes; what is HANDED to the figure is the PAGE-space span
            // (`s`), because `PlacedSvg` maps page space to its own turned local space itself
            // (`to_local`) — turning a label here as well would turn it twice.
            for (s, ds) in spans.iter().zip(dspans) {
                let (cx, cy) = (ds.x + ds.width * 0.5, ds.y + ds.size * 0.5);
                // Inside a CAPTIONED figure, all interior text is the diagram's labels — render
                // it on the SVG even if the in-figure "prose" gate flagged it (that gate guards
                // an UNcaptioned framed block). This keeps the labels visible (figure_text) while
                // the body flow drops them as figure content. A caption line is still excluded.
                let take =
                    (in_figure(cx, cy) || (near_fig_label(cx, cy, s.size, &s.text) && !on_split_row(cx, cy))) && !in_prose(cx, cy);
                // The same wholeness rule binds the captioned-figure blanket claim: it also
                // reaches outside the ink, and on `arxiv_nerf.pdf` p18 its margin catches the
                // page's RUNNING HEADER ("18 B. Mildenhall, P. P. Srinivasan, M. Tancik et
                // al."), which then renders across the top of the architecture diagram at
                // figure scale. A claim from outside the ink is a claim on a whole line or on
                // nothing; `in_figure` — the tight 4pt box — stays unconditional.
                if (take || (in_captioned_fig_pt(cx, cy) && !on_split_row(cx, cy))) && !in_caption(cx, cy) {
                    labels.push(mk(clone_span(s)));
                    label_in_table.push(in_table(cx, cy));
                }
            }
            vector::attach_labels(&mut vectors, &labels, &label_in_table);
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
            .filter_map(|(idx, l)| {
                let t = l.text();
                caption_label(&t).and_then(|(f, n)| {
                    // Drop multi-page "Figure N—Continued" markers — re-emitting them would
                    // duplicate the original figure's id and pollute the output with empty
                    // continuation captions.
                    (!inside_table_frame(l.x0, l.x1, l.y)
                        && !is_ref_continuation(idx)
                        && !is_inline_xref(&t)
                        && !caption_is_continued(&t)
                        && !is_dotleader_toc(&lines, idx))
                        .then_some((idx, f, n))
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
        // Parallel to `standalone`: the structured projection for each standalone caption —
        // (id without the `fig-`/`tab-` prefix slug, caption inner html, is_figure).
        let mut standalone_meta: Vec<(String, String, bool)> = Vec::new();
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
                    .map(|(j, im)| {
                        let (_, _, yb, yt) = dibox(im);
                        (j, edge(yb, yt))
                    })
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                let svg_best = vectors.iter().enumerate()
                    .filter(|(j, _)| svg_cap[*j].is_none())
                    .map(|(j, v)| {
                        let (_, _, yb, yt) = dvbox(v);
                        (j, edge(yb, yt))
                    })
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
                    .min_by(|(_, a), (_, b)| (a.bbox.y1 - cy).abs().partial_cmp(&(b.bbox.y1 - cy).abs()).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(j, t)| {
                        let below = cy < (t.bbox.y1 + t.bbox.y0) * 0.5; // caption sits below the table (y up)
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
                let (block, full_id) = if is_fig {
                    (format!("<figure id=\"fig-{nid}\"><figcaption>{html}</figcaption></figure>"), format!("fig-{nid}"))
                } else {
                    (format!("<div id=\"tab-{nid}\">{html}</div>"), format!("tab-{nid}"))
                };
                standalone.push((lines[idx].x0, cy, block));
                standalone_meta.push((full_id, html.clone(), is_fig));
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
            // `!on_split_row` here is the OTHER half of the exactly-once invariant. The figure
            // side refuses a claim on a row it cannot take whole (see `split_rows`); if the body
            // side kept dropping that row as a figure label, the row would be emitted NEITHER
            // place — which is how `econ_EM_2606_02234.pdf` p25 could lose "0.33 0.34 0.50 0.50"
            // entirely. Both sides ask the same question, so exactly one of them keeps it.
            let axis_label = near_fig_label(fig_cx, l.y, l.size, &l.text()) && !line_on_split_row(l);
            // Text sitting INSIDE a CAPTIONED figure's ink box is that diagram's own label
            // ("Image Semantics", "Group 2", "Outputs") — a figure carries a caption, so its
            // interior is figure content, never a section heading. (A real heading sits OUTSIDE
            // the ink box; this is why we use the tight box with no margin, distinct from the
            // generic in_figure path below that deliberately spares a heading overlapping a
            // figure.) Without this, such labels — now visible via Form-XObject text — promote
            // to spurious <h4>s and drag heading precision down.
            // A SHORT line sitting INSIDE a CAPTIONED figure's ink box is that diagram's own
            // label ("Image Semantics", "Group 2", "Outputs", a plot's axis title) — figure
            // content, never a section heading. A figure carries a caption, so its interior is
            // figure text; this holds even when the figure's wall of small labels tripped the
            // in-figure "prose" gate (that gate exists for an UNcaptioned framed certificate,
            // not a captioned diagram). The ≤6-word bound keeps any genuine wrapped sentence
            // that overlaps the box out of this branch. Without it these labels — now visible
            // via Form-XObject text — leak into the body as spurious <h4>s.
            let label_words = l.text().split_whitespace().count();
            let in_captioned_fig = label_words <= 6
                && captioned_fig_boxes
                    .iter()
                    .any(|&(xl, xr, yb, yt)| fig_cx >= xl && fig_cx <= xr && l.y >= yb && l.y <= yt);
            let fig_label = ((in_figure(fig_cx, l.y) || axis_label)
                && !in_prose(fig_cx, l.y)
                && (axis_label || detect_header(l, body, Some(&profile)).is_none()))
                || in_captioned_fig;
            if !consumed_caption.contains(&idx) && !fig_label {
                items.push(Item::L(l));
                boxes.push((l.x0, l.x1.max(l.x0 + 0.1), l.y, l.y + l.size.max(1.0)));
            }
        }
        for (j, t) in tables.iter().enumerate() {
            items.push(Item::T(j));
            boxes.push((t.bbox.x0, t.bbox.x1.max(t.bbox.x0 + 0.1), t.bbox.y0, t.bbox.y1));
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
                let (vxl, vxr, vyb, vyt) = dvbox(v);
                let vr = Rect::new(vxl, vyb, vxr, vyt);
                let varea = vr.area().max(1.0);
                for (ii, im) in images.iter().enumerate() {
                    if vec_owner[vi].is_some() || img_owner[ii].is_some() {
                        continue;
                    }
                    let (ixl, ixr, iyb, iyt) = dibox(im);
                    let ir = Rect::new(ixl, iyb, ixr, iyt);
                    let overlap = vr.overlap_area(ir);
                    let imarea = ir.area().max(1.0);
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
            let (_, _, _, iyt) = dibox(im);
            items.push(Item::Img(j));
            boxes.push((px0, px1, iyt - 1.0, iyt + 1.0)); // full-width separator
        }
        for (j, v) in vectors.iter().enumerate() {
            if vec_owner[j].is_some() {
                continue; // overlaid onto its image, not emitted separately
            }
            let (vxl, vxr, vyb, vyt) = dvbox(v);
            items.push(Item::Svg(j));
            boxes.push((vxl, vxr.max(vxl + 0.1), vyb, vyt));
        }
        for (j, (cx0, cy, _)) in standalone.iter().enumerate() {
            items.push(Item::Cap(j));
            boxes.push((*cx0, *cx0 + 0.1, *cy, *cy + body.max(1.0)));
        }
        let order = text::xy_cut_order(&boxes, body);
        // The non-text items (tables/figures/captions) carry their positioned box onto the
        // emitted [`PageElement`] so the block projection records it; text runs get their box
        // inside `emit_lines` (unioned over the run's lines). `boxes` is `(x_left, x_right,
        // y_bottom, y_top)`; the [`Bbox`] form is `[x0, y0, x1, y1]` = `[x_left, y_bottom,
        // x_right, y_top]`.
        let item_box = |bi: usize| -> Option<Bbox> {
            let (xl, xr, yb, yt) = boxes[bi];
            Some([xl, yb, xr, yt])
        };
        let items: Vec<(&Item, Option<Bbox>)> = order.iter().map(|&i| (&items[i], item_box(i))).collect();

        // Emit, grouping consecutive lines into text blocks. `page_promote` lists the
        // PDF-outline titles whose target page is this one, so body-size section titles
        // still become headings.
        let page_promote: &[(String, u8)] = promote_by_page.get(pno).map(|v| v.as_slice()).unwrap_or(&[]);
        // The accepted tables' rects, for the grid-residue heading guard in `emit_lines`.
        let table_interiors: Vec<(f32, f32, f32, f32)> =
            tables.iter().map(|t| (t.bbox.x0, t.bbox.x1, t.bbox.y0 - body, t.bbox.y1 + body)).collect();
        let prose_ctx = ProseCtx { body, title_sz, promote: page_promote, profile: &profile, plan: &head_plan, table_interiors: &table_interiors };
        let mut run: Vec<&Line> = Vec::new();
        let flush = |run: &mut Vec<&Line>, out: &mut Vec<PageElement>| {
            if !run.is_empty() {
                emit_lines(run, prose_ctx, out);
                run.clear();
            }
        };
        for (it, ibox) in &items {
            let ibox = *ibox;
            match it {
                Item::L(l) => run.push(l),
                Item::T(j) => {
                    flush(&mut run, &mut els);
                    let caption = tab_cap[*j].as_ref().map(|(n, c, b)| (n.clone(), c.clone(), *b));
                    els.push(PageElement::at(ElKind::Table(tables[*j].table.clone().with_caption(caption)), ibox));
                }
                Item::Img(j) => {
                    flush(&mut run, &mut els);
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
                        // A codec placeholder is SVG, not pixels; the figure pastes it in
                        // rather than referencing it (see `vector::Raster::placeholder`).
                        let ph = vector::inline_svg_payload(&images[*j].uri);
                        img_uris.push(std::mem::take(&mut images[*j].uri));
                        let href = format!("\u{0}{idx}\u{0}");
                        let im = &images[*j];
                        let vi = img_overlays[*j][0];
                        let svg = vectors[vi].composite_svg(&[vector::Raster {
                            href: &href,
                            placeholder: ph.as_deref(),
                            rect: (im.x_left, im.x_right, im.y_bottom, im.y_top),
                            ctm: im.ctm,
                            clip: im.clip,
                            seq: &im.seq,
                        }]);
                        // Caption may have attached to the image OR its overlay vector.
                        let cap = img_cap[*j].as_ref().or(svg_cap[vi].as_ref());
                        let (html, id, caption) = match cap {
                            Some((num, cap)) => (
                                format!("<figure id=\"fig-{}\">{svg}<figcaption>{cap}</figcaption></figure>", num_id(num)),
                                num.clone(),
                                Some(cap.clone()),
                            ),
                            None => (format!("<figure>{svg}</figure>"), String::new(), None),
                        };
                        els.push(PageElement::at(ElKind::Figure { html, id, caption, image: None, svg: Some(svg) }, ibox));
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
                    let rect = (im.x_left, im.x_right, im.y_bottom, im.y_top);
                    let overlays: String = img_overlays[*j]
                        .iter()
                        .map(|&vi| {
                            let v = &vectors[vi];
                            v.overlay_svg(&v.overlay_style(rect))
                        })
                        .collect();
                    let rel = if overlays.is_empty() { "" } else { " style=\"position:relative\"" };
                    let svg_field = (!overlays.is_empty()).then(|| overlays.clone());
                    let (html, id, caption) = match &img_cap[*j] {
                        Some((num, cap)) => (
                            format!(
                                "<figure{rel} id=\"fig-{nid}\">{graphic}{overlays}<figcaption>{cap}</figcaption></figure>",
                                nid = num_id(num)
                            ),
                            num.clone(),
                            Some(cap.clone()),
                        ),
                        None => (format!("<figure{rel}>{graphic}{overlays}</figure>"), String::new(), None),
                    };
                    // The figure carries a raster placeholder (`<image N>`/`<img>`): mint the
                    // asset id keyed on the figure number so block projection can name it.
                    let image = (!id.is_empty()).then(|| format!("img/fig_{}.png", num_id(&id)));
                    els.push(PageElement::at(ElKind::Figure { html, id, caption, image, svg: svg_field }, ibox));
                }
                Item::Svg(j) => {
                    flush(&mut run, &mut els);
                    // A vector frame containing raster image(s) (a plot whose data points
                    // are a raster within the axes): composite into ONE `<svg>` with each
                    // raster embedded as an `<image>` in the figure's coordinate space.
                    let svg = if !svg_rasters[*j].is_empty() {
                        // Codec placeholders are SVG, not pixels — read before the URIs are
                        // taken (see `vector::Raster::placeholder`).
                        let phs: Vec<Option<String>> =
                            svg_rasters[*j].iter().map(|&ii| vector::inline_svg_payload(&images[ii].uri)).collect();
                        let hrefs: Vec<String> = svg_rasters[*j]
                            .iter()
                            .map(|&ii| {
                                let idx = img_uris.len();
                                img_uris.push(std::mem::take(&mut images[ii].uri));
                                format!("\u{0}{idx}\u{0}")
                            })
                            .collect();
                        let refs: Vec<vector::Raster<'_>> = svg_rasters[*j]
                            .iter()
                            .zip(&hrefs)
                            .zip(&phs)
                            .map(|((&ii, href), ph)| {
                                let im = &images[ii];
                                vector::Raster {
                                    href: href.as_str(),
                                    placeholder: ph.as_deref(),
                                    rect: (im.x_left, im.x_right, im.y_bottom, im.y_top),
                                    ctm: im.ctm,
                                    clip: im.clip,
                                    seq: &im.seq,
                                }
                            })
                            .collect();
                        vectors[*j].composite_svg(&refs)
                    } else {
                        vectors[*j].svg()
                    };
                    // Caption may have attached to the vector OR to one of its embedded
                    // rasters — use whichever has it so the composite keeps its caption.
                    let cap = svg_cap[*j].as_ref().or_else(|| svg_rasters[*j].iter().find_map(|&ii| img_cap[ii].as_ref()));
                    let (html, id, caption) = match cap {
                        Some((num, cap)) => (
                            format!("<figure id=\"fig-{nid}\">{svg}<figcaption>{cap}</figcaption></figure>", nid = num_id(num)),
                            num.clone(),
                            Some(cap.clone()),
                        ),
                        None => (format!("<figure>{svg}</figure>"), String::new(), None),
                    };
                    els.push(PageElement::at(ElKind::Figure { html, id, caption, image: None, svg: Some(svg) }, ibox));
                }
                Item::Cap(j) => {
                    flush(&mut run, &mut els);
                    let (.., html) = &standalone[*j];
                    let meta = &standalone_meta[*j];
                    els.push(PageElement::at(ElKind::Caption {
                        html: html.clone(),
                        id: meta.0.clone(),
                        text: meta.1.clone(),
                        is_figure: meta.2,
                    }, ibox));
                }
            }
        }
        flush(&mut run, &mut els);
        // Return the page's typed element IR (NOT yet rendered to HTML) plus its image-URI
        // list. The cross-page element transforms run on the assembled `Vec<Vec<PageElement>>`
        // below, then each page is emitted and the bodies merged. Keeping the IR un-rendered
        // here is what lets the cross-page passes operate on the elements, not on a string.
        (*pno, els, img_uris)
        })
        .collect();
    phase("02_render", t);

    // Cross-page element transforms (the single-stream replacement for the legacy HTML-string
    // post-processing passes). They run on the per-page element lists BEFORE emission, so the
    // model's block projection sees the same post-transform elements the HTML is emitted from.
    let t = std::time::Instant::now();
    let mut pages_els: Vec<PageIR> = renders;
    crate::elem_passes::run_cross_page_passes(&mut pages_els, mode);
    if owner_diagnostics_enabled {
        extract::emit_ordered_table_owner_diagnostics(
            pages_els
                .iter()
                .map(|(pno, elements, _)| {
                    let diagnostics: String = elements
                        .iter()
                        .filter_map(|element| {
                            let ElKind::Table(table) = &element.kind else {
                                return None;
                            };
                            let bbox = element.bbox.map(Rect::from)?;
                            Some(extract::table_analysis_owner_diagnostic(
                                *pno,
                                "detected",
                                bbox,
                                table,
                            ))
                        })
                        .collect();
                    (*pno, diagnostics)
                })
                .collect(),
        );
    }
    phase("04_elem_passes", t);
    if let Some(t0) = prof_start {
        eprintln!("[DPDF_PROFILE] {} pages, total {:.1}ms", page_spans.len(), t0.elapsed().as_secs_f64() * 1e3);
    }
    (pages_els, outline)
}

/// Emit + merge the post-transform [`PageIR`] list into the PRE-id, PRE-nav `body` (the full
/// `<!doctype…></html>` document, headings still bare, image sentinels resolved per
/// `inline_images`), returning `(body, global_image_uris)`. Each page's element IR is rendered
/// to its body HTML, framed per `mode` (page mode wraps `<section data-page>`; section mode is
/// bare), its page-local `\0idx\0` sentinels remapped to global indices, and the global URI list
/// accumulated. The final `dedup_ids` string pass (the one not in the element IR — see
/// [`crate::elem_passes::run_residual_string_passes`]) runs over the merged body.
pub(crate) fn emit_and_merge(pages_els: &[PageIR], mode: Mode) -> (String, Vec<String>) {
    let mut out = String::from(DOC_SHELL_HEAD);
    let mut img_uris: Vec<String> = Vec::new();
    for (pno, els, uris) in pages_els {
        let body_inner = emit_page_elements(els);
        let frag = if mode == Mode::Page {
            format!("<section data-page=\"{pno}\" id=\"page-{pno}\">\n{body_inner}\n</section>\n")
        } else {
            body_inner
        };
        append_with_img_offset(&mut out, &frag, img_uris.len());
        img_uris.extend(uris.iter().cloned());
    }
    out.push_str("</body>\n</html>\n");
    let out = crate::elem_passes::run_residual_string_passes(out);
    (out, img_uris)
}

/// The render-pipeline HEAD: the analysis + per-page render + cross-page transforms + emit +
/// merge, producing the PRE-id, PRE-nav `body`, the global image-URI list, and the PDF's own
/// outline. A thin composition of [`render_doc_elements`] (the IR) + [`emit_and_merge`] (the
/// HTML), kept so [`to_html`] and the legacy callers have one entry point.
pub(crate) fn render_doc(
    access: &dyn crate::access::DocumentAccess,
    mode: Mode,
    inline_images: bool,
) -> (String, Vec<String>, Vec<links::OutlineEntry>) {
    let (pages_els, outline) = render_doc_elements(access, mode, inline_images);
    let (body, img_uris) = emit_and_merge(&pages_els, mode);
    (body, img_uris, outline)
}

/// The render-pipeline TAIL, shared by the PDF parse path ([`to_html`]) and the model-only
/// re-render ([`crate::model::render`]): turn the merged, deduped, PRE-id page-mode `body`
/// (the full `<!doctype…></html>` document, headings still bare, images as `\0idx\0` sentinels
/// or already-resolved `<image N>` placeholders) into the final HTML.
///
/// Splitting this out is what makes "renderers are pure functions of the model" hold by
/// construction: the model captures each page's body verbatim (image sentinels already
/// resolved), reconstructs the same merged `body`, and runs THIS function — so a model-only
/// re-render is the identical code path as a fresh parse, only the `body` source differs.
///
/// - `mode`: page mode IDs + TOCs headings in place; section mode regroups into nested
///   `<section id="sec-…">` wrappers (both mint the SAME `sec-…` ids from heading text).
/// - `outline`: the PDF's own `/Outlines`; when non-empty and `include_toc`, its clean TOC
///   replaces the heading-detected `<nav>`.
/// - `img_uris` / `inline_images`: splice the deferred image data URIs / `<image N>` numbers
///   into any remaining `\0idx\0` sentinels (a no-op when the body carries none, e.g. the
///   model path, whose stored body already has resolved `<image N>` placeholders).
pub(crate) fn assemble(
    body: String,
    mode: Mode,
    include_toc: bool,
    outline: &[links::OutlineEntry],
    img_uris: &[String],
    inline_images: bool,
) -> String {
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
        nav_from_outline(result, outline, mode)
    } else {
        result
    };
    // Splice the deferred image URIs / `<image N>` numbers into their sentinels.
    substitute_images(result, img_uris, inline_images)
}

/// The HTML document shell `to_html` (and the model re-render) wrap the page bodies in.
/// Exposed so the model path reconstructs the byte-identical `<head>`/`<style>` prelude.
pub(crate) const DOC_SHELL_HEAD: &str = "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
     <style>\nbody{max-width:48rem;margin:auto;padding:1rem;\
font-family:system-ui,-apple-system,\"Segoe UI\",Roboto,\"Helvetica Neue\",Arial,sans-serif;\
line-height:1.55}\n\
     img,svg{max-width:100%;height:auto}\n\
     table{border-collapse:collapse;margin:1rem 0}\n\
     th,td{border:1px solid #9994;padding:.3rem .6rem;text-align:left}\n</style>\n</head>\n<body>\n";

/// A page-space span in DISPLAY space — the same run of text, where a viewer sees it.
///
/// The anchor moves through [`geom::PageTurn::pt`] and the baseline angle through
/// [`geom::PageTurn::angle`]; `size` and `width` are lengths along/across that baseline, so a
/// rigid quarter turn leaves them alone. On a `/Rotate 90` page the body's `angle = +90°`
/// spans come back upright, which is what puts them back in the reading order
/// ([`crate::layout::lines_of`] keeps only what is upright *in the space it is handed*).
pub(crate) fn turn_span(turn: geom::PageTurn, s: &Span) -> Span {
    let (x, y) = turn.pt(s.x, s.y);
    Span { x, y, angle: turn.angle(s.angle), ..clone_span(s) }
}

/// A page's ruling in DISPLAY space. Each rule is a degenerate rectangle, so it turns through
/// [`geom::PageTurn::rect`] exactly like a table or figure box does — and on a quarter-turned
/// page a horizontal rule becomes a vertical one, which is why the two lists swap rather than
/// each being mapped in place. Upright pages take the untouched original.
fn turn_rules(turn: geom::PageTurn, r: vector::PageRules) -> vector::PageRules {
    if turn.is_identity() {
        return r;
    }
    let mut out = vector::PageRules::default();
    let mut place = |x0: f32, y0: f32, x1: f32, y1: f32| {
        let (dx0, dx1, dy0, dy1) = turn.rect(x0, x1, y0, y1);
        if dx1 - dx0 >= dy1 - dy0 {
            out.h.push((dx0, dx1, (dy0 + dy1) * 0.5));
        } else {
            out.v.push(((dx0 + dx1) * 0.5, dy0, dy1));
        }
    };
    for &(x0, x1, y) in &r.h {
        place(x0, y, x1, y);
    }
    for &(x, y0, y1) in &r.v {
        place(x, y0, x, y1);
    }
    out
}

pub(crate) fn clone_span(s: &Span) -> Span {
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
        mcid: s.mcid,
        source: s.source,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{CellAnalysis, TableCellRole, TableEvidence};

    fn line(text: &str, y: f32) -> Line {
        Line {
            y,
            x0: 72.0,
            x1: 300.0,
            size: 10.0,
            mono: false,
            mono_w: 0,
            tot_w: text.chars().count(),
            runs: vec![Run { text: text.to_string(), bold: false, italic: false, href: None, script: 0 }],
            font: 1,
        }
    }

    fn claimed_table(claim: SourceSlice) -> crate::table::PositionedTableAnalysis {
        crate::table::PositionedTableAnalysis::from_parts(
            Rect::new(0.0, 0.0, 100.0, 100.0),
            Vec::new(),
            vec![vec!["owned".to_string()]],
            0,
            Vec::new(),
        )
        .with_ownership(
            crate::table::CandidateKey::synthetic(),
            crate::table::TableClaim::from_rows(vec![vec![claim]]),
        )
    }

    #[test]
    fn exact_table_claim_splitting_is_unicode_safe_and_follows_the_baseline() {
        let source = SourceSlice::test_occurrence(7, 4);
        let span = Span {
            x: 10.0,
            y: 20.0,
            size: 10.0,
            width: 40.0,
            text: "Aé中Z".to_string(),
            bold: false,
            italic: false,
            mono: false,
            angle: std::f32::consts::FRAC_PI_2,
            font: 1,
            mcid: None,
            source,
        };
        let tables = [claimed_table(source.sub_slice(1, 3))];
        let claims = TableClaimIndex::new(&tables);
        let mut pieces = Vec::new();
        push_unclaimed_fragments(&span, &claims, &mut pieces);
        assert_eq!(pieces.iter().map(|piece| piece.text.as_str()).collect::<Vec<_>>(), vec!["A", "Z"]);
        assert!((pieces[1].x - 10.0).abs() < 0.001);
        assert!((pieces[1].y - 50.0).abs() < 0.001);
        assert_eq!((pieces[1].source.char_start(), pieces[1].source.char_end()), (3, 4));
    }

    #[test]
    fn table_claim_index_handles_empty_whole_overlapping_and_sub_sliced_sources() {
        let source = SourceSlice::test_occurrence(19, 5);
        let span = Span {
            x: 10.0,
            y: 20.0,
            size: 10.0,
            width: 50.0,
            text: "Aé中Z!".to_string(),
            bold: true,
            italic: false,
            mono: false,
            angle: 0.0,
            font: 4,
            mcid: Some(8),
            source,
        };

        let empty = TableClaimIndex::new(&[]);
        let unchanged = prose_spans_without_table_claims(std::slice::from_ref(&span), &empty);
        assert_eq!(unchanged.len(), 1);
        assert_eq!(unchanged[0].text, span.text);
        assert_eq!((unchanged[0].x, unchanged[0].y, unchanged[0].width), (span.x, span.y, span.width));
        assert_eq!(unchanged[0].source, span.source);

        let whole = TableClaimIndex::new(&[claimed_table(source)]);
        assert!(prose_spans_without_table_claims(std::slice::from_ref(&span), &whole).is_empty());

        let overlapping = TableClaimIndex::new(&[
            claimed_table(source.sub_slice(1, 3)),
            claimed_table(source.sub_slice(2, 4)),
        ]);
        let survivors = prose_spans_without_table_claims(std::slice::from_ref(&span), &overlapping);
        assert_eq!(survivors.iter().map(|piece| piece.text.as_str()).collect::<Vec<_>>(), vec!["A", "!"]);
        assert_eq!(
            survivors.iter().map(|piece| (piece.source.char_start(), piece.source.char_end())).collect::<Vec<_>>(),
            vec![(0, 1), (4, 5)]
        );

        let sub_source = source.sub_slice(2, 5);
        let sub_span = Span { x: 30.0, width: 30.0, text: "中Z!".to_string(), source: sub_source, ..clone_span(&span) };
        let sub_claim = TableClaimIndex::new(&[claimed_table(source.sub_slice(2, 4))]);
        let sub_survivors = prose_spans_without_table_claims(&[sub_span], &sub_claim);
        assert_eq!(sub_survivors.len(), 1);
        assert_eq!(sub_survivors[0].text, "!");
        assert_eq!((sub_survivors[0].source.char_start(), sub_survivors[0].source.char_end()), (4, 5));
        assert!((sub_survivors[0].x - 50.0).abs() < 0.001);
    }

    #[test]
    fn prose_below_a_table_survives_when_the_table_does_not_claim_it() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/three_column_prose.pdf");
        let raw = std::fs::read(path).expect("fixture bytes");
        let doc = Document::load(path).expect("fixture loads");
        let html = to_html(
            &crate::access::test_adapter_with_source(&doc, &raw),
            Mode::Page,
            false,
            false,
        );
        assert_eq!(html.matches("for this reach is incomplete before").count(), 3, "all three columns keep the disjoint line below the middle-column table");
        assert_eq!(html.matches("<table").count(), 1, "the real table is unchanged");
        for cell in ["Zone", "Depth", "18.2", "42.5", "128"] {
            assert_eq!(html.matches(cell).count(), 1, "{cell:?} is emitted exactly once");
        }
    }

    #[test]
    fn running_logo_rules_do_not_consume_neighboring_tables() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/logo_rule_chain.pdf");
        let raw = std::fs::read(path).expect("fixture bytes");
        let doc = Document::load(path).expect("fixture loads");
        let html = to_html(
            &crate::access::test_adapter_with_source(&doc, &raw),
            Mode::Page,
            false,
            false,
        );
        assert_eq!(html.matches("<figure").count(), 0);
        assert_eq!(html.matches("<table").count(), 6);
    }

    /// `tests/fixtures_pdf/form_grid_prose.pdf` (see `gen_form_grid_prose`): five bold, short,
    /// heading-faced lines that are NOT headings — a mid-clause URL sentence, a table caption,
    /// a grid sub-label in the band above a table's top rule, a dot-leader form line and a
    /// bullet — beside two REAL headings in the same face. Before this, the URL line was the
    /// document `<h1>` and the other four were `<h2>`/`<h4>` sections. Every trap must keep
    /// its text: these rules change classification, never emission.
    #[test]
    fn heading_faced_form_and_grid_lines_are_not_headings() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/form_grid_prose.pdf");
        let raw = std::fs::read(path).expect("fixture bytes");
        let doc = Document::load(path).expect("fixture loads");
        let html = to_html(
            &crate::access::test_adapter_with_source(&doc, &raw),
            Mode::Page,
            false,
            false,
        );
        let headings: Vec<&str> = html
            .match_indices("<h")
            .filter(|(i, _)| html[i + 2..].starts_with(|c: char| c.is_ascii_digit()))
            .filter_map(|(i, _)| {
                let open = i + html[i..].find('>')?;
                let close = open + html[open..].find("</h")?;
                Some(&html[open + 1..close])
            })
            .collect();
        assert!(
            headings.iter().any(|h| h.contains("Program Notes")),
            "the real title is gone: {headings:?}"
        );
        assert!(
            headings.iter().any(|h| h.contains("Data Sources")),
            "the real heading in the same bold face is gone: {headings:?}"
        );
        let h1 = html.find("<h1").map(|i| {
            let open = i + html[i..].find('>').unwrap_or(0);
            let close = open + html[open..].find("</h1>").unwrap_or(0);
            html[open + 1..close].to_string()
        });
        assert_eq!(
            h1.as_deref().map(str::trim),
            Some("Program Notes"),
            "the document <h1> must fall through the URL line and the caption to the first \
             real heading"
        );
        for trap in [
            "Registry Entries Have Been Published At",
            "Table 3: District totals",
            "Section B Adjustments",
            "11 Educator expenses",
            "Mineral and energy resources",
        ] {
            assert!(
                !headings.iter().any(|h| h.contains(trap)),
                "{trap:?} promoted to a heading: {headings:?}"
            );
            assert!(html.contains(trap), "{trap:?} lost from the output entirely");
        }
        assert_eq!(html.matches("<table").count(), 1, "the real grid is unchanged");
    }

    /// Emit `texts` as one run of body-size lines with `promote` as the page's outline
    /// entries, and report the `<hN>` tag of every heading emitted.
    fn heading_tags(promote: &[(String, u8)], texts: &[&str]) -> Vec<u8> {
        let lines: Vec<Line> = texts.iter().enumerate().map(|(i, t)| line(t, 700.0 - 20.0 * i as f32)).collect();
        let refs: Vec<&Line> = lines.iter().collect();
        let mut out: Vec<PageElement> = Vec::new();
        let (profile, plan) = (DocProfile::default(), HeadingPlan::default());
        emit_lines(&refs, ProseCtx { body: 10.0, title_sz: 10.0, promote, profile: &profile, plan: &plan, table_interiors: &[] }, &mut out);
        out.iter()
            .filter_map(|e| match &e.kind {
                ElKind::Heading { level, .. } => Some(*level),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn outline_promoted_lines_keep_the_authors_nesting_depth() {
        // Every outline-matched line used to be forced to logical level 1 (<h2>), which
        // flattened a nested bookmark tree and cost header-level accuracy. The 0-based
        // outline depth maps to logical depth+1, i.e. <h(depth+2)>.
        let promote: Vec<(String, u8)> = vec![
            (title_key("Methods"), 0),
            (title_key("Data collection"), 1),
            (title_key("Instrument calibration"), 2),
            (title_key("Very deep note"), 9),
        ];
        let tags = heading_tags(&promote, &["Methods", "Data collection", "Instrument calibration", "Very deep note"]);
        // depth 0 → <h2> (section), 1 → <h3>, 2 → <h4>; depth is clamped so the tag never
        // exceeds <h6>.
        assert_eq!(tags, vec![2, 3, 4, 6]);
    }

    #[test]
    fn a_line_absent_from_the_outline_is_not_promoted() {
        // Body-size prose with no outline entry stays a paragraph — the promotion is
        // page-scoped to the author's own bookmarks.
        let promote: Vec<(String, u8)> = vec![(title_key("Methods"), 1)];
        assert!(heading_tags(&promote, &["Some ordinary body sentence here."]).is_empty());
        // …and a match longer than the 12-word cap is still rejected.
        let long = "One two three four five six seven eight nine ten eleven twelve thirteen";
        let promote_long: Vec<(String, u8)> = vec![(title_key(long), 1)];
        assert!(heading_tags(&promote_long, &[long]).is_empty());
    }

    #[test]
    fn semantic_header_depth_is_independent_of_detached_row_storage() {
        // The visible table parts and their colspans are one axis; which leading rows are
        // semantic headers is another. This is the contract G7 needs in order to correct
        // over-attached rows without deleting, moving, or flattening any cell.
        let header = vec![
            vec![("All columns".to_string(), 2)],
            vec![("A".to_string(), 1), ("B".to_string(), 1)],
        ];
        let grid = vec![vec!["1".to_string(), "2".to_string()]];
        let render = |header_rows| {
            table_html(&TableAnalysis::from_parts(
                header.clone(), grid.clone(), header_rows, None, Vec::new(),
            ))
        };

        let none = render(0);
        assert_eq!(none.matches("<th").count(), 0);
        assert_eq!(none.matches("<td").count(), 5);
        assert!(none.contains("<td colspan=\"2\">All columns</td>"));

        let two = render(2);
        assert_eq!(two.matches("<th").count(), 3);
        assert_eq!(two.matches("<td").count(), 2);
        assert!(two.contains(
            "<th scope=\"colgroup\" colspan=\"2\">All columns</th>"
        ));

        let through_grid = render(3);
        assert_eq!(through_grid.matches("<th").count(), 5);
        assert_eq!(through_grid.matches("<td").count(), 0);
    }

    #[test]
    fn canonical_table_cells_emit_column_scope_and_materialized_spans() {
        let mut group = CellAnalysis::declared(
            "Group".into(), 0, 0, 1, 2, TableCellRole::Header, None,
        );
        group.role = TableCellRole::Header;
        let mut solo = CellAnalysis::declared(
            "Solo".into(), 0, 2, 1, 1, TableCellRole::Header, None,
        );
        solo.role = TableCellRole::Header;
        let grid = vec![
            vec![group, CellAnalysis::covered(0, 1), solo],
            vec![
                CellAnalysis::declared(
                    "North".into(), 1, 0, 2, 1, TableCellRole::Data, None,
                ),
                CellAnalysis::new("10".into(), 1, 1, 1, 1, None),
                CellAnalysis::new("20".into(), 1, 2, 1, 1, None),
            ],
            vec![
                CellAnalysis::covered(2, 0),
                CellAnalysis::new("11".into(), 2, 1, 1, 1, None),
                CellAnalysis::new("21".into(), 2, 2, 1, 1, None),
            ],
        ];
        let html = table_html(&TableAnalysis::from_cells(
            Vec::new(), grid, 1, vec![TableEvidence::Declared],
        ));

        assert_eq!(
            html,
            "<table data-dpdf-semantic-spans><tr><th scope=\"colgroup\" colspan=\"2\">Group</th>\
             <th scope=\"col\">Solo</th></tr><tr><td rowspan=\"2\">North</td>\
             <td>10</td><td>20</td></tr><tr><td>11</td><td>21</td></tr></table>"
        );
    }

    /// `tests/gen_fixtures.py::gen_rotated_body` — the same displayed page at `/Rotate`
    /// 0/90/180/270: a heading, two paragraphs, and a spine label set sideways to the reader.
    /// No figure, no raster, no table — text is the only thing on it.
    #[test]
    fn a_turned_page_reads_its_body_the_way_the_reader_sees_it() {
        // THE defect: `lines_of` dropped every span with a non-zero angle, which is EVERY
        // span on a quarter-turned page — so such a page emitted no prose at all. It hid
        // behind the one corpus document that turns pages, whose turned pages are a full-page
        // ruled table: the table's vector figure carried the words as SVG labels, so the loss
        // never showed up as an empty page.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/rotated_body.pdf");
        let raw = std::fs::read(path).expect("rotated_body.pdf fixture must exist");
        let doc = Document::load(path).expect("rotated_body.pdf fixture must load");
        let html = to_html(&crate::access::test_adapter_with_source(&doc, &raw), Mode::Page, false, false);
        let pages: Vec<&str> = html.split("<section data-page=").skip(1).collect();
        assert_eq!(pages.len(), 4, "fixture has one page per rotation");
        let want = [
            "A page that carries a Rotate key is turned clockwise before it is shown, and its text is authored \
             at the matching angle so that the reader sees ordinary upright prose on an ordinary page.",
            "Nothing about that page is a figure, so nothing else can carry its words into the output.",
        ];
        for (i, page) in pages.iter().enumerate() {
            let rot = [0, 90, 180, 270][i];
            assert!(page.contains("Turning the page"), "/Rotate {rot}: the heading is missing");
            for w in want {
                assert!(page.contains(w), "/Rotate {rot}: paragraph missing from\n{page}");
            }
            // Reading order, not merely presence: the second paragraph follows the first.
            let (a, b) = (page.find(want[0]), page.find(want[1]));
            assert!(a < b, "/Rotate {rot}: paragraphs out of reading order");
        }
        // The spine label is upright in PAGE space on the turned pages and sideways to the
        // READER — which is precisely what the "rotated text is not body text" rule is for.
        // The rule is kept; what changed is the space it is asked in.
        assert!(!html.contains("SPINE"), "a label set sideways to the reader is not body prose");
    }
}
