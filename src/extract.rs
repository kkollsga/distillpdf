//! Image, font and table extraction pillars, built on lopdf's object model.

use crate::text::{self, Span};
use lopdf::{Dictionary, Document, Object};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

fn filter_to_format(filters: &Option<Vec<String>>) -> &'static str {
    match filters {
        Some(fs) => {
            if fs.iter().any(|f| f == "DCTDecode") {
                "jpeg"
            } else if fs.iter().any(|f| f == "JPXDecode") {
                "jpx"
            } else if fs.iter().any(|f| f == "CCITTFaxDecode") {
                "ccitt"
            } else if fs.iter().any(|f| f == "JBIG2Decode") {
                "jbig2"
            } else {
                "raw" // Flate/LZW/none -> needs PNG assembly from samples
            }
        }
        None => "raw",
    }
}

/// Extract images from all pages as a list of dicts:
/// {page, index, width, height, color_space, format, data(bytes)}.
pub fn extract_images<'py>(py: Python<'py>, doc: &Document) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for (&pno, &page_id) in &doc.get_pages() {
        let imgs = match doc.get_page_images(page_id) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (idx, im) in imgs.iter().enumerate() {
            let d = PyDict::new(py);
            d.set_item("page", pno)?;
            d.set_item("index", idx)?;
            d.set_item("width", im.width)?;
            d.set_item("height", im.height)?;
            d.set_item("color_space", im.color_space.clone())?;
            d.set_item("format", filter_to_format(&im.filters))?;
            d.set_item("data", PyBytes::new(py, im.content))?;
            list.append(d)?;
        }
    }
    Ok(list)
}

/// Group spans into visual rows (top-to-bottom), cells left-to-right.
fn rows_of(mut spans: Vec<Span>) -> Vec<Vec<Span>> {
    spans.retain(|s| !s.text.trim().is_empty() && s.angle.abs() < 0.01); // rotated text isn't tabular
    if spans.is_empty() {
        return Vec::new();
    }
    let band = (spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32 * 0.6).max(2.0);
    // Cluster by actual y-proximity, not rounded bands: a span joins the current
    // row if within `band` (≈half a line) of the row's reference y. This merges
    // small sub/superscripts (e.g. the "BASE"/"LARGE" in BERT_BASE/BERT_LARGE,
    // ~1pt off the baseline) into their row instead of letting a rounding boundary
    // split them into a 1-cell row that would flush a table run mid-table.
    spans.sort_by(|p, q| q.y.partial_cmp(&p.y).unwrap_or(std::cmp::Ordering::Equal));
    let mut rows: Vec<Vec<Span>> = Vec::new();
    let mut ref_y: Option<f32> = None;
    for s in spans {
        if ref_y.is_none_or(|ry| (ry - s.y).abs() > band) {
            rows.push(Vec::new());
            ref_y = Some(s.y);
        }
        rows.last_mut().unwrap().push(s);
    }
    for r in &mut rows {
        r.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
    }
    rows
}

/// A merged cell: text, its left x-edge, and current right edge.
struct Cell {
    x: f32,
    end: f32,
    text: String,
}

/// Merge a row's word-spans into cells: small inter-word gaps (prose) collapse;
/// wide gutters (table columns) stay separate.
/// Whether a space belongs between two glyph-runs joined into one cell — mirrors
/// the HTML typographic binding so "33"+"."+"20" becomes "33.20" (a single value),
/// not "33 . 20" (which then reads as two columns).
fn join_space(prev: &str, next: &str) -> bool {
    let (p, n) = match (prev.chars().last(), next.chars().next()) {
        (Some(p), Some(n)) => (p, n),
        _ => return false,
    };
    if ")]},.;:!?%".contains(n) {
        return false; // no space before closing/trailing punctuation
    }
    if "([{".contains(p) {
        return false; // no space after an opening bracket
    }
    if matches!(p, '.' | ':' | '/' | '-' | ',' | '\u{2212}') && n.is_ascii_digit() {
        return false; // numeric separator (decimal/ratio/range): 33.20, 1:3, 27-31
    }
    true
}

fn is_num_token(t: &str) -> bool {
    let t = t.trim();
    !t.is_empty() && t.chars().any(|c| c.is_ascii_digit()) && t.chars().all(|c| c.is_ascii_digit() || ".,%+-±()*".contains(c) || c == '\u{2212}')
}

fn row_cells(row: &[Span]) -> Vec<Cell> {
    let mut cells: Vec<Cell> = Vec::new();
    for s in row {
        let txt = s.text.trim();
        if txt.is_empty() {
            continue;
        }
        let w = if s.width > 0.1 { s.width } else { txt.chars().count() as f32 * s.size * 0.5 };
        let gap = cells.last().map_or(f32::INFINITY, |p| s.x - p.end);
        // Two NUMERIC tokens separated by any real gap are adjacent data columns,
        // not one cell ("33.20 0.963" → two cells); they merge only on a hair-thin
        // gap (same number split mid-glyph). Text ("BERT BASE") still merges up to
        // the normal column gutter.
        let numeric_split = is_num_token(txt)
            && cells.last().is_some_and(|p| is_num_token(p.text.rsplit(' ').next().unwrap_or("")))
            && gap > s.size * 0.45;
        match cells.last_mut() {
            Some(prev) if gap < s.size * 1.3 && !numeric_split => {
                if join_space(&prev.text, txt) {
                    prev.text.push(' ');
                }
                prev.text.push_str(txt);
                prev.end = s.x + w;
            }
            _ => cells.push(Cell { x: s.x, end: s.x + w, text: txt.to_string() }),
        }
    }
    cells
}

/// Cluster cell LEFT edges into column anchors (gap-based, tolerance `tol`). This is the
/// pre-band-model detector, kept as a FALLBACK: the whitespace-lane `column_bands` is the
/// primary, but on a wide-first-column table (e.g. the Transformer "Layer Type | …" Table 1)
/// a long row label bridges the lane and merges columns, so the band model degenerates to
/// <2 columns and the table is lost. Left-x clustering recovers those — it anchors on where
/// each column STARTS, which a wide neighbour doesn't disturb.
fn columns(rows: &[Vec<Cell>], tol: f32) -> Vec<f32> {
    let mut xs: Vec<f32> = rows.iter().flat_map(|r| r.iter().map(|c| c.x)).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut cols: Vec<f32> = Vec::new();
    for x in xs {
        if cols.last().is_none_or(|&c| x - c > tol) {
            cols.push(x);
        }
    }
    cols
}

/// Index of the column anchor nearest to `x` (left-x fallback occupancy counting).
fn nearest_col(cols: &[f32], x: f32) -> Option<usize> {
    cols.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (x - **a).abs().partial_cmp(&(x - **b).abs()).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
}

/// Column bands via vertical whitespace lanes (PASS 1 of table parsing).
///
/// Project every cell's x-interval from the data rows AND any header rows, then read
/// off the maximal x-ranges that some row covers; the clear gaps between them are the
/// column separators. This keys on WHERE TEXT SITS, not where it starts, so a
/// right-aligned numeric column (whose left edges scatter row to row) stays a single
/// band, and because the header rows are projected too, a SPARSE column the body
/// rarely fills is still a band (the header spans it). `bridge` is how many outlier
/// rows may span a lane before it stops being a separator (0 = a lane must be fully
/// clear). Cells within a row are disjoint, so interval coverage == row coverage.
/// Returns each column band as (lo, hi), left→right; deterministic (event sweep).
fn column_bands(rows: &[&[Cell]], bridge: usize) -> Vec<(f32, f32)> {
    let mut ev: Vec<(f32, i32)> = Vec::new();
    for r in rows {
        for c in *r {
            if c.end > c.x {
                ev.push((c.x, 1));
                ev.push((c.end, -1));
            }
        }
    }
    if ev.is_empty() {
        return Vec::new();
    }
    ev.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.cmp(&a.1)));
    let mut bands: Vec<(f32, f32)> = Vec::new();
    let mut cov = 0i32;
    let mut prev_x = ev[0].0;
    let mut in_band = false;
    let (mut lo, mut hi) = (0.0f32, 0.0f32);
    for (x, d) in ev {
        if x > prev_x {
            // segment [prev_x, x) carried coverage `cov`
            if cov as usize > bridge {
                if !in_band {
                    in_band = true;
                    lo = prev_x;
                }
                hi = x;
            } else if in_band {
                bands.push((lo, hi));
                in_band = false;
            }
        }
        cov += d;
        prev_x = x;
    }
    if in_band {
        bands.push((lo, hi));
    }
    bands
}

/// Index of the band whose interval contains `x`, else the nearest band by distance
/// to its interval. Used to assign a span to a column in PASS 2.
fn band_of(bands: &[(f32, f32)], x: f32) -> Option<usize> {
    if bands.is_empty() {
        return None;
    }
    for (i, &(lo, hi)) in bands.iter().enumerate() {
        if x >= lo && x <= hi {
            return Some(i);
        }
    }
    bands
        .iter()
        .enumerate()
        .min_by(|(_, &(lo, hi)), (_, &(lo2, hi2))| {
            let d = |l: f32, h: f32| if x < l { l - x } else { x - h };
            d(lo, hi).partial_cmp(&d(lo2, hi2)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
}

/// Structural ADMISSION test: is this region a genuine data table, or prose / an
/// equation / a symbolic matrix that merely happens to have aligned tokens?
///
/// This is the single backstop that keeps false positives out. It is deliberately
/// kept SEPARATE from column-keeping (how many columns survive) so that recovering a
/// sparse column can never silently re-admit a prose/equation block: admission reads
/// the region's content, column-keeping reads its geometry, and the two no longer
/// interfere. Returns true to accept the region as a table.
fn is_coherent_grid(grid: &[Vec<String>]) -> bool {
    // Prose guard: real tabular cells are terse. A 2-column block averaging >4
    // words/cell is running prose (wrapped body lines), not a table.
    let (mut wc, mut nz, mut prose) = (0usize, 0usize, 0usize);
    for row in grid {
        for c in row {
            let w = c.split_whitespace().count();
            if w > 0 {
                wc += w;
                nz += 1;
                if w > 8 {
                    prose += 1;
                }
            }
        }
    }
    let mean_words = if nz > 0 { wc as f32 / nz as f32 } else { 0.0 };
    let ncols = grid.first().map(|r| r.len()).unwrap_or(0);
    if ncols <= 2 && mean_words > 4.0 {
        return false;
    }
    // 2-col body gridded into 3 cols (gutter-crossing title): tell is a phantom
    // anchor column empty in nearly every row plus long cells. Real 3-col tables
    // are populated, so they pass (e.g. the W-9 field tables).
    let has_empty_col = ncols > 0
        && !grid.is_empty()
        && (0..ncols).any(|c| {
            let empty = grid.iter().filter(|r| r.get(c).is_none_or(|s| s.trim().is_empty())).count();
            empty * 5 >= grid.len() * 4
        });
    if ncols == 3 && mean_words > 4.5 && has_empty_col {
        return false;
    }
    // Wider mis-grids: reject only when nearly every cell is a full sentence.
    if nz >= 6 && prose * 3 >= nz * 2 && mean_words > 6.0 {
        return false;
    }
    // Display EQUATION mis-detected as a table: cells are dominated by math
    // operators / Greek (not numeric data) and the region carries an '=' or an
    // equation-number "(N)". Reject so the equation stays in the text flow
    // (where it is reassembled as one block) instead of a spurious <table>. A
    // numeric data table has no operators and no '=', so it is unaffected.
    let opcell = |t: &str| t.chars().any(|c| "=+−–×÷·≤≥≠≈∝∫∑∏√∈∉∂∇→←↔⇒⇐↦∼≜≡∥⟨⟩".contains(c) || "αβγδεζηθικλμνξπρςστυϕφχψωΓΔΘΛΞΠΣΦΨΩ".contains(c));
    let op = grid.iter().flatten().filter(|c| opcell(c)).count();
    // An equation is signalled by a RELATION — '=' or an inequality/equivalence
    // (≤ ≥ ≠ ≈ ≜ ≡ ∝). These appear in display math/inequalities but almost never as
    // the content of a data cell (a stats table's "p ≤ 0.05" carries real words too,
    // which the alpha_words gate below preserves).
    let has_rel = grid.iter().flatten().any(|c| c.chars().any(|ch| "=≤≥≠≈≜≡∝".contains(ch)));
    let eqnum = grid.iter().flatten().any(|c| {
        let t = c.trim();
        let inner: String = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or("").to_string();
        !inner.is_empty() && inner.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
    });
    // Real (alphabetic, ≥2-letter) words — an equation has almost none; its
    // "words" are space-separated symbols. A data table has real words.
    let alpha_words = grid.iter().flatten().flat_map(|c| c.split(|ch: char| !ch.is_alphabetic())).filter(|w| w.chars().count() >= 3).count();
    // Reject an equation region: it carries a relation or eq-number, OR it is
    // operator-dense (a relation/arrow chain), and it has almost no real words.
    if nz > 0 && alpha_words <= nz && ((op >= 1 && (has_rel || eqnum)) || op * 2 >= nz) {
        return false;
    }
    // Symbolic MATRIX/array mis-detected as a table (e.g. a block matrix of
    // subscripted variables W₀, D₁Y₁, ∇W₁). Unlike the equation case above it
    // carries no '=' / eq-number and is not operator-dense — its cells are plain
    // variables. Signature: NO data values (a real data table has decimals or
    // multi-digit numbers; a matrix has only single-digit sub/superscripts), NO
    // real words, and a majority of cells are variable-like (start with a letter).
    // A numeric data table fails this (its cells start with digits and it has data
    // values), so it is unaffected.
    let dataval = grid
        .iter()
        .flatten()
        .filter(|c| {
            let b = c.as_bytes();
            (0..b.len()).any(|i| b[i].is_ascii_digit() && i + 2 < b.len() && b[i + 1] == b'.' && b[i + 2].is_ascii_digit())
                || c.chars().filter(|ch| ch.is_ascii_digit()).count() >= 3
        })
        .count();
    let letter_start = grid.iter().flatten().filter(|c| c.trim_start().chars().next().is_some_and(|ch| ch.is_alphabetic())).count();
    if nz >= 4 && dataval == 0 && alpha_words == 0 && letter_start * 2 >= nz {
        return false;
    }
    // Scattered symbolic DIAGRAM mis-detected as a table (e.g. a commutative
    // diagram: nodes X, Y, D, E with arrow labels ⟨(234)⟩ flung across the page).
    // Distinct from the matrix case above, which needs a letter-start majority —
    // a diagram is half bare digits, half symbols, so no axis dominates. Its tells
    // are instead: very LOW fill (nodes float in whitespace, unlike a real table
    // whose occupied columns are densely populated), NO numeric data values, almost
    // no real words, and either arrow/operator glyphs or short variable-like cells.
    // Gated on dataval == 0 so no numeric data table can ever be hit.
    // Commutative DIAGRAM mis-detected as a table (nodes X, Y, D, E with morphism
    // labels ⟨(234)⟩ scattered across the page — or, once the left-x fallback merges
    // them, a degenerate 2-column block). The tell is a category-theory
    // arrow/morphism glyph (→ ↦ ⟨ ⟩ …), which essentially never appears in tabular
    // DATA, in a grid that is NOT word-dominated. A word-heavy table (a state- or
    // reaction-transition table whose cells are real labels — conv → relu → pool)
    // survives via the alpha-word gate, so numeric/label tables are unaffected.
    let diagram_glyph = grid.iter().flatten().any(|c| c.chars().any(|ch| "→←↔⇒⇐↦⟨⟩∘↪↩⟶⟵↠↣".contains(ch)));
    // A real DATA table is full of decimal values (319.61, 0.446); a commutative
    // diagram has none — its numbers are bare node indices. Require decimal-absence
    // so a numeric table that merely uses an arrow in a header (input → output) is
    // never mistaken for a diagram.
    let has_decimal = grid.iter().flatten().any(|c| {
        let b = c.as_bytes();
        (0..b.len()).any(|i| b[i].is_ascii_digit() && i + 2 < b.len() && b[i + 1] == b'.' && b[i + 2].is_ascii_digit())
    });
    if nz >= 6 && diagram_glyph && alpha_words * 3 <= nz && !has_decimal {
        return false;
    }
    true
}

/// Detect tables: runs of >=3 consecutive rows that each have >=2 gutter-separated
/// cells and share >=2 columns occupied in a majority of rows. This rejects
/// word-positioned prose (whose words merge into a single cell).
/// A detected table with its vertical extent (PDF user space, y increases up).
#[derive(Clone)]
pub struct PosTable {
    pub y_top: f32,
    pub y_bottom: f32,
    pub x_left: f32,
    pub x_right: f32,
    pub grid: Vec<Vec<String>>,
    /// Grouped/multi-level HEADER rows mapped onto the data column grid, each cell as
    /// (text, colspan): a header cell spanning several data columns ("Masking Rates"
    /// over MASK/SAME/RND) carries colspan>1; cells over one column carry colspan 1.
    /// Empty when the table has no detached header (the data grid's row 0 is the header).
    pub header: Vec<Vec<(String, usize)>>,
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

/// The x of the central gutter when the page is a two-column layout, else None.
///
/// A two-column page is split down the middle by a vertical whitespace lane that
/// is empty across (almost) every text row — a handful of full-width lines (a
/// title, a banner) are tolerated. Crucially, when such a clean centre gutter
/// exists there is, by definition, *no* full-width element crossing it, so the
/// caller can treat each side completely independently. A page where a wide
/// element (a spanning figure/table) sits across the centre has no clean gutter,
/// returns None, and is handled whole.
pub(crate) fn central_gutter(spans: &[Span]) -> Option<f32> {
    let rows = rows_of(spans.iter().map(clone_span).collect());
    if rows.len() < 6 {
        return None;
    }
    let span_r = |s: &Span| (s.x, s.x + s.width.max(s.size * 0.3));
    let x0 = spans.iter().map(|s| s.x).fold(f32::INFINITY, f32::min);
    let x1 = spans.iter().map(|s| span_r(s).1).fold(f32::NEG_INFINITY, f32::max);
    let width = x1 - x0;
    if !(width > 1.0) {
        return None;
    }
    // Scan the central band for the x crossed by the fewest rows.
    let (lo, hi) = (x0 + width * 0.30, x0 + width * 0.70);
    let step = (width / 200.0).max(1.0);
    let row_clear = |x: f32| rows.iter().filter(|r| !r.iter().any(|s| { let (a, b) = span_r(s); a <= x && x <= b })).count();
    let mut best = (0usize, lo);
    let mut x = lo;
    while x <= hi {
        let c = row_clear(x);
        if c > best.0 {
            best = (c, x);
        }
        x += step;
    }
    // A two-column LAYOUT has wide wrapping PROSE on both sides: each side's line is
    // a SINGLE wide cell (>=4 words, spanning most of its half-width). A table half
    // has >=2 cells (its own columns), so a table's internal gutter — even one near
    // the centre — is never mistaken for a page split. Require >=3 prose lines/side.
    let g = best.1;
    let prose_lines = |left: bool, min_w: f32| {
        rows.iter()
            .filter(|r| {
                let side: Vec<Span> = r.iter().filter(|s| (s.x + s.width.max(0.0) * 0.5 < g) == left).map(clone_span).collect();
                let cells = row_cells(&side);
                cells.len() == 1
                    && cells[0].text.split_whitespace().count() >= 4
                    && (cells[0].end - cells[0].x) > min_w
            })
            .count()
    };
    if best.0 as f32 >= rows.len() as f32 * 0.88
        && prose_lines(true, (g - x0) * 0.5) >= 3
        && prose_lines(false, (x1 - g) * 0.5) >= 3
    {
        Some(g)
    } else {
        None
    }
}

/// Detect tables. On a two-column page we split down the middle and detect each
/// side independently — a clean centre gutter guarantees nothing spans it, so the
/// two sides are genuinely separate (this is what stops adjacent-column prose from
/// merging into a phantom wide table). Otherwise (single column, or a full-width
/// element across the centre) the whole page is one region.
pub fn detect_tables_pos(spans: &[Span]) -> Vec<PosTable> {
    // Tables are built from upright text only — rotated labels (axis titles etc.) must
    // not perturb gutter detection or column structure (they're figure labels).
    let upright: Vec<Span> = spans.iter().filter(|s| s.angle.abs() < 0.01).map(clone_span).collect();
    let spans = &upright[..];
    match central_gutter(spans) {
        None => detect_tables_region(spans),
        Some(g) => {
            // Split down the gutter and detect each side independently (this is what
            // stops adjacent-column prose from merging into a phantom wide table).
            let side = |left: bool| -> Vec<Span> {
                spans.iter().filter(|s| (s.x + s.width.max(0.0) * 0.5 < g) == left).map(clone_span).collect()
            };
            let lt = detect_tables_region(&side(true));
            let rt = detect_tables_region(&side(false));
            // A full-width table (e.g. BERT's GLUE table) was split into a left half
            // and a right half that occupy the SAME rows. Detect that: a left-side
            // table whose vertical extent overlaps a right-side table is one table cut
            // in two. Re-detect across the FULL width within just that vertical band
            // (prose outside the band can't interfere) to recover the whole table. A
            // single-column table beside prose has no mate (prose isn't a table), so
            // it is kept as-is — no cross-column bleed.
            let overlaps = |a: &PosTable, b: &PosTable| {
                let lo = a.y_bottom.max(b.y_bottom);
                let hi = a.y_top.min(b.y_top);
                let span = (a.y_top - a.y_bottom).min(b.y_top - b.y_bottom).max(1.0);
                (hi - lo) >= span * 0.5
            };
            let mut out: Vec<PosTable> = Vec::new();
            let mut used_r = vec![false; rt.len()];
            for l in &lt {
                match rt.iter().enumerate().find(|(j, r)| !used_r[*j] && overlaps(l, r)) {
                    Some((j, r)) => {
                        used_r[j] = true;
                        let (yb, yt) = (l.y_bottom.min(r.y_bottom), l.y_top.max(r.y_top));
                        let pad = 2.0;
                        let band: Vec<Span> =
                            spans.iter().filter(|s| s.y >= yb - pad && s.y <= yt + pad).map(clone_span).collect();
                        let merged = detect_tables_region(&band);
                        if merged.is_empty() {
                            out.push(l.clone());
                            out.push(r.clone());
                        } else {
                            out.extend(merged);
                        }
                    }
                    None => out.push(l.clone()),
                }
            }
            for (j, r) in rt.into_iter().enumerate() {
                if !used_r[j] {
                    out.push(r);
                }
            }
            out
        }
    }
}

/// Detect tables within a single region (one text column, or the whole page):
/// runs of >=3 consecutive multi-cell rows sharing >=2 aligned columns (occupied
/// in a majority of rows). Rejects word-positioned prose (words merge to a cell).
fn detect_tables_region(spans: &[Span]) -> Vec<PosTable> {
    let avg_size = if spans.is_empty() {
        10.0
    } else {
        spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32
    };
    let tol = (avg_size * 1.5).max(6.0);
    let rows = rows_of(spans.iter().map(clone_span).collect());
    let mut celled: Vec<(f32, Vec<Cell>, Vec<Span>)> = rows
        .iter()
        .map(|r| (r.first().map(|s| s.y).unwrap_or(0.0), row_cells(r), r.iter().map(clone_span).collect()))
        .collect();

    // Coalesce wrapped multi-line cells. A borderless table with a long column (e.g.
    // a "Description" that wraps) emits its overflow lines as rows holding only that
    // one interior cell. Those 1-cell rows would otherwise break the multi-cell row
    // run and the table would be missed (and its bare ruling leak out as a figure).
    // Fold each such overflow line into the nearest multi-cell row whose columns
    // include it, so the wrapped cell stays a single cell and the run is contiguous.
    {
        let anchors: Vec<usize> = (0..celled.len()).filter(|&i| celled[i].1.len() >= 2).collect();
        if anchors.len() >= 2 {
            // Left edge of the table body: a genuine row label starts here; an overflow
            // line of a wrapped *interior* cell does not, which is how we tell them apart.
            let region_min_x = anchors
                .iter()
                .map(|&i| celled[i].1.iter().map(|c| c.x).fold(f32::INFINITY, f32::min))
                .fold(f32::INFINITY, f32::min);
            let mut absorb: Vec<(usize, usize)> = Vec::new(); // (anchor, overflow-row)
            for ti in 0..celled.len() {
                if celled[ti].1.len() != 1 {
                    continue;
                }
                let cx = celled[ti].1[0].x;
                if cx <= region_min_x + tol {
                    continue; // sits at the left edge -> a row label / prose line, not overflow
                }
                let mut best: Option<(usize, f32)> = None;
                for &ai in &anchors {
                    let dy = (celled[ai].0 - celled[ti].0).abs();
                    if dy > avg_size * 1.8 {
                        continue; // not vertically adjacent -> not the same wrapped cell
                    }
                    if !celled[ai].1.iter().any(|c| (c.x - cx).abs() <= tol) {
                        continue; // overflow x must line up with one of the anchor's columns
                    }
                    if best.is_none_or(|(_, bd)| dy < bd) {
                        best = Some((ai, dy));
                    }
                }
                if let Some((ai, _)) = best {
                    absorb.push((ai, ti));
                }
            }
            let mut drop = vec![false; celled.len()];
            for (ai, ti) in absorb {
                let mut moved = std::mem::take(&mut celled[ti].2);
                drop[ti] = true;
                celled[ai].2.append(&mut moved);
            }
            if drop.iter().any(|&d| d) {
                let mut kept: Vec<(f32, Vec<Cell>, Vec<Span>)> = Vec::new();
                for (i, mut row) in celled.into_iter().enumerate() {
                    if drop[i] {
                        continue;
                    }
                    // Reading order within a merged cell is top-to-bottom: sort the row's
                    // spans by descending y (then x) so bin_row accumulates the wrapped
                    // lines in order. The anchor's cell list (row.1) is left untouched —
                    // the overflow lands in an existing column, so the column structure
                    // and x-extent are unchanged.
                    row.2.sort_by(|a, b| {
                        b.y.partial_cmp(&a.y)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
                    });
                    kept.push(row);
                }
                celled = kept;
            }
        }
    }

    let mut tables = Vec::new();

    let flush = |run: &Vec<&(f32, Vec<Cell>, Vec<Span>)>, headers: &[&(f32, Vec<Cell>, Vec<Span>)], tables: &mut Vec<PosTable>| {
        if run.len() < 3 {
            return;
        }
        let owned: Vec<Vec<Cell>> = run
            .iter()
            .map(|(_, c, _)| c.iter().map(|x| Cell { x: x.x, end: x.end, text: x.text.clone() }).collect())
            .collect();
        // Region x-extent of the DATA.
        let (mut x_left, mut x_right) = (f32::INFINITY, f32::NEG_INFINITY);
        for row in &owned {
            for c in row {
                x_left = x_left.min(c.x);
                x_right = x_right.max(c.end);
            }
        }
        // Build a grid from the RAW SPANS for a candidate set of kept columns (expressed as
        // x-bands) by assigning each span to the band containing its CENTRE, then ADMIT it
        // (prose/equation/matrix reject). Returns (grid, kept left-x anchors) iff admitted.
        // `min_fill` is the minimum fraction of non-empty grid cells required to ACCEPT —
        // 0 for the band model (its header-named keep legitimately produces sparse wide
        // tables), but raised for the left-x fallback so a sparse symbol SCATTER (a
        // commutative diagram, a math array) isn't clustered into a spurious table.
        let try_model = |kept: Vec<(f32, f32)>, min_fill: f32| -> Option<(Vec<Vec<String>>, Vec<f32>)> {
            if kept.len() < 2 {
                return None;
            }
            let grid: Vec<Vec<String>> = run
                .iter()
                .map(|(_, _, spans)| {
                    let mut cells = vec![String::new(); kept.len()];
                    for s in spans {
                        let txt = s.text.trim();
                        if txt.is_empty() {
                            continue;
                        }
                        let w = if s.width > 0.1 { s.width } else { txt.chars().count() as f32 * s.size * 0.5 };
                        if let Some(k) = band_of(&kept, s.x + w * 0.5) {
                            if !cells[k].is_empty() && join_space(&cells[k], txt) {
                                cells[k].push(' ');
                            }
                            cells[k].push_str(txt);
                        }
                    }
                    cells
                })
                .collect();
            if min_fill > 0.0 {
                let total = grid.len() * kept.len();
                let filled = grid.iter().flatten().filter(|c| !c.trim().is_empty()).count();
                if total == 0 || (filled as f32) < min_fill * total as f32 {
                    return None;
                }
            }
            if is_coherent_grid(&grid) {
                Some((grid, kept.iter().map(|b| b.0).collect()))
            } else {
                None
            }
        };
        // PASS 1a (PRIMARY) — whitespace-lane band columns: keys on where text SITS, so
        // right-aligned numerics stay distinct and a header-named sparse column survives.
        let band_kept: Vec<(f32, f32)> = {
            let owned_slices: Vec<&[Cell]> = owned.iter().map(|r| r.as_slice()).collect();
            let bands = column_bands(&owned_slices, 0);
            if bands.len() < 2 {
                Vec::new()
            } else {
                let center = |c: &Cell| (c.x + c.end) * 0.5;
                let mut occ = vec![0usize; bands.len()];
                for row in &owned {
                    let mut hit = vec![false; bands.len()];
                    for c in row {
                        if let Some(bi) = band_of(&bands, center(c)) {
                            hit[bi] = true;
                        }
                    }
                    for (i, &h) in hit.iter().enumerate() {
                        if h {
                            occ[i] += 1;
                        }
                    }
                }
                // A band is NAMED when a header cell (a stranded header row, or the run's own
                // first row) overlaps it by ≥0.35 of its width — header-named bands survive
                // even when the body rarely fills them (wide sparse tables).
                let hdr_src: Vec<&Cell> = headers.iter().flat_map(|hr| hr.1.iter()).chain(owned.first().into_iter().flat_map(|r| r.iter())).collect();
                let body_rows = owned.len();
                (0..bands.len())
                    .filter(|&i| {
                        let (lo, hi) = bands[i];
                        let w = hi - lo;
                        let named = w > 0.0 && hdr_src.iter().any(|c| c.end.min(hi) - c.x.max(lo) > 0.35 * w);
                        occ[i] * 2 >= body_rows || named
                    })
                    .map(|i| bands[i])
                    .collect()
            }
        };
        // PASS 1b (FALLBACK) — left-x clustering, as bands [anchor_k, anchor_{k+1}). Recovers
        // wide-first-column tables the lane model over-merges (Transformer Table 1), where a
        // long row label bridges a lane and collapses the band grid to <2 columns.
        let leftx_kept = || -> Vec<(f32, f32)> {
            let cols = columns(&owned, tol);
            if cols.len() < 2 {
                return Vec::new();
            }
            let mut occ = vec![0usize; cols.len()];
            for row in &owned {
                for c in row {
                    if let Some(ci) = nearest_col(&cols, c.x) {
                        occ[ci] += 1;
                    }
                }
            }
            let keep: Vec<usize> = (0..cols.len()).filter(|&i| occ[i] * 2 >= owned.len()).collect();
            keep.iter()
                .enumerate()
                .map(|(j, &k)| (cols[k], keep.get(j + 1).map(|&nk| cols[nk]).unwrap_or(x_right + tol * 0.5)))
                .collect()
        };
        // Band model first; on its failure (degenerate or rejected) fall back to left-x,
        // which must clear a density bar (≥0.5 filled) so a sparse math scatter the band
        // model correctly rejected isn't resurrected as a spurious table.
        let (grid, kept_x) = match try_model(band_kept, 0.0).or_else(|| try_model(leftx_kept(), 0.5)) {
            Some(gx) => gx,
            None => return,
        };

        // Now that the data table is ACCEPTED (past every prose/equation guard), attach
        // the grouped/multi-level HEADER rows the run-builder skipped — they don't form
        // uniform >=2-cell rows, so they were stranded above the data and leaked into the
        // prose. Map each header cell onto the SINGLE data-column grid: the data columns
        // its x-span covers become one cell with colspan = #covered (a label centred over
        // several columns merges); a cell over one column gets colspan 1; uncovered
        // columns become empty cells. Only rows horizontally overlapping the table count.
        let ncols = kept_x.len();
        let m = tol * 0.5;
        let mut y_top = run.first().map(|(y, _, _)| *y).unwrap_or(0.0);
        let mut header: Vec<Vec<(String, usize)>> = Vec::new();
        for hr in headers.iter() {
            let (hx0, hx1) = hr.1.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), c| (a.min(c.x), b.max(c.end)));
            if hx1 < x_left - m || hx0 > x_right + m {
                continue; // not above this table's columns
            }
            let mut slots: Vec<Option<(String, usize)>> = vec![None; ncols];
            let mut owner: Vec<Option<usize>> = vec![None; ncols]; // column → slot holding its text
            // Each data column k owns the x-region [band_k.lo, band_{k+1}.lo) (last → x_right).
            // A header cell covers column k when its x-span overlaps that region by a
            // MEANINGFUL fraction of the column width — so a group label centred over
            // several columns spans them all, while a label that merely grazes a
            // neighbouring column by a few points (e.g. "MNLI" starting 3pt inside the
            // previous column) is NOT pulled into it.
            let col_hi = |k: usize| if k + 1 < ncols { kept_x[k + 1] } else { x_right + m };
            for c in &hr.1 {
                let txt = c.text.trim();
                if txt.is_empty() {
                    continue;
                }
                let covered: Vec<usize> = (0..ncols)
                    .filter(|&k| {
                        let w = col_hi(k) - kept_x[k];
                        let overlap = c.end.min(col_hi(k)) - c.x.max(kept_x[k]);
                        w > 0.0 && overlap > 0.35 * w
                    })
                    .collect();
                let (a, span) = match (covered.first(), covered.last()) {
                    (Some(&f), Some(&l)) => (f, l - f + 1),
                    _ => {
                        // grazes no column centre — pin to the nearest by left edge
                        let k = (0..ncols)
                            .min_by(|&i, &j| (kept_x[i] - c.x).abs().partial_cmp(&(kept_x[j] - c.x).abs()).unwrap_or(std::cmp::Ordering::Equal))
                            .unwrap_or(0);
                        (k, 1)
                    }
                };
                match owner[a] {
                    Some(o) => {
                        // collision: append to whichever slot actually holds the text
                        if let Some((t, _)) = slots[o].as_mut() {
                            t.push(' ');
                            t.push_str(txt);
                        }
                    }
                    None => {
                        slots[a] = Some((txt.to_string(), span));
                        for k in a..(a + span).min(ncols) {
                            owner[k] = Some(a);
                        }
                    }
                }
            }
            // Emit cells in column order, honouring spans (skip columns a spanned cell ate).
            let mut hrow: Vec<(String, usize)> = Vec::new();
            let mut k = 0;
            while k < ncols {
                match slots[k].take() {
                    Some((t, sp)) => {
                        hrow.push((t, sp));
                        k += sp.max(1);
                    }
                    None => {
                        hrow.push((String::new(), 1));
                        k += 1;
                    }
                }
            }
            header.push(hrow);
            y_top = y_top.max(hr.0);
        }
        tables.push(PosTable {
            y_top,
            y_bottom: run.last().map(|(y, _, _)| *y).unwrap_or(0.0),
            x_left,
            x_right,
            grid,
            header,
        });
    };

    let n = celled.len();
    let mut i = 0;
    while i < n {
        if celled[i].1.len() < 2 {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && celled[i].1.len() >= 2 {
            i += 1;
        }
        // Walk upward over contiguous HEADER-like rows immediately above the data run:
        // tightly spaced, short, and not prose (no long sentence cell). Stops at a gap
        // or a prose line, so it captures stranded grouped-header rows without eating
        // body text above the table.
        let mut h = start;
        while h > 0 {
            let cand = &celled[h - 1];
            let gap = cand.0 - celled[h].0; // y increases up; cand sits above
            let words: usize = cand.1.iter().map(|c| c.text.split_whitespace().count()).sum();
            let prose_cell = cand.1.iter().any(|c| c.text.split_whitespace().count() > 5 && c.text.trim_end().ends_with('.'));
            if gap > 0.0 && gap < avg_size * 2.2 && words <= 8 && !prose_cell {
                h -= 1;
            } else {
                break;
            }
        }
        let headers: Vec<&(f32, Vec<Cell>, Vec<Span>)> = celled[h..start].iter().collect();
        let run_slice: Vec<&(f32, Vec<Cell>, Vec<Span>)> = celled[start..i].iter().collect();
        flush(&run_slice, &headers, &mut tables);
    }
    tables
}

fn detect_tables(spans: Vec<Span>) -> Vec<Vec<Vec<String>>> {
    detect_tables_pos(&spans).into_iter().map(|t| t.grid).collect()
}


/// Extract tables from all pages as a list of dicts:
/// {page, n_rows, n_cols, cells: [[str]]}.
pub fn extract_tables<'py>(py: Python<'py>, doc: &Document, raw: &[u8]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for (&pno, &page_id) in &doc.get_pages() {
        let spans = text::extract_spans(doc, page_id, raw);
        for grid in detect_tables(spans) {
            let d = PyDict::new(py);
            d.set_item("page", pno)?;
            d.set_item("n_rows", grid.len())?;
            d.set_item("n_cols", grid.first().map(|r| r.len()).unwrap_or(0))?;
            d.set_item("cells", grid)?;
            list.append(d)?;
        }
    }
    Ok(list)
}

/// Resolve an object that may be a direct value or an indirect reference.
fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(r) => doc.get_object(*r).ok(),
        other => Some(other),
    }
}

/// Does this font dict (or its descendant) carry an embedded font program?
fn font_embedded(doc: &Document, dict: &Dictionary) -> bool {
    // Type0: descriptor lives on the descendant font.
    let descriptor = dict
        .get(b"FontDescriptor")
        .ok()
        .and_then(|o| resolve(doc, o))
        .or_else(|| {
            dict.get(b"DescendantFonts")
                .ok()
                .and_then(|o| resolve(doc, o))
                .and_then(|o| o.as_array().ok())
                .and_then(|a| a.first())
                .and_then(|o| resolve(doc, o))
                .and_then(|o| o.as_dict().ok())
                .and_then(|dd| dd.get(b"FontDescriptor").ok())
                .and_then(|o| resolve(doc, o))
        });
    match descriptor.and_then(|o| o.as_dict().ok()) {
        Some(d) => {
            d.has(b"FontFile") || d.has(b"FontFile2") || d.has(b"FontFile3")
        }
        None => false,
    }
}

/// Extract per-page font info: {page, name, subtype, base_font, encoding,
/// embedded(bool), has_tounicode(bool)}.
pub fn extract_fonts<'py>(py: Python<'py>, doc: &Document) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for (&pno, &page_id) in &doc.get_pages() {
        let fonts = match doc.get_page_fonts(page_id) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for (name, dict) in fonts {
            let d = PyDict::new(py);
            d.set_item("page", pno)?;
            d.set_item("name", String::from_utf8_lossy(&name).into_owned())?;
            let subtype = dict
                .get(b"Subtype")
                .and_then(|o| o.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            d.set_item("subtype", subtype)?;
            let base_font = dict
                .get(b"BaseFont")
                .and_then(|o| o.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            d.set_item("base_font", base_font)?;
            let encoding = dict
                .get(b"Encoding")
                .ok()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_else(|| "custom".to_string());
            d.set_item("encoding", encoding)?;
            d.set_item("embedded", font_embedded(doc, dict))?;
            d.set_item("has_tounicode", dict.has(b"ToUnicode"))?;
            list.append(d)?;
        }
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
    }

    #[test]
    fn numeric_data_table_is_coherent() {
        let g = grid(&[
            &["Region", "Q1", "Q2", "Q3"],
            &["North", "12.5", "13.1", "11.9"],
            &["South", "9.4", "10.2", "8.8"],
        ]);
        assert!(is_coherent_grid(&g));
    }

    #[test]
    fn prose_two_column_rejected() {
        // a glossary: short term + long wrapped definition (mean words/cell > 4 in 2 cols)
        let g = grid(&[
            &["alpha", "the first letter of the Greek alphabet used widely in mathematics"],
            &["beta", "the second letter often denoting a coefficient or a regression slope"],
            &["gamma", "the third letter frequently used for the Lorentz factor in physics"],
        ]);
        assert!(!is_coherent_grid(&g));
    }

    #[test]
    fn commutative_diagram_rejected() {
        // morphism glyphs, no decimal data, not word-dominated → a diagram, not a table
        let g = grid(&[
            &["X", "", "⟨ (234) ⟩", "", "⟨ (34) ⟩"],
            &["E", "1 P", "", "A 4", "Stab(1)"],
            &["", "x", "3 12", "", ""],
            &["2", "4", "", "", ""],
        ]);
        assert!(!is_coherent_grid(&g));
    }
}
