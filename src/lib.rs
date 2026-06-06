//! distillpdf — pure-Rust PDF extraction on lopdf, exposed to Python via PyO3.
//!
//! Phase 0: open a PDF, report page count, extract text.
//! Engine (lopdf) is confined to this boundary module; higher-level extraction
//! layers will be added above it (text spans, tables, images, fonts).

use lopdf::Document;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

mod afm;
mod captions;
mod extract;
mod frontmatter;
mod html;
mod img;
mod links;
mod markdown;
mod nav;
mod postprocess;
mod profile;
mod text;
mod vector;

/// Maximum Form-XObject / content-stream recursion depth. Bounds runaway recursion and
/// cyclic Form references while allowing legitimately deep nesting.
pub(crate) const MAX_FORM_DEPTH: u32 = 40;

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

/// Parse the `image_mode` string into a render strategy:
/// * `"embed"` → inline base64 `data:` URIs (self-contained).
/// * `"external"` → extract figures to an `img/` folder; only possible when writing to a
///   file, so a returned string falls back to `string_fallback` (HTML uses `Embed` to stay
///   self-contained; Markdown uses `Placeholder`, since inline data URIs are impractical).
/// * `"drop"` → replace images with placeholder text.
/// The success sentinel returned by the file-writing methods: Python `int` 1.
fn ok_one(py: Python<'_>) -> PyResult<Py<PyAny>> {
    use pyo3::IntoPyObject;
    Ok(1i64.into_pyobject(py).unwrap().into_any().unbind())
}

fn parse_image_mode(s: &str, writing: bool, string_fallback: markdown::ImgMode) -> PyResult<markdown::ImgMode> {
    match s {
        "embed" => Ok(markdown::ImgMode::Embed),
        "drop" => Ok(markdown::ImgMode::Placeholder),
        "external" => Ok(if writing { markdown::ImgMode::Files } else { string_fallback }),
        other => Err(PyValueError::new_err(format!(
            "invalid image_mode {other:?}: expected \"embed\", \"external\", or \"drop\""
        ))),
    }
}

/// A loaded PDF document.
#[pyclass]
struct Pdf {
    doc: Document,
    /// Raw PDF bytes, kept for lenient recovery of malformed streams.
    raw: Vec<u8>,
    /// Source path (`open`); `None` when constructed from bytes. Used to derive the
    /// default `<source>.html` / `<source>.md` output name when `outputfile=True`.
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

    /// Resolve where rendered output is written, for the given default extension
    /// (`"html"` / `"md"`): an explicit file path verbatim, `<source-stem>.<ext>` inside an
    /// explicit directory, or `<source>.<ext>` next to the opened PDF when no path is given
    /// (the `outputfile=True` convenience — `text.pdf` → `text.<ext>`).
    fn resolve_out_path(&self, path: Option<&str>, ext: &str) -> PyResult<std::path::PathBuf> {
        match path {
            // A directory → write <source-stem>.<ext> inside it.
            Some(p) if std::path::Path::new(p).is_dir() => {
                let stem = self
                    .source
                    .as_ref()
                    .and_then(|s| s.file_stem())
                    .ok_or_else(|| PyValueError::new_err("a directory path needs a source filename to derive the name; pass a full file path"))?;
                Ok(std::path::Path::new(p).join(stem).with_extension(ext))
            }
            Some(p) => Ok(std::path::PathBuf::from(p)),
            // No path → <source>.<ext> next to the opened PDF.
            None => self
                .source
                .as_ref()
                .map(|s| s.with_extension(ext))
                .ok_or_else(|| PyValueError::new_err("no source path (opened from_bytes); pass an explicit path")),
        }
    }

    /// Write the document `content` to `dest` plus any extracted figure files (relative
    /// paths, e.g. `img/fig_01_x.png`) under `dest`'s directory. Returns the dest path.
    fn write_doc(&self, dest: std::path::PathBuf, content: &str, files: &[markdown::ImageFile]) -> PyResult<String> {
        // Create the destination directory if the caller pointed at a not-yet-existing folder.
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| PyValueError::new_err(format!("mkdir failed: {e}")))?;
        }
        std::fs::write(&dest, content).map_err(|e| PyValueError::new_err(format!("write failed: {e}")))?;
        if !files.is_empty() {
            let dir = dest.parent().unwrap_or_else(|| std::path::Path::new("."));
            for f in files {
                let fp = dir.join(&f.path);
                if let Some(parent) = fp.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| PyValueError::new_err(format!("mkdir failed: {e}")))?;
                }
                std::fs::write(&fp, &f.bytes).map_err(|e| PyValueError::new_err(format!("write failed: {e}")))?;
            }
        }
        Ok(dest.to_string_lossy().into_owned())
    }
}

#[pymethods]
impl Pdf {
    /// Open a PDF from a filesystem path. This only loads and parses the PDF container;
    /// the actual extraction/render happens in `to_html()` / `to_markdown()`, which is
    /// where the rendering options (`mode`/`images`/`toc`) live.
    #[staticmethod]
    fn open(path: &str) -> PyResult<Self> {
        let raw = std::fs::read(path).map_err(|e| PyValueError::new_err(format!("read failed: {e}")))?;
        let doc =
            Document::load_mem(&raw).map_err(|e| PyValueError::new_err(format!("open failed: {e}")))?;
        Ok(Pdf { doc, raw, source: Some(std::path::PathBuf::from(path)) })
    }

    /// Open a PDF from raw bytes. There is no source path, so writing output with
    /// `outputfile=True` (no `path`) is an error — pass an explicit `path` instead.
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

    /// Convert the PDF to thin, AI-ready HTML.
    ///
    /// By default this **writes a file and returns `1`** on success — `path` if given
    /// (a file, or a directory to place `<source-stem>.html` in), otherwise `<source>.html`
    /// next to the opened PDF (`text.pdf` → `text.html`). Set `return_string=True` to get
    /// the HTML back as a string instead (and write nothing).
    ///
    /// `mode` (`"section"` default / `"page"`) chooses the structure; `toc=False` drops the
    /// `<nav>` table of contents.
    ///
    /// `image_mode` controls figures:
    /// * `"embed"` (default) → inline base64 `data:` URIs — a single self-contained file (or
    ///   string).
    /// * `"external"` → extract figures to an `img/` folder next to the HTML
    ///   (`img/fig_NN_slug.ext`, vector figures as `.svg`) and reference them — small HTML,
    ///   same `img/` layout as `to_markdown()`. (A returned string has no folder to write
    ///   into, so it falls back to `"embed"`.)
    /// * `"drop"` → `<image N>` placeholders, no image bytes.
    ///
    /// The conversion (which internally renders pages in parallel) runs with the GIL
    /// released, so converting many PDFs across Python threads scales across cores.
    #[pyo3(signature = (path=None, return_string=false, mode="section", toc=true, image_mode="embed"))]
    fn to_html(&self, py: Python<'_>, path: Option<&str>, return_string: bool, mode: &str, toc: bool, image_mode: &str) -> PyResult<Py<PyAny>> {
        // Writing to disk is the default; `return_string=True` returns the HTML instead.
        let im = parse_image_mode(image_mode, !return_string, markdown::ImgMode::Embed)?;
        // Placeholder renders `<image N>`; embed/external need the real image bytes.
        let html = self.render(py, mode, !matches!(im, markdown::ImgMode::Placeholder), toc)?;
        if return_string {
            return Ok(pyo3::types::PyString::new(py, &html).into_any().unbind());
        }
        let dest = self.resolve_out_path(path, "html")?;
        if matches!(im, markdown::ImgMode::Files) {
            // Extract figures to img/ next to the file.
            let (html, files) = py.allow_threads(|| markdown::externalize_images(&html));
            self.write_doc(dest, &html, &files)?;
        } else {
            self.write_doc(dest, &html, &[])?;
        }
        ok_one(py)
    }

    /// Convert the PDF to clean Markdown.
    ///
    /// Markdown is produced by transforming the same HTML `to_html()` emits, so every
    /// processor improvement flows in automatically — there is no separate Markdown
    /// renderer to maintain.
    ///
    /// File output works exactly like `to_html()`: by default it **writes** `<source>.md`
    /// (or `path`) and returns `1`; `return_string=True` returns the Markdown string.
    /// `mode`/`toc` match `to_html()`.
    ///
    /// `image_mode` controls figures (same values as `to_html()`, but defaulting to
    /// `"external"` — inline `data:` URIs are impractical in Markdown):
    /// * `"external"` (default) → extract figures to an `img/` folder next to the `.md`
    ///   (`img/fig_NN_slug.ext`) and reference them; a returned string (no folder) falls
    ///   back to caption-only placeholders.
    /// * `"embed"` → inline `data:` URIs.
    /// * `"drop"` → caption-only placeholders.
    #[pyo3(signature = (path=None, return_string=false, mode="section", toc=true, image_mode="external"))]
    fn to_markdown(&self, py: Python<'_>, path: Option<&str>, return_string: bool, mode: &str, toc: bool, image_mode: &str) -> PyResult<Py<PyAny>> {
        // Markdown string output can't externalise and shouldn't inline, so it drops to
        // placeholders.
        let im = parse_image_mode(image_mode, !return_string, markdown::ImgMode::Placeholder)?;
        let need_bytes = matches!(im, markdown::ImgMode::Embed | markdown::ImgMode::Files);
        let html = self.render(py, mode, need_bytes, toc)?;
        let (md, files) = py.allow_threads(|| markdown::html_to_markdown(&html, toc, im));

        if return_string {
            return Ok(pyo3::types::PyString::new(py, &md).into_any().unbind());
        }
        let dest = self.resolve_out_path(path, "md")?;
        self.write_doc(dest, &md, &files)?;
        ok_one(py)
    }

    /// Document outline: a list of `(level, title, page, anchor_id)` per heading, in
    /// reading order. `level` 1 is the title, 2 a section, 3 a subsection, … . The
    /// `anchor_id` matches an `id=` in `to_html()` (link with `#anchor_id`). `mode`
    /// matches `to_html()`: `"page"` carries real page numbers, `"section"` yields 0.
    #[pyo3(signature = (mode="section"))]
    fn toc(&self, py: Python<'_>, mode: &str) -> PyResult<Vec<(u8, String, u32, String)>> {
        let mode = parse_mode(mode)?;
        // Force the TOC nav on (and skip image encoding — irrelevant to the outline) —
        // `nav::toc` parses the outline back out of that <nav>.
        Ok(py.allow_threads(|| nav::toc(&html::to_html(&self.doc, &self.raw, mode, false, true))))
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
                .map(|e| ((e.level + 1), e.title, e.page, format!("page-{}", e.page)))
                .collect()
        }))
    }

    /// HTML of a single section: the heading matching `name` (its `sec-…` slug, an id
    /// prefix, or a case-insensitive title substring) plus its content up to the next
    /// same-or-higher heading. E.g. `section("abstract")`. None if no match. `mode` and
    /// `image_mode` match `to_html()` (the result is a string, so `"external"` behaves like
    /// `"embed"`).
    #[pyo3(signature = (name, mode="section", image_mode="embed"))]
    fn section(&self, py: Python<'_>, name: &str, mode: &str, image_mode: &str) -> PyResult<Option<String>> {
        let mode = parse_mode(mode)?;
        let images = !matches!(parse_image_mode(image_mode, false, markdown::ImgMode::Embed)?, markdown::ImgMode::Placeholder);
        // `nav::section` resolves via the TOC nav, so build with it present.
        Ok(py.allow_threads(|| nav::section(&html::to_html(&self.doc, &self.raw, mode, images, true), name)))
    }

    /// Structured front-matter of an academic paper, parsed from page 1:
    /// `{title:str, authors:[{name:str, affiliation:str|None}], abstract:str|None,
    /// keywords:[str]}`. Fields are empty/None when not detected. Authors are linked to
    /// their organisation via the affiliation superscript markers.
    fn metadata<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let fm = py.allow_threads(|| html::extract_front_matter(&self.doc, &self.raw));
        let d = PyDict::new(py);
        d.set_item("title", fm.title)?;
        let authors = PyList::empty(py);
        for a in fm.authors {
            let ad = PyDict::new(py);
            ad.set_item("name", a.name)?;
            ad.set_item("affiliation", a.affiliation)?;
            authors.append(ad)?;
        }
        d.set_item("authors", authors)?;
        d.set_item("affiliations", fm.affiliations)?;
        d.set_item("abstract", fm.abstract_text)?;
        d.set_item("keywords", fm.keywords)?;
        Ok(d)
    }

    /// Diagnostic: force our ToUnicode extractor for all pages (eval only).
    fn _mine_text(&self) -> PyResult<String> {
        let mut out = String::new();
        for &page_id in self.doc.get_pages().values() {
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
/// shorthand for `Pdf.open(...)`. Rendering options live on `to_html()`/`to_markdown()`.
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
