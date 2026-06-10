//! distillpdf — pure-Rust PDF extraction on lopdf, exposed to Python via PyO3.
//!
//! Phase 0: open a PDF, report page count, extract text.
//! Engine (lopdf) is confined to this boundary module; higher-level extraction
//! layers will be added above it (text spans, tables, images, fonts).

use lopdf::dictionary;
use lopdf::Document;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

mod afm;
mod captions;
mod elem_passes;
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

/// Append `stream_id` to a page's `/Contents` (PDF concatenates a content array), so an
/// extra content stream — e.g. the invisible OCR text overlay — draws after the page's own
/// content while leaving it untouched.
fn append_page_content(doc: &mut Document, page_id: lopdf::ObjectId, stream_id: lopdf::ObjectId) {
    let Ok(page) = doc.get_object_mut(page_id).and_then(|o| o.as_dict_mut()) else { return };
    let new = match page.get(b"Contents").ok().cloned() {
        Some(lopdf::Object::Array(mut a)) => {
            a.push(lopdf::Object::Reference(stream_id));
            lopdf::Object::Array(a)
        }
        Some(existing @ lopdf::Object::Reference(_)) => lopdf::Object::Array(vec![existing, lopdf::Object::Reference(stream_id)]),
        _ => lopdf::Object::Reference(stream_id),
    };
    page.set("Contents", new);
}

/// Give a page its own `/Resources` carrying the OCR overlay fonts (under names distinct from
/// the page's own fonts), preserving its existing resources so the page's raster/text still
/// render. Used by the keep-raster searchable-PDF path.
fn add_overlay_fonts(doc: &mut Document, page_id: lopdf::ObjectId, helv: lopdf::ObjectId, helv_b: lopdf::ObjectId) {
    // Resolve the page's effective resources (own or inherited from /Pages), as an owned copy.
    let mut res = match doc.get_page_resources(page_id) {
        Ok((Some(d), _)) => d.clone(),
        Ok((None, ids)) => ids.first().and_then(|id| doc.get_dictionary(*id).ok()).cloned().unwrap_or_default(),
        Err(_) => lopdf::Dictionary::new(),
    };
    let mut fonts = match res.get(b"Font").ok().cloned() {
        Some(lopdf::Object::Dictionary(d)) => d,
        Some(lopdf::Object::Reference(r)) => doc.get_dictionary(r).cloned().unwrap_or_default(),
        _ => lopdf::Dictionary::new(),
    };
    fonts.set(ocr::pdf::OVERLAY_FONT, lopdf::Object::Reference(helv));
    fonts.set(ocr::pdf::OVERLAY_FONT_BOLD, lopdf::Object::Reference(helv_b));
    res.set("Font", fonts);
    if let Ok(page) = doc.get_object_mut(page_id).and_then(|o| o.as_dict_mut()) {
        page.set("Resources", lopdf::Object::Dictionary(res));
    }
}

/// Current UTC time as an ISO-8601 `YYYY-MM-DDTHH:MM:SSZ` string. This is the ONLY clock
/// read into a `.dpdf` model (`source.generated_at`); everything else is content-derived and
/// deterministic. Computed from `SystemTime` by hand (a civil-date conversion) so the model
/// path needs no `chrono`/`time` direct dependency for one timestamp.
fn iso8601_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm), epoch 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
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
    /// Cached OCR results: `{1-based page: DocTags}`, populated once by `set_ocr` (the
    /// `distillpdf.ocr.run` orchestrator) so a single model pass feeds every renderer
    /// (`to_pdf` / OCR-augmented HTML / Markdown) — the model never re-runs per output.
    ocr_cache: std::sync::Mutex<std::collections::HashMap<u32, String>>,
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
        Ok(Pdf { doc, raw, source: Some(std::path::PathBuf::from(path)), ocr_cache: Default::default() })
    }

    /// Open a PDF from raw bytes. There is no source path, so writing output with
    /// `outputfile=True` (no `path`) is an error — pass an explicit `path` instead.
    #[staticmethod]
    fn from_bytes(data: &[u8]) -> PyResult<Self> {
        let raw = data.to_vec();
        let doc =
            Document::load_mem(&raw).map_err(|e| PyValueError::new_err(format!("parse failed: {e}")))?;
        Ok(Pdf { doc, raw, source: None, ocr_cache: Default::default() })
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
        let fm = py.allow_threads(|| frontmatter::extract_front_matter(&self.doc, &self.raw));
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
        for (&pno, &page_id) in &self.doc.get_pages() {
            let decision = ocr::detect::decide(&self.doc, page_id, &self.raw);
            let needs = !matches!(decision, ocr::detect::OcrDecision::NotNeeded);
            let (w, h) = ocr::page_size_pts(&self.doc, page_id);
            let d = PyDict::new(py);
            d.set_item("page", pno)?;
            d.set_item("needs_ocr", needs)?;
            d.set_item("reason", format!("{decision:?}"))?;
            d.set_item("width_pts", w)?;
            d.set_item("height_pts", h)?;
            let img = if needs {
                ocr::page_main_image(&self.doc, page_id).map(|(b, _)| b)
            } else {
                None
            };
            match img {
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
        let mut cache = self.ocr_cache.lock().map_err(|_| PyValueError::new_err("ocr cache poisoned"))?;
        cache.extend(ocr);
        Ok(cache.len())
    }

    /// The cached OCR results (`{page: DocTags}`), empty if `set_ocr` was never called.
    fn get_ocr(&self) -> PyResult<std::collections::HashMap<u32, String>> {
        Ok(self.ocr_cache.lock().map_err(|_| PyValueError::new_err("ocr cache poisoned"))?.clone())
    }

    /// True if OCR results have been cached on this object (a model pass already ran).
    fn has_ocr(&self) -> PyResult<bool> {
        Ok(!self.ocr_cache.lock().map_err(|_| PyValueError::new_err("ocr cache poisoned"))?.is_empty())
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
            None => self.ocr_cache.lock().map_err(|_| PyValueError::new_err("ocr cache poisoned"))?.clone(),
        };
        let buf = py.allow_threads(|| -> Result<Vec<u8>, String> {
            let mut doc = Document::load_mem(&self.raw).map_err(|e| e.to_string())?;
            let (helv, helv_b) = ocr::pdf::add_fonts(&mut doc);
            let pages = doc.get_pages();
            for (&pno, &page_id) in &pages {
                let Some(dt) = ocr.get(&pno) else { continue };
                let (w, h) = ocr::page_size_pts(&doc, page_id);
                if remove_raster {
                    // Clean reflow: replace the page's content with our text + cropped figures.
                    let image = ocr::page_main_image(&doc, page_id).map(|(_, img)| img);
                    let pin = ocr::pdf::PageInput { page: ocr::doctags::parse(dt), width: w, height: h, image };
                    let (content, xobjs) = ocr::pdf::build_page_content(&mut doc, &pin)?;
                    let data = content.encode().map_err(|e| e.to_string())?;
                    let stream_id = doc.add_object(lopdf::Stream::new(lopdf::Dictionary::new(), data));
                    // (The old full-page image XObject simply goes undrawn, then is pruned.)
                    let mut xo = lopdf::Dictionary::new();
                    for (name, id) in &xobjs {
                        xo.set(name.as_bytes().to_vec(), lopdf::Object::Reference(*id));
                    }
                    let res = dictionary! {
                        "Font" => dictionary! { "F1" => helv, "F2" => helv_b },
                        "XObject" => xo,
                    };
                    let page = doc.get_object_mut(page_id).map_err(|e| e.to_string())?.as_dict_mut().map_err(|e| e.to_string())?;
                    page.set("Contents", lopdf::Object::Reference(stream_id));
                    page.set("Resources", lopdf::Object::Dictionary(res));
                } else {
                    // Keep the scan: append an invisible OCR text layer over the original page.
                    let pin = ocr::pdf::PageInput { page: ocr::doctags::parse(dt), width: w, height: h, image: None };
                    let data = ocr::pdf::build_text_overlay(&pin).encode().map_err(|e| e.to_string())?;
                    let stream_id = doc.add_object(lopdf::Stream::new(lopdf::Dictionary::new(), data));
                    append_page_content(&mut doc, page_id, stream_id);
                    add_overlay_fonts(&mut doc, page_id, helv, helv_b);
                }
            }
            if remove_raster {
                // Drop the now-unreferenced full-page rasters + old content streams.
                doc.prune_objects();
            }
            let mut buf = Vec::new();
            doc.save_to(&mut buf).map_err(|e| e.to_string())?;
            Ok(buf)
        }).map_err(PyValueError::new_err)?;
        std::fs::write(path, buf).map_err(|e| PyValueError::new_err(format!("write failed: {e}")))?;
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
        let profile = model::AssetProfile::parse(assets).map_err(PyValueError::new_err)?;
        let dest = self.resolve_out_path(path, "dpdf")?;
        // The display name recorded in source.file: the source PDF's basename when known.
        let file = self
            .source
            .as_ref()
            .and_then(|s| s.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document.pdf".to_string());
        // The single timestamp in the model — taken once here so the rest is deterministic.
        let generated_at = iso8601_now();
        let (model, asset_bytes) =
            py.allow_threads(|| model::build::build_model(&self.doc, &self.raw, &file, generated_at, profile));
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| PyValueError::new_err(format!("mkdir failed: {e}")))?;
        }
        // Embedded figure bytes (per the profile) ride along in the container; dropped assets
        // contribute only their stub from `model.assets`.
        model::container::save(&model, &dest, &asset_bytes, None).map_err(PyValueError::new_err)?;
        Ok(dest.to_string_lossy().into_owned())
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

/// Load a `.dpdf` container and return its `model.json` as a JSON string — the minimal
/// Wave-1 handle so callers (and pytest) can exercise distill → load round-trips and inspect
/// the model. (The rich `Doc` accessor API is a later wave.) The returned JSON is the
/// canonical, sorted-key form, so `distill` → `load_model` → re-save is byte-stable.
#[pyfunction]
fn load_model(path: &str) -> PyResult<String> {
    let (model, _assets) = model::container::load(std::path::Path::new(path)).map_err(PyValueError::new_err)?;
    let bytes = model::container::to_canonical_json(&model).map_err(PyValueError::new_err)?;
    String::from_utf8(bytes).map_err(|e| PyValueError::new_err(format!("model json not utf-8: {e}")))
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
    let (_old_model, carried) =
        model::container::load(std::path::Path::new(src_path)).map_err(PyValueError::new_err)?;
    let model: model::DocModel =
        serde_json::from_str(model_json).map_err(|e| PyValueError::new_err(format!("parse model_json: {e}")))?;
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
        .map_err(PyValueError::new_err)
}

/// Read the raw bytes of a single container member (e.g. an `embeddings/<id>.bin` vector
/// matrix) from a `.dpdf`, or `None` if the member isn't present. Lets the Python search path
/// pull a space's f32 matrix without re-implementing the zip reader.
#[pyfunction]
fn read_dpdf_member(path: &str, member: &str) -> PyResult<Option<Vec<u8>>> {
    let (_model, members) =
        model::container::load(std::path::Path::new(path)).map_err(PyValueError::new_err)?;
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
    let m = parse_mode(mode)?;
    let (model, _assets) = model::container::load(std::path::Path::new(path)).map_err(PyValueError::new_err)?;
    Ok(py.allow_threads(|| model::render::render_html(&model, m, toc)))
}

/// Render a loaded `.dpdf` model to Markdown, with no source PDF present — the existing
/// HTML→Markdown transform over the model-only HTML. `mode`/`toc` match `to_html`;
/// `image_mode` matches `to_markdown` (the Wave-1/2 model has no figure bytes, so `"external"`
/// degrades to caption placeholders). Returns the Markdown string.
#[pyfunction]
#[pyo3(signature = (path, mode="section", toc=true, image_mode="external"))]
fn render_markdown(py: Python<'_>, path: &str, mode: &str, toc: bool, image_mode: &str) -> PyResult<String> {
    let m = parse_mode(mode)?;
    let (model, _assets) = model::container::load(std::path::Path::new(path)).map_err(PyValueError::new_err)?;
    let (md, _files) = py
        .allow_threads(|| model::render::render_markdown(&model, m, toc, image_mode))
        .map_err(PyValueError::new_err)?;
    Ok(md)
}

/// Extract plain text from a loaded `.dpdf` model (one page per line) — the model-only
/// analogue of `Pdf.extract_text`, sourced from the file with no source PDF present.
#[pyfunction]
fn render_text(py: Python<'_>, path: &str) -> PyResult<String> {
    let (model, _assets) = model::container::load(std::path::Path::new(path)).map_err(PyValueError::new_err)?;
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
    let im = parse_image_mode(image_mode, true, markdown::ImgMode::Placeholder)?;
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
