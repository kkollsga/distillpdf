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
    spans.retain(|s| !s.text.trim().is_empty());
    if spans.is_empty() {
        return Vec::new();
    }
    let band = (spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32 * 0.6).max(2.0);
    spans.sort_by(|p, q| {
        let (bp, bq) = ((p.y / band).round(), (q.y / band).round());
        bq.partial_cmp(&bp)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(p.x.partial_cmp(&q.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut rows: Vec<Vec<Span>> = Vec::new();
    let mut cur_band: Option<f32> = None;
    for s in spans {
        let b = (s.y / band).round();
        if cur_band.map_or(true, |cb| (cb - b).abs() >= 0.5) {
            rows.push(Vec::new());
            cur_band = Some(b);
        }
        rows.last_mut().unwrap().push(s);
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
fn row_cells(row: &[Span]) -> Vec<Cell> {
    let mut cells: Vec<Cell> = Vec::new();
    for s in row {
        let txt = s.text.trim();
        if txt.is_empty() {
            continue;
        }
        let w = if s.width > 0.1 { s.width } else { txt.chars().count() as f32 * s.size * 0.5 };
        match cells.last_mut() {
            // gap small relative to font size -> same cell (continuous text)
            Some(prev) if s.x - prev.end < s.size * 1.3 => {
                prev.text.push(' ');
                prev.text.push_str(txt);
                prev.end = s.x + w;
            }
            _ => cells.push(Cell { x: s.x, end: s.x + w, text: txt.to_string() }),
        }
    }
    cells
}

/// Cluster cell x-positions into column anchors.
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

/// Detect tables: runs of >=3 consecutive rows that each have >=2 gutter-separated
/// cells and share >=2 columns occupied in a majority of rows. This rejects
/// word-positioned prose (whose words merge into a single cell).
/// A detected table with its vertical extent (PDF user space, y increases up).
pub struct PosTable {
    pub y_top: f32,
    pub y_bottom: f32,
    pub grid: Vec<Vec<String>>,
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
    }
}

/// Detect tables with vertical positions: runs of >=3 consecutive multi-cell rows
/// sharing >=2 aligned columns (occupied in a majority of rows). Rejects word-
/// positioned prose (whose words merge into a single cell).
pub fn detect_tables_pos(spans: &[Span]) -> Vec<PosTable> {
    let avg_size = if spans.is_empty() {
        10.0
    } else {
        spans.iter().map(|s| s.size).sum::<f32>() / spans.len() as f32
    };
    let tol = (avg_size * 1.5).max(6.0);
    let rows = rows_of(spans.iter().map(clone_span).collect());
    let celled: Vec<(f32, Vec<Cell>)> = rows
        .iter()
        .map(|r| (r.first().map(|s| s.y).unwrap_or(0.0), row_cells(r)))
        .collect();
    let mut tables = Vec::new();
    let mut run: Vec<&(f32, Vec<Cell>)> = Vec::new();

    let mut flush = |run: &Vec<&(f32, Vec<Cell>)>, tables: &mut Vec<PosTable>| {
        if run.len() < 3 {
            return;
        }
        let owned: Vec<Vec<Cell>> = run
            .iter()
            .map(|(_, c)| c.iter().map(|x| Cell { x: x.x, end: x.end, text: x.text.clone() }).collect())
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
        let keep: Vec<usize> = (0..cols.len()).filter(|&i| occ[i] * 2 >= owned.len()).collect();
        if keep.len() < 2 {
            return;
        }
        let grid: Vec<Vec<String>> = owned
            .iter()
            .map(|row| {
                let mut cells = vec![String::new(); keep.len()];
                for c in row {
                    if let Some(ci) = nearest_col(&cols, c.x) {
                        if let Some(k) = keep.iter().position(|&x| x == ci) {
                            if !cells[k].is_empty() {
                                cells[k].push(' ');
                            }
                            cells[k].push_str(&c.text);
                        }
                    }
                }
                cells
            })
            .collect();
        // Reject 2-column "tables" whose cells are long: that's two-column prose,
        // not a table (real table cells are short; prose columns are sentences).
        let (mut wc, mut nz) = (0usize, 0usize);
        for row in &grid {
            for c in row {
                let w = c.split_whitespace().count();
                if w > 0 {
                    wc += w;
                    nz += 1;
                }
            }
        }
        let mean_words = if nz > 0 { wc as f32 / nz as f32 } else { 0.0 };
        if grid.first().map(|r| r.len()).unwrap_or(0) <= 2 && mean_words > 8.0 {
            return;
        }

        tables.push(PosTable {
            y_top: run.first().map(|(y, _)| *y).unwrap_or(0.0),
            y_bottom: run.last().map(|(y, _)| *y).unwrap_or(0.0),
            grid,
        });
    };

    for row in &celled {
        if row.1.len() >= 2 {
            run.push(row);
        } else {
            flush(&run, &mut tables);
            run.clear();
        }
    }
    flush(&run, &mut tables);
    tables
}

fn detect_tables(spans: Vec<Span>) -> Vec<Vec<Vec<String>>> {
    detect_tables_pos(&spans).into_iter().map(|t| t.grid).collect()
}

fn nearest_col(cols: &[f32], x: f32) -> Option<usize> {
    cols.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (x - **a)
                .abs()
                .partial_cmp(&(x - **b).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
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
