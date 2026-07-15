//! distillpdf — pure-Rust PDF extraction on lopdf, exposed to Python via PyO3.
//!
//! Phase 0: open a PDF, report page count, extract text.
//! Engine (lopdf) is confined to this boundary module; higher-level extraction
//! layers will be added above it (text spans, tables, images, fonts).

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

mod afm;
mod captions;
mod doc;
mod elem_passes;
mod error;
mod extract;
mod frontmatter;
mod headings;
mod html;
mod img;
mod layout;
mod links;
mod markdown;
mod model;
mod nav;
mod ocr;
mod postprocess;
mod profile;
mod text;
mod vector;

use doc::PdfDocument;
use error::Error;

/// Maximum Form-XObject / content-stream recursion depth. Bounds runaway recursion and
/// cyclic Form references while allowing legitimately deep nesting.
pub(crate) const MAX_FORM_DEPTH: u32 = 40;

use pyo3::types::{PyDict, PyList};

/// Map a core [`Error`] to the `ValueError` the Python API has always raised. `Display` on
/// `Error` reproduces the exact message strings, so pytest assertions stay green.
fn to_py(e: Error) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// The success sentinel returned by the file-writing methods: Python `int` 1.
fn ok_one(py: Python<'_>) -> PyResult<Py<PyAny>> {
    use pyo3::IntoPyObject;
    Ok(1i64.into_pyobject(py).unwrap().into_any().unbind())
}

/// A loaded PDF document — a thin PyO3 wrapper over the pure-Rust [`PdfDocument`] core.
#[pyclass]
struct Pdf {
    inner: PdfDocument,
}

#[pymethods]
impl Pdf {
    /// Open a PDF from a filesystem path. This only loads and parses the PDF container;
    /// the actual extraction/render happens in `to_html()` / `to_markdown()`, which is
    /// where the rendering options (`mode`/`images`/`toc`) live.
    #[staticmethod]
    fn open(path: &str) -> PyResult<Self> {
        Ok(Pdf { inner: PdfDocument::open(path).map_err(to_py)? })
    }

    /// Open a PDF from raw bytes. There is no source path, so writing output with
    /// `outputfile=True` (no `path`) is an error — pass an explicit `path` instead.
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        Ok(Pdf { inner: PdfDocument::from_bytes(data).map_err(to_py)? })
    }

    /// Number of pages.
    fn page_count(&self) -> usize {
        self.inner.page_count()
    }

    /// Extract plain text from all pages (concatenated, page order).
    ///
    /// Hybrid: prefer our ToUnicode-aware content-stream extractor (handles CID
    /// fonts + diacritics); fall back to lopdf's extractor per page when ours
    /// yields little (so simple-encoded PDFs never regress).
    fn extract_text(&self) -> PyResult<String> {
        Ok(self.inner.extract_text())
    }

    /// Extract images from all pages (list of dicts incl. raw bytes).
    fn extract_images<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for im in self.inner.extract_images() {
            let d = PyDict::new(py);
            d.set_item("page", im.page)?;
            d.set_item("index", im.index)?;
            d.set_item("width", im.width)?;
            d.set_item("height", im.height)?;
            d.set_item("color_space", im.color_space)?;
            d.set_item("format", im.format)?;
            d.set_item("data", pyo3::types::PyBytes::new(py, &im.data))?;
            list.append(d)?;
        }
        Ok(list)
    }

    /// Extract per-page font info (list of dicts).
    fn extract_fonts<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for fi in self.inner.extract_fonts() {
            let d = PyDict::new(py);
            d.set_item("page", fi.page)?;
            d.set_item("name", fi.name)?;
            d.set_item("subtype", fi.subtype)?;
            d.set_item("base_font", fi.base_font)?;
            d.set_item("encoding", fi.encoding)?;
            d.set_item("embedded", fi.embedded)?;
            d.set_item("has_tounicode", fi.has_tounicode)?;
            list.append(d)?;
        }
        Ok(list)
    }

    /// Extract tables from all pages (list of dicts with cell grids).
    fn extract_tables<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for t in self.inner.extract_tables() {
            let d = PyDict::new(py);
            d.set_item("page", t.page)?;
            d.set_item("n_rows", t.cells.len())?;
            d.set_item("n_cols", t.cells.first().map(|r| r.len()).unwrap_or(0))?;
            d.set_item("cells", t.cells)?;
            list.append(d)?;
        }
        Ok(list)
    }

    /// Extract hyperlinks from all pages. Each dict:
    /// {page, rect:[x0,y0,x1,y1], kind:"uri"|"internal",
    ///  uri:str|None, dest_page:int|None, dest_name:str|None}.
    fn extract_links<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for lk in self.inner.extract_links() {
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
        let im = doc::parse_image_mode(image_mode, !return_string, markdown::ImgMode::Embed).map_err(to_py)?;
        let mode = doc::parse_mode(mode).map_err(to_py)?;
        // Placeholder renders `<image N>`; embed/external need the real image bytes.
        let images = !matches!(im, markdown::ImgMode::Placeholder);
        let html = py.allow_threads(|| self.inner.render(mode, images, toc));
        if return_string {
            return Ok(pyo3::types::PyString::new(py, &html).into_any().unbind());
        }
        let dest = self.inner.resolve_out_path(path, "html").map_err(to_py)?;
        if matches!(im, markdown::ImgMode::Files) {
            // Extract figures to img/ next to the file.
            let (html, files) = py.allow_threads(|| markdown::externalize_images(&html));
            self.inner.write_doc(dest, &html, &files).map_err(to_py)?;
        } else {
            self.inner.write_doc(dest, &html, &[]).map_err(to_py)?;
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
        let im = doc::parse_image_mode(image_mode, !return_string, markdown::ImgMode::Placeholder).map_err(to_py)?;
        let mode = doc::parse_mode(mode).map_err(to_py)?;
        let need_bytes = matches!(im, markdown::ImgMode::Embed | markdown::ImgMode::Files);
        let html = py.allow_threads(|| self.inner.render(mode, need_bytes, toc));
        let (md, files) = py.allow_threads(|| markdown::html_to_markdown(&html, toc, im));

        if return_string {
            return Ok(pyo3::types::PyString::new(py, &md).into_any().unbind());
        }
        let dest = self.inner.resolve_out_path(path, "md").map_err(to_py)?;
        self.inner.write_doc(dest, &md, &files).map_err(to_py)?;
        ok_one(py)
    }

    /// Document outline: a list of `(level, title, page, anchor_id)` per heading, in
    /// reading order. `level` 1 is the title, 2 a section, 3 a subsection, … . The
    /// `anchor_id` matches an `id=` in `to_html()` (link with `#anchor_id`). `mode`
    /// matches `to_html()`: `"page"` carries real page numbers, `"section"` yields 0.
    #[pyo3(signature = (mode="section"))]
    fn toc(&self, py: Python<'_>, mode: &str) -> PyResult<Vec<(u8, String, u32, String)>> {
        let mode = doc::parse_mode(mode).map_err(to_py)?;
        // Force the TOC nav on (and skip image encoding — irrelevant to the outline) —
        // `nav::toc` parses the outline back out of that <nav>.
        Ok(py.allow_threads(|| self.inner.toc(mode)))
    }

    /// The PDF's OWN table of contents — the author-supplied `/Outlines` bookmarks —
    /// as `(level, title, page, anchor)` tuples in reading order. `level` is 1-based
    /// nesting depth; `page` is the 1-indexed target page (0 if unresolved); `anchor` is
    /// the `#page-N` fragment `to_html(mode="page")` exposes. Empty list when the PDF has
    /// no outline. This is distinct from `toc()`, which is built from detected headings;
    /// when an outline is present, `to_html()` also uses it for the rendered `<nav>`.
    fn outline(&self, py: Python<'_>) -> PyResult<Vec<(u8, String, u32, String)>> {
        Ok(py.allow_threads(|| self.inner.outline()))
    }

    /// HTML of a single section: the heading matching `name` (its `sec-…` slug, an id
    /// prefix, or a case-insensitive title substring) plus its content up to the next
    /// same-or-higher heading. E.g. `section("abstract")`. None if no match. `mode` and
    /// `image_mode` match `to_html()` (the result is a string, so `"external"` behaves like
    /// `"embed"`).
    #[pyo3(signature = (name, mode="section", image_mode="embed"))]
    fn section(&self, py: Python<'_>, name: &str, mode: &str, image_mode: &str) -> PyResult<Option<String>> {
        let mode = doc::parse_mode(mode).map_err(to_py)?;
        let images = !matches!(doc::parse_image_mode(image_mode, false, markdown::ImgMode::Embed).map_err(to_py)?, markdown::ImgMode::Placeholder);
        // `nav::section` resolves via the TOC nav, so build with it present.
        Ok(py.allow_threads(|| self.inner.section(mode, name, images)))
    }

    /// Structured front-matter of an academic paper, parsed from page 1:
    /// `{title:str, authors:[{name:str, affiliation:str|None}], abstract:str|None,
    /// keywords:[str]}`. Fields are empty/None when not detected. Authors are linked to
    /// their organisation via the affiliation superscript markers.
    fn metadata<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let fm = py.allow_threads(|| self.inner.front_matter());
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

    /// OCR plan: per page, whether OCR is needed and (if so) the page's main raster as
    /// standard image bytes for a backend to read. Each dict:
    /// {page, needs_ocr:bool, reason:str, width_pts, height_pts, image:bytes|None}.
    /// Drives the `distillpdf.ocr` orchestrators (the model runs in the optional [ocr] extra).
    fn ocr_plan<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for e in self.inner.ocr_plan() {
            let d = PyDict::new(py);
            d.set_item("page", e.page)?;
            d.set_item("needs_ocr", e.needs_ocr)?;
            d.set_item("reason", e.reason)?;
            d.set_item("width_pts", e.width_pts)?;
            d.set_item("height_pts", e.height_pts)?;
            match e.image {
                Some(b) => d.set_item("image", pyo3::types::PyBytes::new(py, &b))?,
                None => d.set_item("image", py.None())?,
            }
            list.append(d)?;
        }
        Ok(list)
    }

    /// Store OCR results on this object: `ocr` is a `{1-based page: DocTags}` map produced
    /// by one model pass. Once set, `to_pdf` (and the `distillpdf.ocr` HTML/Markdown
    /// orchestrators) reuse it, so the model runs **once** regardless of how many outputs
    /// are produced. Merges into any existing cache. Returns the cached page count.
    fn set_ocr(&self, ocr: std::collections::HashMap<u32, String>) -> PyResult<usize> {
        self.inner.set_ocr(ocr).map_err(to_py)
    }

    /// The cached OCR results (`{page: DocTags}`), empty if `set_ocr` was never called.
    fn get_ocr(&self) -> PyResult<std::collections::HashMap<u32, String>> {
        self.inner.get_ocr().map_err(to_py)
    }

    /// True if OCR results have been cached on this object (a model pass already ran).
    fn has_ocr(&self) -> PyResult<bool> {
        self.inner.has_ocr().map_err(to_py)
    }

    /// Write a searchable PDF from the OCR results (`ocr`, a `{1-based page: DocTags}` map;
    /// when omitted the results cached on this object via `set_ocr` are used).
    ///
    /// Two modes, controlled by `remove_raster`:
    /// * `False` (default) — **keep the original scan** and add the OCR text as an INVISIBLE
    ///   (selectable/searchable) layer over it. The scan stays exactly as-is, so OCR errors
    ///   never destroy content — the safe choice for archival/legal use.
    /// * `True` — **clean reflow**: rebuild OCR'd pages as real visible text + cropped figure
    ///   regions and drop the page raster (a much smaller file; makes
    ///   `to_html(in) ≈ to_html(to_pdf(in))` hold). OCR errors are then the only text.
    ///
    /// Non-OCR'd pages are kept verbatim either way. Returns `1`.
    #[pyo3(signature = (path, ocr=None, remove_raster=false))]
    fn to_pdf(&self, py: Python<'_>, path: &str, ocr: Option<std::collections::HashMap<u32, String>>, remove_raster: bool) -> PyResult<Py<PyAny>> {
        let ocr = match ocr {
            Some(m) => m,
            None => self.inner.get_ocr().map_err(to_py)?,
        };
        let buf = py.allow_threads(|| self.inner.build_searchable_pdf(&ocr, remove_raster)).map_err(to_py)?;
        std::fs::write(path, buf).map_err(|e| to_py(Error::Write(e)))?;
        ok_one(py)
    }

    /// Distill the document into a `.dpdf` container (the durable analysis model) — the
    /// engine-track artifact: a zip of `model.json` (the typed element tree: pages, the
    /// section tree, blocks in reading order, tables, figures, links, indexes) plus `img/`
    /// assets. Re-render HTML / Markdown / text from the file later, in milliseconds, instead
    /// of re-paying the full analysis cost.
    ///
    /// `path` chooses where to write: an explicit `*.dpdf` file, a directory (→
    /// `<source-stem>.dpdf` inside it), or `None` to write `<source>.dpdf` next to the opened
    /// PDF. Returns the written path.
    ///
    /// `assets` chooses the asset save profile (size is a deliberate choice, never a surprise):
    /// * `"figures"` (default) — embed figure image bytes (hash + dimensions filled); page
    ///   rasters stay dropped-with-stub (regenerable).
    /// * `"full"` — figures and (eventually) page rasters; equals `"figures"` on the
    ///   born-digital path until page-raster capture lands.
    /// * `"none"` — text + structure only; all asset bytes dropped, the regenerable stubs kept
    ///   (a few MB even for a large scan; emailable).
    ///
    /// **Experimental (`schema_version = 0`).** A dropped asset always keeps a stub (hash/dims/
    /// regen) — a named, reversible hole, re-extractable from the hash-bound source PDF. OCR
    /// passes and per-block bboxes are filled by later waves.
    #[pyo3(signature = (path=None, assets="figures"))]
    fn distill(&self, py: Python<'_>, path: Option<&str>, assets: &str) -> PyResult<String> {
        let opts = doc::DistillOptions::from_assets(assets).map_err(to_py)?;
        py.allow_threads(|| self.inner.distill(path, &opts)).map_err(to_py)
    }

    /// Diagnostic: force our ToUnicode extractor for all pages (eval only).
    fn _mine_text(&self) -> PyResult<String> {
        Ok(self.inner.mine_text())
    }

    /// Diagnostic: raw spans (text, x, width, size) for a 1-indexed page.
    fn _dbg_spans(&self, page: u32) -> PyResult<Vec<(String, f32, f32, f32)>> {
        self.inner.dbg_spans(page).map_err(to_py)
    }

    /// Diagnostic: spans with y for a 1-indexed page (text, x, y, width, size).
    fn _dbg_spans_xy(&self, page: u32) -> PyResult<Vec<(String, f32, f32, f32, f32)>> {
        self.inner.dbg_spans_xy(page).map_err(to_py)
    }

    /// Diagnostic for one 1-indexed page.
    fn debug_page(&self, page: u32) -> PyResult<String> {
        self.inner.debug_page(page).map_err(to_py)
    }

    /// Extract text from a single 1-indexed page (hybrid).
    fn extract_page_text(&self, page: u32) -> PyResult<String> {
        self.inner.extract_page_text(page).map_err(to_py)
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

/// Load a `.dpdf` container and return its `model.json` as a JSON string — the minimal
/// Wave-1 handle so callers (and pytest) can exercise distill → load round-trips and inspect
/// the model. (The rich `Doc` accessor API is a later wave.) The returned JSON is the
/// canonical, sorted-key form, so `distill` → `load_model` → re-save is byte-stable.
#[pyfunction]
fn load_model(path: &str) -> PyResult<String> {
    let (model, _assets) = doc::load_dpdf(std::path::Path::new(path)).map_err(to_py)?;
    let bytes = model::container::to_canonical_json(&model).map_err(|e| to_py(Error::Model(e)))?;
    String::from_utf8(bytes).map_err(|e| to_py(Error::ModelNotUtf8(e.to_string())))
}

/// Re-save a `.dpdf` from `src_path` to `dst_path` with a NEW `model.json` and additional
/// verbatim binary members (e.g. `embeddings/<id>.bin` vector matrices). The original
/// container's members (img/ assets AND any pre-existing embedding bins) are carried byte-for-
/// byte; `extra_members` (name → bytes) are written/overwritten on top. This is the durable
/// write path the Python `Doc.embed` uses to add an embedding space without a source PDF: it
/// re-validates indexes + embedding spaces, so a half-record is a loud error, and keeps the
/// archive deterministic (sorted members, zeroed timestamps) so save→load→save is byte-stable
/// WITH embeddings present. `src_path == dst_path` is supported (read fully, then overwrite).
#[pyfunction]
#[pyo3(signature = (src_path, dst_path, model_json, extra_members))]
fn save_dpdf(
    src_path: &str,
    dst_path: &str,
    model_json: &str,
    extra_members: std::collections::BTreeMap<String, Vec<u8>>,
) -> PyResult<()> {
    let (_old_model, carried) = doc::load_dpdf(std::path::Path::new(src_path)).map_err(to_py)?;
    let model: model::DocModel =
        serde_json::from_str(model_json).map_err(|e| to_py(Error::ParseModelJson(e.to_string())))?;
    // The carried members are everything that was in the old container except model.json (the
    // loader already strips it). Split them: embedded ASSET bytes (referenced by model.assets)
    // ride via the asset map; everything else (embedding bins, etc.) is an extra member. The
    // new extra_members overwrite any same-named carried member (re-embedding a space).
    let asset_ids: std::collections::BTreeSet<&str> =
        model.assets.iter().map(|a| a.id.as_str()).collect();
    let mut assets = model::container::AssetBytes::new();
    let mut extras = model::container::AssetBytes::new();
    for (name, bytes) in carried {
        if asset_ids.contains(name.as_str()) {
            assets.insert(name, bytes);
        } else {
            extras.insert(name, bytes);
        }
    }
    for (name, bytes) in extra_members {
        extras.insert(name, bytes);
    }
    model::container::save_with_members(&model, std::path::Path::new(dst_path), &assets, &extras, None)
        .map_err(|e| to_py(Error::Model(e)))
}

/// Read the raw bytes of a single container member (e.g. an `embeddings/<id>.bin` vector
/// matrix) from a `.dpdf`, or `None` if the member isn't present. Lets the Python search path
/// pull a space's f32 matrix without re-implementing the zip reader.
#[pyfunction]
fn read_dpdf_member(path: &str, member: &str) -> PyResult<Option<Vec<u8>>> {
    let (_model, members) = doc::load_dpdf(std::path::Path::new(path)).map_err(to_py)?;
    Ok(members.get(member).cloned())
}

/// Render a loaded `.dpdf` model to HTML, with NO source PDF present — the model-only
/// re-render (the proof that renderers are pure functions of the model). `mode`
/// (`"section"` default / `"page"`) and `toc` match `to_html`. The Wave-1/2 born-digital
/// model drops figure bytes (a regenerable stub), so figures render as the `image_mode="drop"`
/// shape; this is byte-identical to `to_html(..., image_mode="drop")` on the source PDF.
#[pyfunction]
#[pyo3(signature = (path, mode="section", toc=true))]
fn render_html(py: Python<'_>, path: &str, mode: &str, toc: bool) -> PyResult<String> {
    let m = doc::parse_mode(mode).map_err(to_py)?;
    let (model, _assets) = doc::load_dpdf(std::path::Path::new(path)).map_err(to_py)?;
    Ok(py.allow_threads(|| model::render::render_html(&model, m, toc)))
}

/// Render a loaded `.dpdf` model to Markdown, with no source PDF present — the existing
/// HTML→Markdown transform over the model-only HTML. `mode`/`toc` match `to_html`;
/// `image_mode` matches `to_markdown` (the Wave-1/2 model has no figure bytes, so `"external"`
/// degrades to caption placeholders). Returns the Markdown string.
#[pyfunction]
#[pyo3(signature = (path, mode="section", toc=true, image_mode="external"))]
fn render_markdown(py: Python<'_>, path: &str, mode: &str, toc: bool, image_mode: &str) -> PyResult<String> {
    let m = doc::parse_mode(mode).map_err(to_py)?;
    let (model, _assets) = doc::load_dpdf(std::path::Path::new(path)).map_err(to_py)?;
    let (md, _files) = py
        .allow_threads(|| model::render::render_markdown(&model, m, toc, image_mode))
        .map_err(|e| to_py(Error::Model(e)))?;
    Ok(md)
}

/// Extract plain text from a loaded `.dpdf` model (one page per line) — the model-only
/// analogue of `Pdf.extract_text`, sourced from the file with no source PDF present.
#[pyfunction]
fn render_text(py: Python<'_>, path: &str) -> PyResult<String> {
    let (model, _assets) = doc::load_dpdf(std::path::Path::new(path)).map_err(to_py)?;
    Ok(py.allow_threads(|| model::render::extract_text(&model)))
}

/// OCR: render one page's DocTags (granite-docling output) to a distillPDF HTML fragment.
#[pyfunction]
fn ocr_doctags_to_html(doctags: &str) -> String {
    ocr::render::doctags_to_html(doctags)
}

/// Convert a distillPDF HTML document to Markdown. Exposed so the OCR orchestrator can
/// derive Markdown from the *same* OCR-augmented HTML it already built — one model pass
/// feeds both outputs. `image_mode`: "drop" (caption placeholders), "embed" (data URIs),
/// "external" (returns the figure files to write alongside the .md). Returns
/// `(markdown, [(relative_path, bytes), …])`.
#[pyfunction]
#[pyo3(signature = (html, toc=true, image_mode="drop"))]
fn html_to_markdown(html: &str, toc: bool, image_mode: &str) -> PyResult<(String, Vec<(String, Vec<u8>)>)> {
    let im = doc::parse_image_mode(image_mode, true, markdown::ImgMode::Placeholder).map_err(to_py)?;
    let (md, files) = markdown::html_to_markdown(html, toc, im);
    Ok((md, files.into_iter().map(|f| (f.path, f.bytes)).collect()))
}

/// OCR: join a list of per-page DocTags into a full distillPDF-style HTML document.
#[pyfunction]
fn ocr_doctags_doc_html(pages: Vec<String>) -> String {
    let mut body = String::new();
    for (i, dt) in pages.iter().enumerate() {
        body.push_str(&format!("<section data-page=\"{}\">\n", i + 1));
        body.push_str(&ocr::render::doctags_to_html(dt));
        body.push_str("</section>\n");
    }
    format!("<!doctype html>\n<html><head><meta charset=\"utf-8\"></head>\n<body>\n{body}</body></html>\n")
}

/// OCR: write a clean, searchable PDF from per-page DocTags. Each item is
/// `(doctags, image_path_or_empty, width_pts, height_pts)`; figure regions are cropped
/// from the page image when a path is given. Page size defaults to US-Letter if 0.
#[pyfunction]
fn ocr_doctags_to_pdf(pages: Vec<(String, String, f64, f64)>, out_path: &str) -> PyResult<()> {
    let inputs: Vec<ocr::pdf::PageInput> = pages
        .iter()
        .map(|(dt, img, w, h)| {
            let image = if img.is_empty() { None } else { image::open(img).ok() };
            ocr::pdf::PageInput {
                page: ocr::doctags::parse(dt),
                width: if *w > 0.0 { *w as f32 } else { 612.0 },
                height: if *h > 0.0 { *h as f32 } else { 792.0 },
                image,
            }
        })
        .collect();
    let bytes = ocr::pdf::write_pdf(&inputs).map_err(PyValueError::new_err)?;
    std::fs::write(out_path, bytes).map_err(|e| PyValueError::new_err(format!("write failed: {e}")))?;
    Ok(())
}

/// Parse the engine-agnostic options dict (from Python `OcrConfig`/backend) into a
/// `NativeCfg`. Unknown keys are ignored; the engine picks what it needs.
fn parse_native_cfg(opts: Option<&Bound<'_, PyDict>>) -> PyResult<ocr::engine::NativeCfg> {
    let mut cfg = ocr::engine::NativeCfg::default();
    let Some(d) = opts else { return Ok(cfg) };
    if let Ok(Some(v)) = d.get_item("languages") {
        if let Ok(langs) = v.extract::<Vec<String>>() {
            cfg.languages = langs;
        }
    }
    if let Ok(Some(v)) = d.get_item("dpi") {
        cfg.dpi = v.extract::<u32>().ok();
    }
    if let Ok(Some(v)) = d.get_item("prompt") {
        cfg.prompt = v.extract::<String>().ok();
    }
    if let Ok(Some(v)) = d.get_item("max_tokens") {
        cfg.max_tokens = v.extract::<u32>().ok();
    }
    if let Ok(Some(v)) = d.get_item("host") {
        cfg.host = v.extract::<String>().ok();
    }
    if let Ok(Some(v)) = d.get_item("port") {
        cfg.port = v.extract::<u16>().ok();
    }
    if let Ok(Some(v)) = d.get_item("tessdata_dir") {
        cfg.tessdata_dir = v.extract::<String>().ok();
    }
    Ok(cfg)
}

/// Run a Rust-native OCR engine on one page image → DocTags. `opts` is a dict of
/// engine-agnostic options (languages, dpi, prompt, host, port…). Mirrors the Python
/// `OcrBackend.ocr_page` contract so a thin `NativeBackend` can wrap it. The GIL is
/// released during inference (engines are `Sync` and CPU/IO-bound).
#[pyfunction]
#[pyo3(signature = (engine, image, opts=None))]
fn ocr_page_native(
    py: Python<'_>,
    engine: &str,
    image: &[u8],
    opts: Option<&Bound<'_, PyDict>>,
) -> PyResult<String> {
    let cfg = parse_native_cfg(opts)?;
    let eng = ocr::engine::native_engine(engine, &cfg).map_err(PyValueError::new_err)?;
    py.allow_threads(|| eng.ocr_page(image)).map_err(PyValueError::new_err)
}

/// Classify a page image for the text-vs-true-image gate: returns `(raw_words,
/// confident_chars)`. `raw_words` ignores OCR confidence (a blurry photo of text still
/// reports many word-like tokens; a genuine image reports almost none), so the caller can
/// keep a hard-but-readable scan while skipping a real photo. One OCR pass; GIL released.
#[pyfunction]
#[pyo3(signature = (engine, image, opts=None))]
fn ocr_classify_native(
    py: Python<'_>,
    engine: &str,
    image: &[u8],
    opts: Option<&Bound<'_, PyDict>>,
) -> PyResult<(usize, usize)> {
    let cfg = parse_native_cfg(opts)?;
    let eng = ocr::engine::native_engine(engine, &cfg).map_err(PyValueError::new_err)?;
    py.allow_threads(|| eng.classify(image)).map_err(PyValueError::new_err)
}

/// Fraction of "ink" pixels (luma below mid-grey) in a page image, in per-mille (0–1000). A
/// cheap content signal for the OCR gate: a blank/near-blank scan is ~0, a page of text or a
/// photo is well above. Used to rescue a document-like image that Tesseract can't read at all
/// into the accurate (granite) pass — a VLM may recover a degraded scan. Decodes once; GIL
/// released.
#[pyfunction]
fn image_ink_permille(py: Python<'_>, image: &[u8]) -> PyResult<u32> {
    py.allow_threads(|| {
        let img = image::load_from_memory(image).map_err(|e| format!("decode image: {e}"))?;
        let g = img.to_luma8();
        let total = (g.width() as u64 * g.height() as u64).max(1);
        let ink = g.pixels().filter(|p| p.0[0] < 128).count() as u64;
        Ok::<u32, String>((ink * 1000 / total) as u32)
    })
    .map_err(PyValueError::new_err)
}

/// Names of native OCR engines compiled into this wheel (e.g. ["tesseract","server"], or
/// just ["server"] when the tesseract feature is off). Import-light; constructs nothing.
#[pyfunction]
fn native_engines() -> Vec<String> {
    ocr::engine::native_engine_names().into_iter().map(String::from).collect()
}

/// Free cached native-engine resources (the Tesseract handles). Registered as a Python
/// `atexit` hook so C handles are released before interpreter teardown. No-op when the
/// tesseract feature is off.
#[pyfunction]
fn ocr_native_shutdown() {
    #[cfg(feature = "tesseract")]
    ocr::tesseract::clear_cache();
}

/// Detect the dominant language of a text sample and map it to a bundled Tesseract code
/// (`eng`/`por`/`nor`). Returns None when detection is low-confidence or the language isn't
/// one we bundle — the caller then keeps the full bundled set. Pure-Rust (whatlang), so it's
/// only present with the `tesseract` feature.
#[cfg(feature = "tesseract")]
#[pyfunction]
fn detect_language(text: &str) -> Option<String> {
    let info = whatlang::detect(text)?;
    if !info.is_reliable() || info.confidence() < 0.55 {
        return None;
    }
    let code = match info.lang() {
        whatlang::Lang::Eng => "eng",
        whatlang::Lang::Por => "por",
        whatlang::Lang::Nob => "nor", // Norwegian Bokmål → the bundled `nor` model
        _ => return None,
    };
    Some(code.to_string())
}

#[pymodule]
fn _distillpdf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Pdf>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_function(wrap_pyfunction!(from_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(load_model, m)?)?;
    m.add_function(wrap_pyfunction!(save_dpdf, m)?)?;
    m.add_function(wrap_pyfunction!(read_dpdf_member, m)?)?;
    m.add_function(wrap_pyfunction!(render_html, m)?)?;
    m.add_function(wrap_pyfunction!(render_markdown, m)?)?;
    m.add_function(wrap_pyfunction!(render_text, m)?)?;
    m.add_function(wrap_pyfunction!(ocr_doctags_to_html, m)?)?;
    m.add_function(wrap_pyfunction!(html_to_markdown, m)?)?;
    m.add_function(wrap_pyfunction!(ocr_doctags_doc_html, m)?)?;
    m.add_function(wrap_pyfunction!(ocr_doctags_to_pdf, m)?)?;
    m.add_function(wrap_pyfunction!(ocr_page_native, m)?)?;
    m.add_function(wrap_pyfunction!(ocr_classify_native, m)?)?;
    m.add_function(wrap_pyfunction!(image_ink_permille, m)?)?;
    m.add_function(wrap_pyfunction!(native_engines, m)?)?;
    m.add_function(wrap_pyfunction!(ocr_native_shutdown, m)?)?;
    #[cfg(feature = "tesseract")]
    m.add_function(wrap_pyfunction!(detect_language, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
