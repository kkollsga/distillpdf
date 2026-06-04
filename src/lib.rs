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
    /// Source path (`open`); `None` when constructed from bytes. Used by `export_html`
    /// to derive the default `<source>.html` output name.
    source: Option<std::path::PathBuf>,
}

impl Pdf {
    /// Render the HTML with the GIL released. Rendering options live on the render
    /// methods (not `open`), since `open` only parses the container — the heavy
    /// extraction happens here.
    fn render(&self, py: Python<'_>, mode: &str, images: bool, toc: bool) -> PyResult<String> {
        let mode = parse_mode(mode)?;
        Ok(py.allow_threads(|| html::to_html(&self.doc, &self.raw, mode, images, toc)))
    }

    /// Resolve the output path for `export_html`: an explicit file, `<stem>.html` inside
    /// an explicit directory, or `<source>.html` next to the opened PDF when omitted.
    fn resolve_html_path(&self, path: Option<&str>) -> PyResult<std::path::PathBuf> {
        match path {
            // A directory → write <source-stem>.html inside it.
            Some(p) if std::path::Path::new(p).is_dir() => {
                let stem = self
                    .source
                    .as_ref()
                    .and_then(|s| s.file_stem())
                    .ok_or_else(|| PyValueError::new_err("export_html: a directory path needs a source filename to derive the name; pass a full file path"))?;
                Ok(std::path::Path::new(p).join(stem).with_extension("html"))
            }
            Some(p) => Ok(std::path::PathBuf::from(p)),
            // No path → <source>.html next to the opened PDF.
            None => self
                .source
                .as_ref()
                .map(|s| s.with_extension("html"))
                .ok_or_else(|| PyValueError::new_err("export_html: no source path (opened from_bytes); pass an explicit path")),
        }
    }
}

#[pymethods]
impl Pdf {
    /// Open a PDF from a filesystem path. This only loads and parses the PDF container;
    /// the actual extraction/render happens in `to_html()` / `export_html()`, which is
    /// where the rendering options (`mode`/`images`/`toc`) live.
    #[staticmethod]
    fn open(path: &str) -> PyResult<Self> {
        let raw = std::fs::read(path).map_err(|e| PyValueError::new_err(format!("read failed: {e}")))?;
        let doc =
            Document::load_mem(&raw).map_err(|e| PyValueError::new_err(format!("open failed: {e}")))?;
        Ok(Pdf { doc, raw, source: Some(std::path::PathBuf::from(path)) })
    }

    /// Open a PDF from raw bytes (no source path, so `export_html` needs an explicit path).
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        let raw = data.to_vec();
        let doc =
            Document::load_mem(&raw).map_err(|e| PyValueError::new_err(format!("parse failed: {e}")))?;
        Ok(Pdf { doc, raw, source: None })
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

    /// Convert the PDF to thin, AI-ready HTML and return it as a string.
    ///
    /// `mode` (`"section"` default / `"page"`) chooses the structure; `images=False`
    /// emits `<image N>` placeholders instead of inline base64; `toc=False` drops the
    /// `<nav>` table of contents. To write straight to a file, use `export_html()`.
    ///
    /// The conversion (which internally renders pages in parallel) runs with the GIL
    /// released, so converting many PDFs across Python threads scales across cores.
    #[pyo3(signature = (mode="section", images=true, toc=true))]
    fn to_html(&self, py: Python<'_>, mode: &str, images: bool, toc: bool) -> PyResult<String> {
        self.render(py, mode, images, toc)
    }

    /// Render the HTML and write it to a file, returning the path written.
    ///
    /// With no `path`, writes `<source>.html` next to the opened PDF
    /// (`open("a/b.pdf").export_html()` → `a/b.html`). If `path` is a directory, writes
    /// `<source-stem>.html` inside it; otherwise `path` is used verbatim. `mode`/`images`/
    /// `toc` work exactly as in `to_html()`. A bytes-constructed `Pdf` (no source path)
    /// requires an explicit `path`.
    #[pyo3(signature = (path=None, mode="section", images=true, toc=true))]
    fn export_html(&self, py: Python<'_>, path: Option<&str>, mode: &str, images: bool, toc: bool) -> PyResult<String> {
        let dest = self.resolve_html_path(path)?;
        let html = self.render(py, mode, images, toc)?;
        std::fs::write(&dest, html).map_err(|e| PyValueError::new_err(format!("write failed: {e}")))?;
        Ok(dest.to_string_lossy().into_owned())
    }

    /// Document outline: a list of `(level, title, page, anchor_id)` per heading, in
    /// reading order. `level` 1 is the title, 2 a section, 3 a subsection, … . The
    /// `anchor_id` matches an `id=` in `to_html()` (link with `#anchor_id`). `mode`
    /// matches `to_html()`: `"page"` carries real page numbers, `"section"` yields 0.
    #[pyo3(signature = (mode="section"))]
    fn toc(&self, py: Python<'_>, mode: &str) -> PyResult<Vec<(u8, String, u32, String)>> {
        let mode = parse_mode(mode)?;
        // Force the TOC nav on (and skip image encoding — irrelevant to the outline) —
        // `html::toc` parses the outline back out of that <nav>.
        Ok(py.allow_threads(|| html::toc(&html::to_html(&self.doc, &self.raw, mode, false, true))))
    }

    /// The PDF's OWN table of contents — the author-supplied `/Outlines` bookmarks —
    /// as `(level, title, page, anchor)` tuples in reading order. `level` is 1-based
    /// nesting depth; `page` is the 1-indexed target page (0 if unresolved); `anchor` is
    /// the `#page-N` fragment `to_html(mode="page")` exposes. Empty list when the PDF has
    /// no outline. This is distinct from `toc()`, which is built from detected headings;
    /// when an outline is present, `to_html()` also uses it for the rendered `<nav>`.
    fn outline(&self, py: Python<'_>) -> PyResult<Vec<(u8, String, u32, String)>> {
        Ok(py.allow_threads(|| {
            links::outline(&self.doc)
                .into_iter()
                .map(|e| ((e.level + 1).min(255), e.title, e.page, format!("page-{}", e.page)))
                .collect()
        }))
    }

    /// HTML of a single section: the heading matching `name` (its `sec-…` slug, an id
    /// prefix, or a case-insensitive title substring) plus its content up to the next
    /// same-or-higher heading. E.g. `section("abstract")`. None if no match. `mode` and
    /// `images` match `to_html()`.
    #[pyo3(signature = (name, mode="section", images=true))]
    fn section(&self, py: Python<'_>, name: &str, mode: &str, images: bool) -> PyResult<Option<String>> {
        let mode = parse_mode(mode)?;
        // `html::section` resolves via the TOC nav, so build with it present.
        Ok(py.allow_threads(|| html::section(&html::to_html(&self.doc, &self.raw, mode, images, true), name)))
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
/// shorthand for `Pdf.open(...)`. Rendering options live on `to_html()`/`export_html()`.
#[pyfunction]
fn open(path: &str) -> PyResult<Pdf> {
    Pdf::open(path)
}

/// Open a PDF from raw bytes — `distillpdf.from_bytes(data)`. Shorthand for
/// `Pdf.from_bytes(...)`.
#[pyfunction]
fn from_bytes(data: &[u8]) -> PyResult<Pdf> {
    Pdf::from_bytes(data)
}

#[pymodule]
fn _distillpdf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Pdf>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_function(wrap_pyfunction!(from_bytes, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
