//! Per-document page-chrome detection: running headers, footers and page numbers.
//!
//! The signal is **repetition**: chrome sits at the same y-row of the page's top or
//! bottom band on most pages, with the same text up to its counters ("Page 12 of 340"
//! and "Page 13 of 340" are one string once digit runs are masked). Body text never
//! looks like that — the first body line's y-row is occupied on half the pages of a
//! dense report, but by a *different* sentence every time, so a text-diversity guard
//! separates the two cleanly (measured 0.07–0.15 for chrome rows vs 0.99 for the top
//! body row on the report that drove this design).
//!
//! Both conditions are required — **position ∧ text** — and the outcome is a *ban list*
//! of specific rows, never a derived margin:
//!   * text alone would delete legitimate reuse (the same report repeats its running
//!     header mid-page as divider-page titles);
//!   * position alone would eat whatever body line happens to sit at a common y;
//!   * a margin would swallow title-page front matter and real footnotes wholesale.
//!
//! Detection runs once over the whole document's spans (the render pipeline already
//! materializes them all before any page renders); filtering happens at the **span**
//! level, before table detection, footnote masking and paragraph assembly — the stages
//! that were previously absorbing chrome into `<td>`s, `<aside>`s and glued paragraphs.

use std::collections::{HashMap, HashSet};

use crate::geom::PageTurn;
use crate::text::Span;
use lopdf::ObjectId;

/// Fraction of the display page box's height treated as the top and bottom chrome bands.
const BAND_FRAC: f32 = 0.12;
/// A chrome row must appear on at least this many pages…
const MIN_PAGES: usize = 4;
/// …and on at least this fraction of the document's pages…
const MIN_COVERAGE: f32 = 0.5;
/// …with at most this many distinct masked texts per covered page. Chrome repeats
/// (ratio near 1/pages); body text at a shared y is different every time (ratio near 1).
const MAX_DIVERSITY: f32 = 0.35;
/// A chrome row is ISOLATED: the outermost text row of its page, separated from the rest
/// of the text by at least this many times its own font size. Line spacing runs
/// ~1.15–1.4×, chrome gutters ~2×+ — this is what keeps the edge rows of a full-page or
/// page-spanning table (counter-varying data at a constant y, masked-identical across
/// pages) from reading as chrome: they sit at line spacing from the next row.
const MIN_GAP_FACTOR: f32 = 1.8;
/// Row quantization: spans within the same 2 pt y-band are one row.
const ROW_QUANT: f32 = 2.0;

#[derive(Default)]
struct ChromeRow {
    /// Whole joined keys that recur at this row.
    whole: HashSet<String>,
    /// Recurring horizontal chunks within an otherwise variable joined key.
    chunks: HashSet<String>,
}

impl ChromeRow {
    fn matches(&self, dspans: &[Span], members: &[usize]) -> bool {
        self.whole.contains(&row_key(dspans, members))
            || row_chunks(dspans, members).iter().any(|key| self.chunks.contains(key))
    }
}

/// One detected chrome row: a y-bucket in the top or bottom band plus recurring whole
/// texts or horizontal members. A span row is dropped only when position and one of those
/// recurrence keys both match.
#[derive(Default)]
pub(crate) struct ChromePlan {
    rows: HashMap<(bool, i32), ChromeRow>,
    /// The masked header texts (top-band rows), for [`crate::profile::DocProfile`]'s
    /// `running_heads` and any consumer that wants the strings rather than the geometry.
    pub(crate) running_heads: HashSet<String>,
}

impl ChromePlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Which of `dspans` (display space) are this plan's chrome rows: `Some(mask)` with
    /// `true` at every span to drop, or `None` when nothing on this page matched (the
    /// caller then keeps its borrows — no allocation on chrome-free pages).
    ///
    /// A MASK rather than a filtered copy because the caller holds the same spans in TWO
    /// positionally-corresponding representations (page space and display space) and must
    /// filter both with the same indices — the figure-label pass zips them.
    pub(crate) fn drop_mask(&self, dspans: &[Span], dbox: (f32, f32, f32, f32)) -> Option<Vec<bool>> {
        if self.rows.is_empty() {
            return None;
        }
        let mut drop: Vec<bool> = vec![false; dspans.len()];
        for (top, bucket, members) in band_rows(dspans, dbox) {
            // ±1 bucket of jitter tolerance: quantization must not split a row whose
            // baseline drifts across a 2 pt boundary between pages.
            let banned = (bucket - 1..=bucket + 1)
                .any(|q| self.rows.get(&(top, q)).is_some_and(|row| row.matches(dspans, &members)));
            if banned {
                for i in members {
                    drop[i] = true;
                }
            }
        }
        drop.iter().any(|d| *d).then_some(drop)
    }
}

/// Detect the document's chrome rows from the materialized per-page spans.
///
/// Bands are fractions of the **display page box** (`/MediaBox` through the page's
/// `/Rotate`), not of the used text extent — a page whose body stops short must not pull
/// the band down into it.
pub(crate) fn plan_chrome(
    access: &dyn crate::access::DocumentAccess,
    page_spans: &[(u32, ObjectId, Vec<Span>)],
    body: f32,
) -> ChromePlan {
    let n_pages = page_spans.len();
    if n_pages < MIN_PAGES {
        return ChromePlan::default();
    }
    #[derive(Default)]
    struct SeenRow {
        whole: HashMap<String, HashSet<u32>>,
        chunks: HashMap<String, HashSet<u32>>,
    }
    // (band, bucket) → whole/chunk recurrence keys → distinct pages seen on.
    let mut seen: HashMap<(bool, i32), SeenRow> = HashMap::new();
    for (pno, pid, spans) in page_spans {
        let page_box = crate::pdfobj::page_box(access, *pid).unwrap_or([
            0.0,
            0.0,
            crate::pdfobj::DEFAULT_PAGE_PTS.0,
            crate::pdfobj::DEFAULT_PAGE_PTS.1,
        ]);
        let turn = PageTurn::new(crate::pdfobj::page_rotation(access, *pid), page_box);
        let dbox = turn.rect(page_box[0], page_box[2], page_box[1], page_box[3]);
        let turned: Option<Vec<Span>> =
            (!turn.is_identity()).then(|| spans.iter().map(|s| crate::html::turn_span(turn, s)).collect());
        let dspans: &[Span] = turned.as_deref().unwrap_or(spans.as_slice());
        for (top, bucket, members) in band_rows(dspans, dbox) {
            // Chrome is set at (or below) body size; a bigger-than-body row is heading
            // material — the "Section N: …" / "Appendix A-N" page-top titles a document
            // may legitimately repeat, counter and all.
            let row_size = members.iter().map(|&i| dspans[i].size).fold(0.0f32, f32::max);
            if row_size > body * 1.15 {
                continue;
            }
            if !isolated(dspans, top, &members) {
                continue; // at line spacing from the body / a table: content, not chrome
            }
            let row = seen.entry((top, bucket)).or_default();
            row.whole.entry(row_key(dspans, &members)).or_default().insert(*pno);
            for chunk in row_chunks(dspans, &members) {
                row.chunks.entry(chunk).or_default().insert(*pno);
            }
        }
    }
    let mut plan = ChromePlan::default();
    for ((top, bucket), seen_row) in seen {
        let covered: HashSet<&u32> = seen_row.whole.values().flatten().collect();
        let pages = covered.len();
        if pages < MIN_PAGES || (pages as f32) < MIN_COVERAGE * n_pages as f32 {
            continue;
        }
        if seen_row.whole.len() as f32 > MAX_DIVERSITY * pages as f32 {
            continue; // a busy body row, not chrome: different text every page
        }
        // Whole keys keep the original precision rule. A recurring horizontal chunk (for
        // example "Page # of #") also proves a variable compound row whose section-title
        // prefix appears only once. Chunk recurrence uses the document chrome floor rather
        // than the two-page whole-key floor so ordinary repeated words cannot qualify it.
        let whole: HashSet<String> = seen_row
            .whole
            .into_iter()
            .filter(|(_, pages)| pages.len() >= 2)
            .map(|(t, _)| t)
            .collect();
        let chunks: HashSet<String> = seen_row
            .chunks
            .into_iter()
            .filter(|(_, pages)| pages.len() >= MIN_PAGES)
            .map(|(t, _)| t)
            .collect();
        if whole.is_empty() && chunks.is_empty() {
            continue;
        }
        if top {
            for value in &whole {
                plan.running_heads.insert(String::from(value.as_str()));
            }
        }
        plan.rows.insert((top, bucket), ChromeRow { whole, chunks });
    }
    plan
}

/// Is this band row the OUTERMOST text row of its page, with a chrome-sized gutter
/// ([`MIN_GAP_FACTOR`] × its font size) to the nearest other text row inward? Both are
/// per-page-instance conditions and gate only DETECTION — filtering drops every matching
/// (row ∧ text) instance, so a footer still goes on the one page where a footnote sits
/// close above it.
fn isolated(dspans: &[Span], top: bool, members: &[usize]) -> bool {
    let ys: Vec<f32> = members.iter().map(|&i| dspans[i].y).collect();
    let (row_min, row_max) = ys.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &y| (lo.min(y), hi.max(y)));
    let row_size = members.iter().map(|&i| dspans[i].size).fold(0.0f32, f32::max);
    let mut nearest_inward: Option<f32> = None;
    for (i, s) in dspans.iter().enumerate() {
        if members.contains(&i) || s.angle.abs() >= 0.01 || s.text.trim().is_empty() {
            continue;
        }
        if top {
            if s.y > row_max + ROW_QUANT {
                return false; // text above a "header" row: not the outermost row
            }
            if s.y < row_min - 0.5 {
                let gap = row_min - s.y;
                nearest_inward = Some(nearest_inward.map_or(gap, |g: f32| g.min(gap)));
            }
        } else {
            if s.y < row_min - ROW_QUANT {
                return false; // text below a "footer" row: not the outermost row
            }
            if s.y > row_max + 0.5 {
                let gap = s.y - row_max;
                nearest_inward = Some(nearest_inward.map_or(gap, |g: f32| g.min(gap)));
            }
        }
    }
    // Chrome DECORATES a page's content; a row that is the page's only text (nothing
    // inward of it) is that page's content, and banning it would empty the page.
    nearest_inward.is_some_and(|gap| gap >= MIN_GAP_FACTOR * row_size.max(6.0))
}

/// The rows of `dspans` that sit in the page's top/bottom chrome bands:
/// `(is_top_band, y_bucket, member span indices)` — upright spans only, bucketed by
/// [`ROW_QUANT`], members in x order.
fn band_rows(dspans: &[Span], dbox: (f32, f32, f32, f32)) -> Vec<(bool, i32, Vec<usize>)> {
    let (_, _, y0, y1) = dbox;
    let band = (y1 - y0).max(1.0) * BAND_FRAC;
    let mut rows: HashMap<(bool, i32), Vec<usize>> = HashMap::new();
    for (i, s) in dspans.iter().enumerate() {
        if s.angle.abs() >= 0.01 || s.text.trim().is_empty() {
            continue;
        }
        let top = if s.y >= y1 - band {
            true
        } else if s.y <= y0 + band {
            false
        } else {
            continue;
        };
        rows.entry((top, (s.y / ROW_QUANT).round() as i32)).or_default().push(i);
    }
    let mut out: Vec<(bool, i32, Vec<usize>)> = rows
        .into_iter()
        .map(|((top, q), mut members)| {
            members.sort_by(|&a, &b| dspans[a].x.total_cmp(&dspans[b].x));
            (top, q, members)
        })
        .collect();
    out.sort_by_key(|(top, q, _)| (*top, *q));
    out
}

/// A row's recurrence key: member texts joined in x order, lowercased, whitespace
/// collapsed, every digit RUN masked to one `#` — so "Page 12 of 340" and
/// "Page 9 of 340" (and "5 Geology 47 of 172" / "6 Reservoir 58 of 172") coincide.
fn normalized_key(joined: &str) -> String {
    let mut out = String::with_capacity(joined.len());
    let mut in_digits = false;
    let mut in_space = false;
    for c in joined.trim().chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                if in_space && !out.is_empty() {
                    out.push(' ');
                }
                out.push('#');
                in_digits = true;
            }
            in_space = false;
        } else if c.is_whitespace() {
            in_digits = false;
            in_space = true;
        } else {
            if in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = false;
            in_digits = false;
            for lc in c.to_lowercase() {
                out.push(lc);
            }
        }
    }
    out
}

fn row_key(dspans: &[Span], members: &[usize]) -> String {
    let joined = members.iter().map(|&i| dspans[i].text.as_str()).collect::<Vec<_>>().join(" ");
    normalized_key(&joined)
}

/// Split one row at authored horizontal gaps, preserving word-level spans inside each
/// component. This exposes a stable page-number tail without treating generic individual
/// words or digit glyphs as recurrence evidence.
fn row_chunks(dspans: &[Span], members: &[usize]) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut previous_end: Option<f32> = None;
    let mut previous_size = 0.0f32;
    for &i in members {
        let span = &dspans[i];
        let width = if span.width > 0.1 {
            span.width
        } else {
            span.text.chars().count() as f32 * span.size * 0.5
        };
        let separated = previous_end.is_some_and(|end| {
            span.x - end > span.size.max(previous_size).max(1.0) * 1.5
        });
        if separated && !current.is_empty() {
            chunks.push(normalized_key(&current));
            current.clear();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(span.text.trim());
        previous_end = Some(span.x + width);
        previous_size = span.size;
    }
    if !current.is_empty() {
        chunks.push(normalized_key(&current));
    }
    chunks.retain(|key| !key.is_empty());
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_runs_mask_to_one_hash_so_counters_coincide() {
        let spans = |texts: &[&str]| -> Vec<Span> {
            texts
                .iter()
                .enumerate()
                .map(|(i, t)| Span {
                    x: i as f32 * 50.0,
                    y: 770.0,
                    size: 9.0,
                    width: 40.0,
                    text: t.to_string(),
                    bold: false,
                    italic: false,
                    mono: false,
                    angle: 0.0,
                    font: 0,
                    mcid: None,
                    source: crate::text::SourceSlice::test_occurrence(0, 0),
                })
                .collect()
        };
        let a = spans(&["Page 12 of 340"]);
        let b = spans(&["Page", "9", "of", "340"]);
        assert_eq!(row_key(&a, &[0]), "page # of #");
        assert_eq!(row_key(&b, &[0, 1, 2, 3]), "page # of #");
    }

    #[test]
    fn recurring_page_tail_matches_a_unique_compound_footer() {
        let values = [
            (72.0, 70.0, "Single Page Appendix"),
            (430.0, 22.0, "Page"),
            (455.0, 10.0, "12"),
            (468.0, 8.0, "of"),
            (479.0, 10.0, "12"),
        ];
        let spans: Vec<Span> = values
            .iter()
            .enumerate()
            .map(|(i, (x, width, value))| Span {
                x: *x,
                y: 50.0,
                size: 8.0,
                width: *width,
                text: (*value).into(),
                bold: false,
                italic: false,
                mono: false,
                angle: 0.0,
                font: 0,
                mcid: None,
                source: crate::text::SourceSlice::test_occurrence(i as u32, value.len()),
            })
            .collect();
        let members = [0, 1, 2, 3, 4];
        assert_eq!(row_chunks(&spans, &members), ["single page appendix", "page # of #"]);

        let row = ChromeRow {
            whole: HashSet::new(),
            chunks: HashSet::from(["page # of #".into()]),
        };
        assert!(row.matches(&spans, &members));
    }
}
