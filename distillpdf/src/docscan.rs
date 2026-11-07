//! The per-document typography analysis, folded one page at a time.
//!
//! Everything here is a small accumulator keyed by a font style, never by a page's content, so
//! feeding a document through it holds nothing per page. [`crate::html`] folds materialized spans
//! through it and [`crate::pages`] folds each page's spans and drops them; both get the same
//! answer because there is one implementation.

use std::collections::BTreeMap;

use crate::profile::{DocProfile, HeadingTier};
use crate::text::Span;

/// Distinct `(size, bold, font)` clusters and body-size buckets kept. Real documents have a few
/// dozen; a hostile content stream can mint one per glyph. Past the cap new styles are not
/// tracked, which is the honest degradation: they are by construction rare in that document.
const MAX_STYLE_CLUSTERS: usize = 4096;

#[derive(Default)]
struct Acc {
    chars: usize,
    spans: usize,
    caption: usize,
    pages: usize,
    last_page: Option<u32>,
}

/// Folds spans into the document's body size, title size and [`DocProfile`].
#[derive(Default)]
pub(crate) struct TypographyScan {
    // BTreeMap: `max_by_key` below returns the LAST maximum in iteration order, so with a
    // `HashMap` two equally-common body sizes picked a different winner run to run — and
    // the body size drives every heading/paragraph decision downstream. Ascending key order
    // makes the tie-break "the larger size wins", deterministically.
    hist: BTreeMap<i32, usize>,
    // BTreeMap, not HashMap: three consumers below read this map IN ITERATION ORDER —
    // `max_by_key` for the body cluster and the title font (both return the LAST maximum),
    // and the `cands` collect whose later `sort_by` is STABLE, so equal-ranking heading
    // tiers keep iteration order and `take(4)` picks by it. Ordered by (size, bold, font),
    // ties resolve to the largest size / bold / highest font id, deterministically.
    clusters: BTreeMap<(i32, bool, u32), Acc>,
    numbered_hits: usize,
    /// The largest span size on the document's first page, once that page has been seen.
    first_page_max: Option<f32>,
}

impl TypographyScan {
    /// Fold one page. Pages must arrive in ascending page order.
    pub(crate) fn add_page(&mut self, pno: u32, spans: &[Span]) {
        if self.first_page_max.is_none() {
            self.first_page_max = Some(spans.iter().map(|s| s.size).fold(f32::NEG_INFINITY, f32::max));
        }
        for s in spans {
            if s.angle.abs() >= 0.01 {
                continue; // body size and clusters ignore rotated labels
            }
            let size = s.size.round() as i32;
            if self.hist.len() < MAX_STYLE_CLUSTERS || self.hist.contains_key(&size) {
                *self.hist.entry(size).or_insert(0) += 1;
            }
            let key = (size, s.bold, s.font);
            if self.clusters.len() >= MAX_STYLE_CLUSTERS && !self.clusters.contains_key(&key) {
                continue;
            }
            let e = self.clusters.entry(key).or_default();
            e.chars += s.text.chars().count();
            e.spans += 1;
            if e.last_page != Some(pno) {
                e.last_page = Some(pno);
                e.pages += 1;
            }
            let lo = s.text.trim_start().to_lowercase();
            if lo.starts_with("figure") || lo.starts_with("table") || lo.starts_with("fig.") || lo.starts_with("tab.") {
                e.caption += 1;
            }
            // A multi-component number token ("3.2") is a strong numbered-section signal.
            let t = s.text.trim();
            if t.len() >= 3 && t.contains('.') && t.bytes().next().is_some_and(|b| b.is_ascii_digit()) && t.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                self.numbered_hits += 1;
            }
        }
    }

    /// The document's body size, title size and style profile: the body/heading
    /// size·weight·font tiers ranked to levels, and whether sections are numbered.
    pub(crate) fn finish(self, pages: usize, has_outline: bool) -> (f32, f32, DocProfile) {
        let body = self.hist.iter().max_by_key(|(_, c)| **c).map(|(s, _)| *s as f32).unwrap_or(10.0);
        // The document title is the largest text on the FIRST page; reserve <h1> for it.
        // (Scoped to page 1 — a large figure label or display equation on a later page
        // must not inflate the threshold and demote the real title to <h2>.)
        let title_sz = self.first_page_max.map_or(body, |max| max.max(body));
        let clusters = &self.clusters;
        let body_i = body.round() as i32;
        let pages = pages.max(1);
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
                if pages >= 4 && a.pages as f32 > pages as f32 * 0.9 {
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
        let profile = DocProfile {
            body_size: body_key.0 as f32,
            body_font,
            body_bold: body_key.1,
            title_sz,
            tiers,
            numbered: self.numbered_hits >= 2,
            has_outline,
        };
        (body, title_sz, profile)
    }
}
