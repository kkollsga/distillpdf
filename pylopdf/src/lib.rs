//! pylopdf — pure-Rust PDF extraction on lopdf, exposed to Python via PyO3.
//!
//! Phase 0: open a PDF, report page count, extract text.
//! Engine (lopdf) is confined to this boundary module; higher-level extraction
//! layers will be added above it (text spans, tables, images, fonts).

use lopdf::Document;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

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
    fn extract_text(&self) -> PyResult<String> {
        let pages: Vec<u32> = self.doc.get_pages().keys().copied().collect();
        let mut out = String::new();
        for p in pages {
            match self.doc.extract_text(&[p]) {
                Ok(t) => {
                    out.push_str(&t);
                    out.push('\n');
                }
                // Skip pages that fail rather than aborting the whole document.
                Err(_) => continue,
            }
        }
        Ok(out)
    }

    /// Extract text from a single 1-indexed page.
    fn extract_page_text(&self, page: u32) -> PyResult<String> {
        self.doc
            .extract_text(&[page])
            .map_err(|e| PyValueError::new_err(format!("extract failed: {e}")))
    }
}

#[pymodule]
fn _pylopdf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Pdf>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
