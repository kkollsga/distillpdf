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
        if ref_y.map_or(true, |ry| (ry - s.y).abs() > band) {
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

/// Cluster cell x-positions into column anchors (gap-based, tolerance `tol`).
fn columns(rows: &[Vec<Cell>], tol: f32) -> Vec<f32> {
    let mut xs: Vec<f32> = rows.iter().flat_map(|r| r.iter().map(|c| c.x)).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut cols: Vec<f32> = Vec::new();
    for x in xs {
        if cols.last().map_or(true, |&c| x - c > tol) {
            cols.push(x);
        }
    }
    cols
}

/// Column index for a span at `x` by left-anchored RANGES: column `k` owns
/// `[cols[k], cols[k+1])`, so a word in a wide cell that drifts past the midpoint
/// toward the next anchor still stays in its own column (a small leftward `tol`
/// margin keeps right-aligned values from spilling left). Falls back to column 0
/// for anything left of the first anchor. Used to fill the grid, where a span's true
/// column is the range it lies in — not merely the nearest anchor.
fn col_band(cols: &[f32], x: f32, tol: f32) -> Option<usize> {
    if cols.is_empty() {
        return None;
    }
    let margin = tol * 0.5;
    let mut k = 0;
    for (i, &c) in cols.iter().enumerate() {
        if c <= x + margin {
            k = i;
        } else {
            break;
        }
    }
    Some(k)
}

/// Index of the column anchor nearest to `x`.
fn nearest_col(cols: &[f32], x: f32) -> Option<usize> {
    cols.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (x - **a).abs().partial_cmp(&(x - **b).abs()).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
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
                    if best.map_or(true, |(_, bd)| dy < bd) {
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
        let cols = columns(&owned, tol);
        if cols.len() < 2 {
            return;
        }
        let mut occ = vec![0usize; cols.len()];
        for row in &owned {
            for c in row {
                if let Some(ci) = nearest_col(&cols, c.x) {
                    occ[ci] += 1;
                }
            }
        }
        // Columns occupied in a majority of rows are the table; stray minority cells
        // merge into the nearest kept column (dense grid, no content drop, no bloat).
        let keep: Vec<usize> = (0..cols.len()).filter(|&i| occ[i] * 2 >= owned.len()).collect();
        if keep.len() < 2 {
            return;
        }
        let col_to_keep: Vec<usize> = (0..cols.len())
            .map(|ci| {
                keep.iter()
                    .enumerate()
                    .min_by(|(_, &a), (_, &b)| {
                        (cols[ci] - cols[a]).abs().partial_cmp(&(cols[ci] - cols[b]).abs()).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(k, _)| k)
                    .unwrap_or(0)
            })
            .collect();
        // Fill the grid from the RAW SPANS, binning each to its nearest column. This
        // splits a packed multi-column header ("MNLI-m QNLI MRPC SST-2" crammed into
        // one cell) back into its columns, while data spans that share a column
        // ("BERT" "BASE") still join — so cells track the real column structure.
        let bin_row = |spans: &[Span]| {
            let mut cells = vec![String::new(); keep.len()];
            for s in spans {
                let txt = s.text.trim();
                if txt.is_empty() {
                    continue;
                }
                if let Some(ci) = col_band(&cols, s.x, tol) {
                    let k = col_to_keep[ci];
                    if !cells[k].is_empty() && join_space(&cells[k], txt) {
                        cells[k].push(' ');
                    }
                    cells[k].push_str(txt);
                }
            }
            cells
        };
        let grid: Vec<Vec<String>> = run.iter().map(|(_, _, spans)| bin_row(spans)).collect();
        // Prose guard: real tabular cells are terse. A 2-column block averaging >4
        // words/cell is running prose (wrapped body lines), not a table.
        let (mut wc, mut nz, mut prose) = (0usize, 0usize, 0usize);
        for row in &grid {
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
            return;
        }
        // 2-col body gridded into 3 cols (gutter-crossing title): tell is a phantom
        // anchor column empty in nearly every row plus long cells. Real 3-col tables
        // are populated, so they pass (e.g. the W-9 field tables).
        let has_empty_col = ncols > 0
            && !grid.is_empty()
            && (0..ncols).any(|c| {
                let empty = grid.iter().filter(|r| r.get(c).map_or(true, |s| s.trim().is_empty())).count();
                empty * 5 >= grid.len() * 4
            });
        if ncols == 3 && mean_words > 4.5 && has_empty_col {
            return;
        }
        // Wider mis-grids: reject only when nearly every cell is a full sentence.
        if nz >= 6 && prose * 3 >= nz * 2 && mean_words > 6.0 {
            return;
        }
        // Display EQUATION mis-detected as a table: cells are dominated by math
        // operators / Greek (not numeric data) and the region carries an '=' or an
        // equation-number "(N)". Reject so the equation stays in the text flow
        // (where it is reassembled as one block) instead of a spurious <table>. A
        // numeric data table has no operators and no '=', so it is unaffected.
        let opcell = |t: &str| t.chars().any(|c| "=+−–×÷·≤≥≠≈∝∫∑∏√∈∉∂∇→←↔⇒⇐↦∼≜≡∥⟨⟩".contains(c) || "αβγδεζηθικλμνξπρςστυϕφχψωΓΔΘΛΞΠΣΦΨΩ".contains(c));
        let op = grid.iter().flatten().filter(|c| opcell(c)).count();
        let has_eq = grid.iter().flatten().any(|c| c.contains('='));
        let eqnum = grid.iter().flatten().any(|c| {
            let t = c.trim();
            let inner: String = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or("").to_string();
            !inner.is_empty() && inner.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        });
        // Real (alphabetic, ≥2-letter) words — an equation has almost none; its
        // "words" are space-separated symbols. A data table has real words.
        let alpha_words = grid.iter().flatten().flat_map(|c| c.split(|ch: char| !ch.is_alphabetic())).filter(|w| w.chars().count() >= 3).count();
        // Reject an equation region: it carries an '=' or eq-number, OR it is
        // operator-dense (a relation/arrow chain), and it has almost no real words.
        if nz > 0 && alpha_words <= nz && ((op >= 1 && (has_eq || eqnum)) || op * 2 >= nz) {
            return;
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
            return;
        }

        let (mut x_left, mut x_right) = (f32::INFINITY, f32::NEG_INFINITY);
        for row in &owned {
            for c in row {
                x_left = x_left.min(c.x);
                x_right = x_right.max(c.end);
            }
        }
        // Now that the data table is ACCEPTED (past every prose/equation guard), attach
        // the grouped/multi-level HEADER rows the run-builder skipped — they don't form
        // uniform >=2-cell rows, so they were stranded above the data and leaked into the
        // prose. Map each header cell onto the SINGLE data-column grid: the data columns
        // its x-span covers become one cell with colspan = #covered (a label centred over
        // several columns merges); a cell over one column gets colspan 1; uncovered
        // columns become empty cells. Only rows horizontally overlapping the table count.
        let kept_x: Vec<f32> = keep.iter().map(|&k| cols[k]).collect();
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
            // Each data column k owns the x-region [anchor_k, anchor_{k+1}) (last → x_right).
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
