//! Per-document page-chrome detection: running headers, footers and page numbers.
//!
//! The signal is **repetition**: chrome sits at the same distance from the page edge, in
//! the page's top or bottom band, on most pages, with the same text up to its counters ("Page 12 of 340"
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

/// Fraction of the document's MODAL display page height that sets the chrome band depth.
/// The band is a document-level absolute size, not a per-page fraction: a word processor
/// authors its header at a fixed distance from the paper edge, so the same 80 pt header
/// offset must stay in-band when a minority of pages turn landscape (whose smaller height
/// would otherwise shrink the band below the offset and leak the header there).
const BAND_FRAC: f32 = 0.12;
/// Per-page ceiling on the band: never deeper than this fraction of the page's own
/// height, so a document whose modal page dwarfs an occasional small page cannot push
/// the band into that page's body.
const BAND_PAGE_CAP: f32 = 0.3;
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
    /// The document-level band depth (pt) detection ran with. Filtering must reuse it:
    /// re-deriving a band from the filtered page's own box would make detection and
    /// filtering disagree on mixed-size documents.
    band: f32,
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
        for (top, bucket, members) in band_rows(dspans, dbox, self.band) {
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
/// The band edges follow the **display page box** (`/MediaBox` through the page's
/// `/Rotate`), not the used text extent — a page whose body stops short must not pull
/// the band down into it. The band's *depth* is document-level ([`BAND_FRAC`] of the
/// modal display height, per-page capped by [`BAND_PAGE_CAP`]), and rows are keyed by
/// distance from the page edge — both so that chrome authored at a fixed offset from the
/// paper edge coincides across portrait and landscape pages of one document.
pub(crate) fn plan_chrome(
    access: &dyn crate::access::DocumentAccess,
    page_spans: &[(u32, ObjectId, Vec<Span>)],
    body: f32,
) -> ChromePlan {
    let n_pages = page_spans.len();
    if n_pages < MIN_PAGES {
        return ChromePlan::default();
    }
    let display_box = |pid: ObjectId| {
        let page_box = crate::pdfobj::page_box(access, pid).unwrap_or([
            0.0,
            0.0,
            crate::pdfobj::DEFAULT_PAGE_PTS.0,
            crate::pdfobj::DEFAULT_PAGE_PTS.1,
        ]);
        let turn = PageTurn::new(crate::pdfobj::page_rotation(access, pid), page_box);
        (turn, turn.rect(page_box[0], page_box[2], page_box[1], page_box[3]))
    };
    // Modal display height → the document band depth. Whole-pt quantization; ties break
    // to the larger height so the outcome is deterministic across hash orders.
    let mut heights: HashMap<i32, usize> = HashMap::new();
    for (_, pid, _) in page_spans {
        let (_, (_, _, y0, y1)) = display_box(*pid);
        *heights.entry((y1 - y0).round() as i32).or_default() += 1;
    }
    let modal_h = heights
        .into_iter()
        .max_by_key(|&(h, n)| (n, h))
        .map(|(h, _)| h as f32)
        .unwrap_or(crate::pdfobj::DEFAULT_PAGE_PTS.1);
    let band = modal_h.max(1.0) * BAND_FRAC;
    #[derive(Default)]
    struct SeenRow {
        whole: HashMap<String, HashSet<u32>>,
        chunks: HashMap<String, HashSet<u32>>,
    }
    // (band, bucket) → whole/chunk recurrence keys → distinct pages seen on.
    let mut seen: HashMap<(bool, i32), SeenRow> = HashMap::new();
    for (pno, pid, spans) in page_spans {
        let (turn, dbox) = display_box(*pid);
        let turned: Option<Vec<Span>> =
            (!turn.is_identity()).then(|| spans.iter().map(|s| crate::html::turn_span(turn, s)).collect());
        let dspans: &[Span] = turned.as_deref().unwrap_or(spans.as_slice());
        for (top, bucket, members) in band_rows(dspans, dbox, band) {
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
    let mut plan = ChromePlan { band, ..ChromePlan::default() };
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
/// `(is_top_band, edge_distance_bucket, member span indices)` — upright spans only,
/// members in x order. The bucket quantizes the span's DISTANCE FROM THE PAGE EDGE by
/// [`ROW_QUANT`], not its absolute y: a header 80 pt below the top edge gets the same
/// key on a portrait and a landscape page, which is what lets a minority orientation's
/// instances join the majority's recurrence counts. `band` is the document-level band
/// depth (see [`plan_chrome`]), capped here at [`BAND_PAGE_CAP`] of this page's height.
fn band_rows(dspans: &[Span], dbox: (f32, f32, f32, f32), band: f32) -> Vec<(bool, i32, Vec<usize>)> {
    let (_, _, y0, y1) = dbox;
    let band = band.min((y1 - y0).max(1.0) * BAND_PAGE_CAP);
    let mut rows: HashMap<(bool, i32), Vec<usize>> = HashMap::new();
    for (i, s) in dspans.iter().enumerate() {
        if s.angle.abs() >= 0.01 || s.text.trim().is_empty() {
            continue;
        }
        let (top, edge_dist) = if s.y >= y1 - band {
            (true, y1 - s.y)
        } else if s.y <= y0 + band {
            (false, s.y - y0)
        } else {
            continue;
        };
        rows.entry((top, (edge_dist / ROW_QUANT).round() as i32)).or_default().push(i);
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

    fn span_at(x: f32, y: f32, size: f32, text: &str) -> Span {
        Span {
            x,
            y,
            size,
            width: text.chars().count() as f32 * size * 0.5,
            text: text.to_string(),
            bold: false,
            italic: false,
            mono: false,
            angle: 0.0,
            font: 0,
            mcid: None,
            source: crate::text::SourceSlice::test_occurrence(0, 0),
        }
    }

    #[test]
    fn band_rows_key_the_same_edge_offset_identically_across_page_heights() {
        // The Cerisa leak: a header 80 pt below the top edge and a footer 42 pt above the
        // bottom edge, on an A4 portrait page and an A4 landscape page. One document-level
        // band (12% of the modal portrait height ≈ 101 pt) must put both instances of each
        // in the same (band, bucket) key, or the minority orientation can never reach the
        // coverage floor and its chrome leaks.
        let band = 841.89 * BAND_FRAC;
        let portrait = (0.0, 595.28, 0.0, 841.89);
        let landscape = (0.0, 841.89, 0.0, 595.28);
        let header_p = [span_at(56.7, 841.89 - 80.0, 8.0, "PL636 Cerisa")];
        let header_l = [span_at(56.7, 595.28 - 80.0, 8.0, "PL636 Cerisa")];
        let footer_p = [span_at(500.0, 42.0, 8.0, "24 of 172")];
        let footer_l = [span_at(750.0, 42.0, 8.0, "25 of 172")];
        let key = |spans: &[Span], dbox| {
            let rows = band_rows(spans, dbox, band);
            assert_eq!(rows.len(), 1, "the row must be in-band: {rows:?}");
            (rows[0].0, rows[0].1)
        };
        assert_eq!(key(&header_p, portrait), key(&header_l, landscape));
        assert_eq!(key(&footer_p, portrait), key(&footer_l, landscape));
        // And a genuine body line stays out of the band on both.
        assert!(band_rows(&[span_at(56.7, 400.0, 10.0, "body prose")], portrait, band).is_empty());
        assert!(band_rows(&[span_at(56.7, 300.0, 10.0, "body prose")], landscape, band).is_empty());
    }

    #[test]
    fn band_page_cap_keeps_a_small_pages_body_out_of_a_big_documents_band() {
        // A 200 pt-tall insert in a document whose modal page is A4: the raw document band
        // (~101 pt) would cover half the little page; the per-page cap holds it to 30%.
        let band = 841.89 * BAND_FRAC;
        let small = (0.0, 300.0, 0.0, 200.0);
        let mid_span = [span_at(20.0, 100.0, 10.0, "centre text")];
        assert!(band_rows(&mid_span, small, band).is_empty());
        let top_span = [span_at(20.0, 190.0, 8.0, "small page header")];
        assert_eq!(band_rows(&top_span, small, band).len(), 1);
    }

    #[test]
    fn plan_chrome_bans_a_fixed_edge_offset_header_on_the_minority_landscape_pages() {
        // Six portrait + two landscape pages, one running header 80 pt from the top edge
        // on every page, varying body text mid-page. Detection must count all eight pages
        // into one row (coverage 8/8) and drop_mask must ban the header on the landscape
        // pages too — the exact shape of the Cerisa PL636 leak.
        use lopdf::{dictionary, Document, Object};
        let mut doc = Document::with_version("1.5");
        let mut page_spans: Vec<(u32, ObjectId, Vec<Span>)> = Vec::new();
        let bodies = [
            "the reservoir shows strong lateral continuity",
            "porosity trends follow the depositional axis",
            "well ties confirm the velocity model update",
            "the mudstone interval caps the upper zone",
            "saturation heights match the capillary data",
            "the aquifer support is weaker than mapped",
            "seismic amplitudes brighten toward the crest",
            "the contact is flat across both segments",
        ];
        for (i, body_text) in bodies.iter().enumerate() {
            let landscape = i == 3 || i == 6;
            let (w, h) = if landscape { (841.89, 595.28) } else { (595.28, 841.89) };
            let pid = doc.add_object(Object::Dictionary(dictionary! {
                "Type" => "Page",
                "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
            }));
            let spans = vec![
                span_at(56.7, h - 80.0, 8.0, "PL636 Cerisa - Concept - General"),
                span_at(56.7, h * 0.5, 10.0, body_text),
            ];
            page_spans.push((i as u32 + 1, pid, spans));
        }
        let access = crate::access::test_adapter(&doc);
        let plan = plan_chrome(&access, &page_spans, 10.0);
        assert!(!plan.is_empty(), "the header row must be detected");
        for (i, (_, _, spans)) in page_spans.iter().enumerate() {
            let landscape = i == 3 || i == 6;
            let (w, h) = if landscape { (841.89, 595.28) } else { (595.28, 841.89) };
            let mask = plan
                .drop_mask(spans, (0.0, w, 0.0, h))
                .unwrap_or_else(|| panic!("page {} must have a chrome match", i + 1));
            assert!(mask[0], "header must be banned on page {} (landscape={landscape})", i + 1);
            assert!(!mask[1], "body must survive on page {}", i + 1);
        }
    }
}
