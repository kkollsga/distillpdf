//! distillpdf — pure-Rust PDF extraction on lopdf, exposed to Python via PyO3.
//!
//! Phase 0: open a PDF, report page count, extract text.
//! Engine (lopdf) is confined to this boundary module; higher-level extraction
//! layers will be added above it (text spans, tables, images, fonts).

use lopdf::Document;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

mod extract;
mod html;
mod img;
mod links;
mod text;
mod vector;
use pyo3::types::{PyDict, PyList};

/// Parse the `mode` string accepted by `open`/`from_bytes` into an `html::Mode`.
fn parse_mode(mode: &str) -> PyResult<html::Mode> {
    match mode {
        "section" => Ok(html::Mode::Section),
        "page" => Ok(html::Mode::Page),
        other => Err(PyValueError::new_err(format!(
            "invalid mode {other:?}: expected \"section\" or \"page\""
        ))),
    }
}

/// A loaded PDF document.
#[pyclass]
struct Pdf {
    doc: Document,
    /// Raw PDF bytes, kept for lenient recovery of malformed streams.
    raw: Vec<u8>,
    /// `to_html()` output structure: section-first (default) or page-first.
    mode: html::Mode,
    /// Whether `to_html()` inlines raster images as base64 `<img>` data URIs. When
    /// false, each image becomes a lightweight `<image N>` placeholder instead.
    inline_images: bool,
    /// Whether `to_html()` prepends an auto `<nav>` table of contents. When false the
    /// TOC is omitted (heading anchors are still emitted, so links/`section()` work).
    include_toc: bool,
}

#[pymethods]
impl Pdf {
    /// Open a PDF from a filesystem path.
    ///
    /// `mode` chooses the `to_html()` structure: `"section"` (default) makes logical
    /// sections first-order — each heading becomes a nested `<section id="sec-…">` and
    /// page info is dropped; `"page"` wraps each page in `<section data-page>`.
    /// `images=False` replaces embedded raster images in `to_html()` with a
    /// `<image N>` placeholder (keeping captions and figure anchors) instead of
    /// inlining their base64 bytes — handy for compact, text-only LLM input.
    /// `toc=False` omits the auto table-of-contents `<nav>` from `to_html()` (heading
    /// anchors are still emitted, so links and `section()` keep working).
    #[staticmethod]
    #[pyo3(signature = (path, mode = "section", images = true, toc = true))]
    fn open(path: &str, mode: &str, images: bool, toc: bool) -> PyResult<Self> {
        let mode = parse_mode(mode)?;
        let raw = std::fs::read(path).map_err(|e| PyValueError::new_err(format!("read failed: {e}")))?;
        let doc =
            Document::load_mem(&raw).map_err(|e| PyValueError::new_err(format!("open failed: {e}")))?;
        Ok(Pdf { doc, raw, mode, inline_images: images, include_toc: toc })
    }

    /// Open a PDF from raw bytes. See `open` for the `mode`/`images`/`toc` flags.
    #[staticmethod]
    #[pyo3(signature = (data, mode = "section", images = true, toc = true))]
    fn from_bytes(data: &[u8], mode: &str, images: bool, toc: bool) -> PyResult<Self> {
        let mode = parse_mode(mode)?;
        let raw = data.to_vec();
        let doc =
            Document::load_mem(&raw).map_err(|e| PyValueError::new_err(format!("parse failed: {e}")))?;
        Ok(Pdf { doc, raw, mode, inline_images: images, include_toc: toc })
    }

    /// Number of pages.
    fn page_count(&self) -> usize {
        self.doc.get_pages().len()
    }

    /// Extract plain text from all pages (concatenated, page order).
    ///
    /// Hybrid: prefer our ToUnicode-aware content-stream extractor (handles CID
    /// fonts + diacritics); fall back to lopdf's extractor per page when ours
    /// yields little (so simple-encoded PDFs never regress).
    fn extract_text(&self) -> PyResult<String> {
        let pages = self.doc.get_pages();
        let mut out = String::new();
        for (&p, &page_id) in &pages {
            // Our position+width-aware extractor is primary (handles CID fonts,
            // accurate word boundaries, reading order). Fall back to lopdf only if
            // ours recovers nothing on a page.
            let mine = text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default();
            if mine.trim().chars().count() >= 2 {
                out.push_str(&mine);
            } else {
                out.push_str(&self.doc.extract_text(&[p]).unwrap_or_default());
            }
            out.push('\n');
        }
        Ok(out)
    }

    /// Extract images from all pages (list of dicts incl. raw bytes).
    fn extract_images<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        extract::extract_images(py, &self.doc)
    }

    /// Extract per-page font info (list of dicts).
    fn extract_fonts<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        extract::extract_fonts(py, &self.doc)
    }

    /// Extract tables from all pages (list of dicts with cell grids).
    fn extract_tables<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        extract::extract_tables(py, &self.doc, &self.raw)
    }

    /// Extract hyperlinks from all pages. Each dict:
    /// {page, rect:[x0,y0,x1,y1], kind:"uri"|"internal",
    ///  uri:str|None, dest_page:int|None, dest_name:str|None}.
    fn extract_links<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for lk in links::extract_links(&self.doc) {
            let d = PyDict::new(py);
            d.set_item("page", lk.page)?;
            d.set_item("rect", lk.rect.to_vec())?;
            d.set_item("kind", if lk.uri.is_some() { "uri" } else { "internal" })?;
            d.set_item("uri", lk.uri)?;
            d.set_item("dest_page", lk.dest_page)?;
            d.set_item("dest_name", lk.dest_name)?;
            list.append(d)?;
        }
        Ok(list)
    }

    /// Convert the PDF to thin, AI-ready HTML (per-page sections, headings,
    /// bold/italic, lists, tables, monospace; inline images added separately).
    ///
    /// The conversion (which internally renders pages in parallel) runs with the GIL
    /// released, so converting many PDFs across Python threads scales across cores.
    fn to_html(&self, py: Python<'_>) -> PyResult<String> {
        Ok(py.allow_threads(|| html::to_html(&self.doc, &self.raw, self.mode, self.inline_images, self.include_toc)))
    }

    /// Document outline: a list of `(level, title, page, anchor_id)` per heading, in
    /// reading order. `level` 1 is the title, 2 a section, 3 a subsection, … . The
    /// `anchor_id` matches an `id=` in `to_html()` (link with `#anchor_id`).
    fn toc(&self, py: Python<'_>) -> PyResult<Vec<(u8, String, u32, String)>> {
        // Force the TOC nav on regardless of `include_toc` — `html::toc` parses the
        // outline back out of that <nav>, so it must be present here even when the
        // user opted out of it in `to_html()`. Mode is honoured, so section mode yields
        // pageless entries.
        Ok(py.allow_threads(|| html::toc(&html::to_html(&self.doc, &self.raw, self.mode, self.inline_images, true))))
    }

    /// HTML of a single section: the heading matching `name` (its `sec-…` slug, an id
    /// prefix, or a case-insensitive title substring) plus its content up to the next
    /// same-or-higher heading. E.g. `section("abstract")`. None if no match.
    fn section(&self, py: Python<'_>, name: &str) -> PyResult<Option<String>> {
        // `html::section` resolves via the TOC nav, so build with it present.
        Ok(py.allow_threads(|| html::section(&html::to_html(&self.doc, &self.raw, self.mode, self.inline_images, true), name)))
    }

    /// Diagnostic: force our ToUnicode extractor for all pages (eval only).
    fn _mine_text(&self) -> PyResult<String> {
        let mut out = String::new();
        for (_p, &page_id) in &self.doc.get_pages() {
            out.push_str(&text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default());
            out.push('\n');
        }
        Ok(out)
    }

    /// Diagnostic: raw spans (text, x, width, size) for a 1-indexed page.
    fn _dbg_spans(&self, page: u32) -> PyResult<Vec<(String, f32, f32, f32)>> {
        let page_id = *self
            .doc
            .get_pages()
            .get(&page)
            .ok_or_else(|| PyValueError::new_err("no page"))?;
        Ok(text::extract_spans(&self.doc, page_id, &self.raw)
            .into_iter()
            .map(|s| (s.text, s.x, s.width, s.size))
            .collect())
    }

    /// Diagnostic: spans with y for a 1-indexed page (text, x, y, width, size).
    fn _dbg_spans_xy(&self, page: u32) -> PyResult<Vec<(String, f32, f32, f32, f32)>> {
        let page_id = *self.doc.get_pages().get(&page).ok_or_else(|| PyValueError::new_err("no page"))?;
        Ok(text::extract_spans(&self.doc, page_id, &self.raw)
            .into_iter()
            .map(|s| (s.text, s.x, s.y, s.width, s.size))
            .collect())
    }

    /// Diagnostic for one 1-indexed page.
    fn debug_page(&self, page: u32) -> PyResult<String> {
        let page_id = *self
            .doc
            .get_pages()
            .get(&page)
            .ok_or_else(|| PyValueError::new_err(format!("no page {page}")))?;
        Ok(text::debug_page(&self.doc, page_id, &self.raw))
    }

    /// Extract text from a single 1-indexed page (hybrid).
    fn extract_page_text(&self, page: u32) -> PyResult<String> {
        let page_id = *self
            .doc
            .get_pages()
            .get(&page)
            .ok_or_else(|| PyValueError::new_err(format!("no page {page}")))?;
        let mine = text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default();
        Ok(if mine.trim().chars().count() >= 2 {
            mine
        } else {
            self.doc.extract_text(&[page]).unwrap_or_default()
        })
    }
}

/// Open a PDF from a filesystem path — `distillpdf.open("file.pdf")`. A module-level
/// shorthand for `Pdf.open(...)`. `mode` selects the `to_html()` structure
/// (`"section"` default / `"page"`); `images=False` emits `<image N>` placeholders
/// instead of inline base64 images; `toc=False` omits the table-of-contents nav.
#[pyfunction]
#[pyo3(signature = (path, mode = "section", images = true, toc = true))]
fn open(path: &str, mode: &str, images: bool, toc: bool) -> PyResult<Pdf> {
    Pdf::open(path, mode, images, toc)
}

/// Open a PDF from raw bytes — `distillpdf.from_bytes(data)`. Shorthand for
/// `Pdf.from_bytes(...)`. See `open` for the `mode`/`images`/`toc` flags.
#[pyfunction]
#[pyo3(signature = (data, mode = "section", images = true, toc = true))]
fn from_bytes(data: &[u8], mode: &str, images: bool, toc: bool) -> PyResult<Pdf> {
    Pdf::from_bytes(data, mode, images, toc)
}

#[pymodule]
fn _distillpdf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Pdf>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_function(wrap_pyfunction!(from_bytes, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
