//! pylopdf — pure-Rust PDF extraction on lopdf, exposed to Python via PyO3.
//!
//! Phase 0: open a PDF, report page count, extract text.
//! Engine (lopdf) is confined to this boundary module; higher-level extraction
//! layers will be added above it (text spans, tables, images, fonts).

use lopdf::Document;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

mod text;

/// A loaded PDF document.
#[pyclass]
struct Pdf {
    doc: Document,
}

#[pymethods]
impl Pdf {
    /// Open a PDF from a filesystem path.
    #[staticmethod]
    fn open(path: &str) -> PyResult<Self> {
        let doc = Document::load(path).map_err(|e| PyValueError::new_err(format!("open failed: {e}")))?;
        Ok(Pdf { doc })
    }

    /// Open a PDF from raw bytes.
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        let doc =
            Document::load_mem(data).map_err(|e| PyValueError::new_err(format!("parse failed: {e}")))?;
        Ok(Pdf { doc })
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
                out.push_str(&text::extract_page(&self.doc, page_id).unwrap_or_default());
            }
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
        Ok(text::debug_page(&self.doc, page_id))
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
            text::extract_page(&self.doc, page_id).unwrap_or_default()
        })
    }
}

#[pymodule]
fn _pylopdf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Pdf>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
