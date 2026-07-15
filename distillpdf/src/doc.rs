//! The pure-Rust document core — `PdfDocument` and its typed operations.
//!
//! This is the layer the PyO3 binding (`src/lib.rs`) is a thin wrapper over, and the future
//! public surface a Rust embedder (kglite's `knowledge_tree`) consumes: a reusable handle
//! (`open`/`from_bytes`), typed results, structured [`Error`]s, typed `.dpdf` load, and a
//! typed [`DistillOptions`]. NO pyo3 appears here — the binding does all Python-object
//! assembly and maps [`Error`] → `PyValueError`.

use lopdf::dictionary;
use lopdf::Document;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::Error;
use crate::extract::{self, FontInfo, ImageInfo, TableInfo};
use crate::model::container::AssetBytes;
use crate::model::{self, AssetProfile, DocModel};
use crate::{frontmatter, html, links, markdown, nav, ocr, text};

/// Which assets a `distill` captures, typed. Replaces the bare `assets=` string at the core
/// boundary; the binding maps its string via [`parse_assets`]/[`DistillOptions::from_assets`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistillOptions {
    /// The asset save profile (figure/page-raster byte capture policy).
    pub profile: AssetProfile,
}

// The named-profile constructors are the future public API (kglite calls
// `distill(&DistillOptions::text_only())` per the consumer contract); the binding itself only
// uses `from_assets`, so they read as dead in this single-crate build until Phase 2's re-exports
// and the external consumer wire them up — the same "defined now, wired later" allowance
// `model/mod.rs` takes.
#[allow(dead_code)]
impl DistillOptions {
    /// Text + structure only — drop all asset bytes (keeps regenerable stubs).
    pub fn text_only() -> Self {
        Self { profile: AssetProfile::None }
    }
    /// Embed figure bytes; page rasters stay dropped-with-stub (the default profile).
    pub fn with_figures() -> Self {
        Self { profile: AssetProfile::Figures }
    }
    /// Figures and page rasters (equals `with_figures` on the born-digital path today).
    pub fn full() -> Self {
        Self { profile: AssetProfile::Full }
    }
    /// Build from the `assets=` string the Python `distill` accepts.
    pub fn from_assets(s: &str) -> Result<Self, Error> {
        Ok(Self { profile: parse_assets(s)? })
    }
}

/// Parse the `mode` string into an [`html::Mode`] (`"section"` / `"page"`).
pub fn parse_mode(mode: &str) -> Result<html::Mode, Error> {
    match mode {
        "section" => Ok(html::Mode::Section),
        "page" => Ok(html::Mode::Page),
        other => Err(Error::InvalidMode(other.to_string())),
    }
}

/// Parse the `image_mode` string into a render strategy:
/// * `"embed"` → inline base64 `data:` URIs (self-contained).
/// * `"external"` → extract figures to an `img/` folder; only possible when writing to a
///   file, so a returned string falls back to `string_fallback`.
/// * `"drop"` → replace images with placeholder text.
pub fn parse_image_mode(s: &str, writing: bool, string_fallback: markdown::ImgMode) -> Result<markdown::ImgMode, Error> {
    match s {
        "embed" => Ok(markdown::ImgMode::Embed),
        "drop" => Ok(markdown::ImgMode::Placeholder),
        "external" => Ok(if writing { markdown::ImgMode::Files } else { string_fallback }),
        other => Err(Error::InvalidImageMode(other.to_string())),
    }
}

/// Parse the `assets=` string into a typed [`AssetProfile`].
pub fn parse_assets(s: &str) -> Result<AssetProfile, Error> {
    AssetProfile::parse(s).map_err(Error::Model)
}

/// Load a `.dpdf` container into its typed [`DocModel`] plus the raw member bytes (assets and
/// any other members kept separate from the model, per the consumer contract). Re-exported at
/// the crate root ([`crate::load_dpdf`]) as the typed `.dpdf` load convenience.
pub fn load_dpdf(path: &Path) -> Result<(DocModel, AssetBytes), Error> {
    model::container::load(path).map_err(Error::Model)
}

/// One page of an OCR plan. Mirrors the dict `Pdf.ocr_plan` returns.
pub struct OcrPlanEntry {
    pub page: u32,
    pub needs_ocr: bool,
    pub reason: String,
    pub width_pts: f32,
    pub height_pts: f32,
    pub image: Option<Vec<u8>>,
}

/// A loaded PDF document — the reusable pure-Rust handle.
pub struct PdfDocument {
    pub(crate) doc: Document,
    /// Raw PDF bytes, kept for lenient recovery of malformed streams.
    pub(crate) raw: Vec<u8>,
    /// Source path (`open`); `None` when constructed from bytes.
    pub(crate) source: Option<PathBuf>,
    /// Cached OCR results: `{1-based page: DocTags}`, populated once by `set_ocr`.
    pub(crate) ocr_cache: Mutex<HashMap<u32, String>>,
}

impl PdfDocument {
    /// Open a PDF from a filesystem path. Only loads/parses the container.
    pub fn open(path: &str) -> Result<Self, Error> {
        let raw = std::fs::read(path).map_err(Error::Read)?;
        let doc = Document::load_mem(&raw).map_err(|e| Error::Open(e.to_string()))?;
        Ok(PdfDocument { doc, raw, source: Some(PathBuf::from(path)), ocr_cache: Default::default() })
    }

    /// Open a PDF from raw bytes. There is no source path.
    pub fn from_bytes(data: &[u8]) -> Result<Self, Error> {
        let raw = data.to_vec();
        let doc = Document::load_mem(&raw).map_err(|e| Error::Parse(e.to_string()))?;
        Ok(PdfDocument { doc, raw, source: None, ocr_cache: Default::default() })
    }

    /// Number of pages.
    pub fn page_count(&self) -> usize {
        self.doc.get_pages().len()
    }

    /// Extract plain text from all pages (concatenated, page order). Hybrid: our
    /// ToUnicode-aware extractor primary, lopdf fallback per page.
    pub fn extract_text(&self) -> String {
        let pages = self.doc.get_pages();
        let mut out = String::new();
        for (&p, &page_id) in &pages {
            let mine = text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default();
            if mine.trim().chars().count() >= 2 {
                out.push_str(&mine);
            } else {
                out.push_str(&self.doc.extract_text(&[p]).unwrap_or_default());
            }
            out.push('\n');
        }
        out
    }

    /// Extract text from a single 1-indexed page (hybrid).
    pub fn extract_page_text(&self, page: u32) -> Result<String, Error> {
        let page_id = *self.doc.get_pages().get(&page).ok_or(Error::NoPage(Some(page)))?;
        let mine = text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default();
        Ok(if mine.trim().chars().count() >= 2 {
            mine
        } else {
            self.doc.extract_text(&[page]).unwrap_or_default()
        })
    }

    /// Diagnostic: force our ToUnicode extractor for all pages.
    pub fn mine_text(&self) -> String {
        let mut out = String::new();
        for &page_id in self.doc.get_pages().values() {
            out.push_str(&text::extract_page(&self.doc, page_id, &self.raw).unwrap_or_default());
            out.push('\n');
        }
        out
    }

    /// Diagnostic: raw spans (text, x, width, size) for a 1-indexed page.
    pub fn dbg_spans(&self, page: u32) -> Result<Vec<(String, f32, f32, f32)>, Error> {
        let page_id = *self.doc.get_pages().get(&page).ok_or(Error::NoPage(None))?;
        Ok(text::extract_spans(&self.doc, page_id, &self.raw)
            .into_iter()
            .map(|s| (s.text, s.x, s.width, s.size))
            .collect())
    }

    /// Diagnostic: spans with y for a 1-indexed page (text, x, y, width, size).
    #[allow(clippy::type_complexity)] // a flat diagnostic tuple mirroring the Python `_dbg_spans_xy`
    pub fn dbg_spans_xy(&self, page: u32) -> Result<Vec<(String, f32, f32, f32, f32)>, Error> {
        let page_id = *self.doc.get_pages().get(&page).ok_or(Error::NoPage(None))?;
        Ok(text::extract_spans(&self.doc, page_id, &self.raw)
            .into_iter()
            .map(|s| (s.text, s.x, s.y, s.width, s.size))
            .collect())
    }

    /// Diagnostic for one 1-indexed page.
    pub fn debug_page(&self, page: u32) -> Result<String, Error> {
        let page_id = *self.doc.get_pages().get(&page).ok_or(Error::NoPage(Some(page)))?;
        Ok(text::debug_page(&self.doc, page_id, &self.raw))
    }

    /// Extract images from all pages.
    pub fn extract_images(&self) -> Vec<ImageInfo> {
        extract::extract_images(&self.doc)
    }

    /// Extract per-page font info.
    pub fn extract_fonts(&self) -> Vec<FontInfo> {
        extract::extract_fonts(&self.doc)
    }

    /// Extract tables from all pages.
    pub fn extract_tables(&self) -> Vec<TableInfo> {
        extract::extract_tables(&self.doc, &self.raw)
    }

    /// Extract hyperlinks from all pages.
    pub fn extract_links(&self) -> Vec<links::Link> {
        links::extract_links(&self.doc)
    }

    /// Render the document to HTML.
    pub fn render(&self, mode: html::Mode, images: bool, toc: bool) -> String {
        html::to_html(&self.doc, &self.raw, mode, images, toc)
    }

    /// The detected-heading outline: `(level, title, page, anchor_id)` in reading order.
    pub fn toc(&self, mode: html::Mode) -> Vec<(u8, String, u32, String)> {
        nav::toc(&html::to_html(&self.doc, &self.raw, mode, false, true))
    }

    /// The PDF's OWN `/Outlines` bookmarks as `(level, title, page, anchor)`.
    pub fn outline(&self) -> Vec<(u8, String, u32, String)> {
        links::outline(&self.doc)
            .into_iter()
            .map(|e| ((e.level + 1), e.title, e.page, format!("page-{}", e.page)))
            .collect()
    }

    /// HTML of a single section resolved by `name`.
    pub fn section(&self, mode: html::Mode, name: &str, images: bool) -> Option<String> {
        nav::section(&html::to_html(&self.doc, &self.raw, mode, images, true), name)
    }

    /// Structured front-matter of an academic paper (page 1).
    pub fn front_matter(&self) -> frontmatter::FrontMatter {
        frontmatter::extract_front_matter(&self.doc, &self.raw)
    }

    /// OCR plan: per page, whether OCR is needed and (if so) the page raster bytes.
    pub fn ocr_plan(&self) -> Vec<OcrPlanEntry> {
        let mut out = Vec::new();
        for (&pno, &page_id) in &self.doc.get_pages() {
            let decision = ocr::detect::decide(&self.doc, page_id, &self.raw);
            let needs = !matches!(decision, ocr::detect::OcrDecision::NotNeeded);
            let (w, h) = ocr::page_size_pts(&self.doc, page_id);
            let image = if needs {
                ocr::page_main_image(&self.doc, page_id).map(|(b, _)| b)
            } else {
                None
            };
            out.push(OcrPlanEntry { page: pno, needs_ocr: needs, reason: format!("{decision:?}"), width_pts: w, height_pts: h, image });
        }
        out
    }

    /// Merge OCR results into the cache; returns the cached page count.
    pub fn set_ocr(&self, ocr: HashMap<u32, String>) -> Result<usize, Error> {
        let mut cache = self.ocr_cache.lock().map_err(|_| Error::OcrPoisoned)?;
        cache.extend(ocr);
        Ok(cache.len())
    }

    /// A copy of the cached OCR results.
    pub fn get_ocr(&self) -> Result<HashMap<u32, String>, Error> {
        Ok(self.ocr_cache.lock().map_err(|_| Error::OcrPoisoned)?.clone())
    }

    /// True if OCR results have been cached.
    pub fn has_ocr(&self) -> Result<bool, Error> {
        Ok(!self.ocr_cache.lock().map_err(|_| Error::OcrPoisoned)?.is_empty())
    }

    /// Build a searchable PDF from OCR results. `remove_raster` selects clean-reflow vs
    /// invisible-overlay. Returns the saved PDF bytes; the caller writes the file.
    pub fn build_searchable_pdf(&self, ocr: &HashMap<u32, String>, remove_raster: bool) -> Result<Vec<u8>, Error> {
        let build = || -> Result<Vec<u8>, String> {
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
                doc.prune_objects();
            }
            let mut buf = Vec::new();
            doc.save_to(&mut buf).map_err(|e| e.to_string())?;
            Ok(buf)
        };
        build().map_err(Error::Model)
    }

    /// Resolve where rendered output is written for the given default extension.
    pub fn resolve_out_path(&self, path: Option<&str>, ext: &str) -> Result<PathBuf, Error> {
        match path {
            Some(p) if Path::new(p).is_dir() => {
                let stem = self.source.as_ref().and_then(|s| s.file_stem()).ok_or(Error::NoSourceDir)?;
                Ok(Path::new(p).join(stem).with_extension(ext))
            }
            Some(p) => Ok(PathBuf::from(p)),
            None => self.source.as_ref().map(|s| s.with_extension(ext)).ok_or(Error::NoSourcePath),
        }
    }

    /// Write `content` to `dest` plus any extracted figure files under `dest`'s directory.
    pub fn write_doc(&self, dest: PathBuf, content: &str, files: &[markdown::ImageFile]) -> Result<String, Error> {
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(Error::Mkdir)?;
        }
        std::fs::write(&dest, content).map_err(Error::Write)?;
        if !files.is_empty() {
            let dir = dest.parent().unwrap_or_else(|| Path::new("."));
            for f in files {
                let fp = dir.join(&f.path);
                if let Some(parent) = fp.parent() {
                    std::fs::create_dir_all(parent).map_err(Error::Mkdir)?;
                }
                std::fs::write(&fp, &f.bytes).map_err(Error::Write)?;
            }
        }
        Ok(dest.to_string_lossy().into_owned())
    }

    /// Distill the document into a `.dpdf` container. Resolves the destination, builds the
    /// model (the single `source.generated_at` clock read is taken here), and saves. Returns
    /// the written path.
    pub fn distill(&self, path: Option<&str>, opts: &DistillOptions) -> Result<String, Error> {
        let dest = self.resolve_out_path(path, "dpdf")?;
        let file = self
            .source
            .as_ref()
            .and_then(|s| s.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document.pdf".to_string());
        let generated_at = iso8601_now();
        let (model, asset_bytes) = model::build::build_model(&self.doc, &self.raw, &file, generated_at, opts.profile);
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(Error::Mkdir)?;
        }
        model::container::save(&model, &dest, &asset_bytes, None).map_err(Error::Model)?;
        Ok(dest.to_string_lossy().into_owned())
    }
}

/// Append `stream_id` to a page's `/Contents` so an extra content stream (the invisible OCR
/// text overlay) draws after the page's own content while leaving it untouched.
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
/// the page's own fonts), preserving its existing resources. Used by the keep-raster path.
fn add_overlay_fonts(doc: &mut Document, page_id: lopdf::ObjectId, helv: lopdf::ObjectId, helv_b: lopdf::ObjectId) {
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

/// Current UTC time as an ISO-8601 `YYYY-MM-DDTHH:MM:SSZ` string. The ONLY clock read into a
/// `.dpdf` model (`source.generated_at`); everything else is content-derived and deterministic.
fn iso8601_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_conveniences() {
        // Mode/ImgMode don't derive Debug, so assert on matches! + the error string.
        assert!(matches!(parse_mode("section").unwrap(), html::Mode::Section));
        assert!(matches!(parse_mode("page").unwrap(), html::Mode::Page));
        // Mode is not Debug, so avoid unwrap_err (which would format the Ok value).
        let err = parse_mode("bogus").err().expect("bogus mode must fail");
        assert_eq!(err.to_string(), "invalid mode \"bogus\": expected \"section\" or \"page\"");
    }

    #[test]
    fn parse_image_mode_writing_and_fallback() {
        use markdown::ImgMode;
        assert!(matches!(parse_image_mode("embed", true, ImgMode::Embed).unwrap(), ImgMode::Embed));
        assert!(matches!(parse_image_mode("drop", true, ImgMode::Embed).unwrap(), ImgMode::Placeholder));
        // external + writing → Files; external + not-writing → the fallback.
        assert!(matches!(parse_image_mode("external", true, ImgMode::Placeholder).unwrap(), ImgMode::Files));
        assert!(matches!(parse_image_mode("external", false, ImgMode::Embed).unwrap(), ImgMode::Embed));
        let err = parse_image_mode("nope", true, ImgMode::Embed).err().expect("bad image_mode must fail");
        assert_eq!(err.to_string(), "invalid image_mode \"nope\": expected \"embed\", \"external\", or \"drop\"");
    }

    #[test]
    fn parse_assets_maps_message_verbatim() {
        assert_eq!(parse_assets("figures").unwrap(), AssetProfile::Figures);
        assert_eq!(parse_assets("full").unwrap(), AssetProfile::Full);
        assert_eq!(parse_assets("none").unwrap(), AssetProfile::None);
        assert_eq!(
            parse_assets("weird").unwrap_err().to_string(),
            "invalid assets \"weird\": expected \"figures\", \"full\", or \"none\""
        );
    }

    #[test]
    fn distill_options_profiles() {
        assert_eq!(DistillOptions::text_only().profile, AssetProfile::None);
        assert_eq!(DistillOptions::with_figures().profile, AssetProfile::Figures);
        assert_eq!(DistillOptions::full().profile, AssetProfile::Full);
        assert_eq!(DistillOptions::from_assets("none").unwrap().profile, AssetProfile::None);
    }
}
