//! pylopdf — pure-Rust PDF extraction on lopdf, exposed to Python via PyO3.
//!
//! Phase 0: open a PDF, report page count, extract text.
//! Engine (lopdf) is confined to this boundary module; higher-level extraction
//! layers will be added above it (text spans, tables, images, fonts).

use lopdf::Document;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

mod extract;
mod text;
use pyo3::types::PyList;

/// A loaded PDF document.
#[pyclass]
struct Pdf {
    doc: Document,
    /// Raw PDF bytes, kept for lenient recovery of malformed streams.
    raw: Vec<u8>,
}

#[pymethods]
impl Pdf {
    /// Open a PDF from a filesystem path.
    #[staticmethod]
    fn open(path: &str) -> PyResult<Self> {
        let raw = std::fs::read(path).map_err(|e| PyValueError::new_err(format!("read failed: {e}")))?;
        let doc =
            Document::load_mem(&raw).map_err(|e| PyValueError::new_err(format!("open failed: {e}")))?;
        Ok(Pdf { doc, raw })
    }

    /// Open a PDF from raw bytes.
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        let raw = data.to_vec();
        let doc =
            Document::load_mem(&raw).map_err(|e| PyValueError::new_err(format!("parse failed: {e}")))?;
        Ok(Pdf { doc, raw })
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
            // lopdf's mature extractor is the default. Our ToUnicode extractor is
            // a *rescue* only when lopdf recovers nothing (the CID-font case), so
            // simple-encoded PDFs can never regress.
            let lopdf = self.doc.extract_text(&[p]).unwrap_or_default();
            if lopdf.trim().chars().count() >= 2 {
                out.push_str(&lopdf);
            } else {
                out.push_str(&text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default());
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

    /// Diagnostic: force our ToUnicode extractor for all pages (eval only).
    fn _mine_text(&self) -> PyResult<String> {
        let mut out = String::new();
        for (_p, &page_id) in &self.doc.get_pages() {
            out.push_str(&text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default());
            out.push('\n');
        }
        Ok(out)
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
        let lopdf = self.doc.extract_text(&[page]).unwrap_or_default();
        Ok(if lopdf.trim().chars().count() >= 2 {
            lopdf
        } else {
            text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default()
        })
    }
}

#[pymodule]
fn _pylopdf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Pdf>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
