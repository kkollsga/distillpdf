//! The pure-Rust document core — `PdfDocument` and its typed operations.
//!
//! This is the layer the PyO3 binding (`src/lib.rs`) is a thin wrapper over, and the future
//! public surface a Rust embedder (kglite's `knowledge_tree`) consumes: a reusable handle
//! (`open`/`from_bytes`), typed results, structured [`Error`]s, typed `.dpdf` load, and a
//! typed [`DistillOptions`]. NO pyo3 appears here — the binding does all Python-object
//! assembly and maps [`Error`] → `PyValueError`.

use lopdf::dictionary;
use lopdf::Document;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::access::{DocumentAccess, EagerDocumentAdapter};
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
    pub(crate) doc: Arc<Document>,
    /// Raw PDF bytes, kept for lenient recovery of malformed streams.
    pub(crate) raw: Arc<[u8]>,
    /// Runtime-selectable immutable access route. L2 uses the eager oracle adapter; L3 adds
    /// the bounded indexed implementation without reopening consumer signatures.
    pub(crate) access: Arc<dyn DocumentAccess>,
    /// Source path (`open`); `None` when constructed from bytes.
    pub(crate) source: Option<PathBuf>,
    /// Cached OCR results: `{1-based page: DocTags}`, populated once by `set_ocr`.
    pub(crate) ocr_cache: Mutex<HashMap<u32, String>>,
}

/// A private one-thread rayon pool, used for **nothing but** `Document::load_mem`.
///
/// lopdf builds `document.objects` from a rayon `par_iter` over the xref, and every object
/// stream a worker decodes is appended to one shared `Mutex<Vec<_>>`. The duplicate-object
/// resolution that follows is `objects.entry(id).or_insert(entry)` — *first entry in that vec
/// wins* — so the winner is decided by **thread-completion order**, which is not stable from
/// run to run. When two object streams both carry a definition for the same object number and
/// the xref's `/Type /ObjStm` container map does not disambiguate them, the loaded document
/// differs between runs.
///
/// Measured (`dev-docs/bench/out/g8/`): 40 loads of one USGS file produce **3 distinct object
/// maps**, flipping a struct-tree element between `/S /Figure` and `/S /Artifact`, which in
/// turn made a table header row appear and disappear across `to_html()` renders of the same
/// bytes. This is not table-specific: any document with that shape can parse differently on
/// each run. Still present in lopdf 0.44 (the merge code is unchanged); filed upstream.
///
/// We do not fork or vendor lopdf (owner decision), and the two obvious knobs are both
/// unacceptable: `RAYON_NUM_THREADS=1` sets the *global* pool and would serialise **our** rayon
/// work too (text extraction is where we most clearly win), and `default-features = false`
/// drops lopdf's rayon at a large load-time cost. Installing a private one-thread pool around
/// the load alone confines the serialisation to exactly the racing code and leaves the global
/// pool — and therefore all of our own parallelism — untouched.
struct LoadPool {
    pool: OnceLock<rayon::ThreadPool>,
    init: Mutex<()>,
}

impl LoadPool {
    const fn new() -> Self {
        Self { pool: OnceLock::new(), init: Mutex::new(()) }
    }

    /// Return the loader pool, creating it exactly once.
    ///
    /// `OnceLock::get_or_try_init` is unstable, so an initialization mutex supplies its
    /// fallible equivalent. The fast path is lock-free; the slow path is serialized so a burst
    /// of first loads cannot worsen thread exhaustion by all trying to create a pool at once.
    /// A failed build leaves the cell empty, allowing a later load to retry.
    fn get_or_build(
        &self,
        build: impl FnOnce() -> Result<rayon::ThreadPool, rayon::ThreadPoolBuildError>,
    ) -> Result<&rayon::ThreadPool, lopdf::Error> {
        if let Some(pool) = self.pool.get() {
            return Ok(pool);
        }
        let guard = self.init.lock().map_err(|_| {
            lopdf::Error::IO(std::io::Error::other("deterministic PDF loader initialization lock was poisoned"))
        })?;
        if let Some(pool) = self.pool.get() {
            drop(guard);
            return Ok(pool);
        }
        let pool = build().map_err(|e| {
            lopdf::Error::IO(std::io::Error::other(format!("failed to initialize deterministic PDF loader: {e}")))
        })?;
        // Every initializer holds `init`, so no other caller can win this set.
        if self.pool.set(pool).is_err() {
            drop(guard);
            return self.pool.get().ok_or_else(|| {
                lopdf::Error::IO(std::io::Error::other("deterministic PDF loader initialization raced"))
            });
        }
        let pool = self.pool.get().ok_or_else(|| {
            lopdf::Error::IO(std::io::Error::other("deterministic PDF loader was not initialized"))
        })?;
        drop(guard);
        Ok(pool)
    }
}

fn load_mem_with_pool(
    raw: &[u8],
    pool: &LoadPool,
    build: impl FnOnce() -> Result<rayon::ThreadPool, rayon::ThreadPoolBuildError>,
) -> Result<Document, lopdf::Error> {
    let pool = pool.get_or_build(build)?;
    pool.install(|| Document::load_mem(raw))
}

/// `Document::load_mem` with a deterministic object map — see [`LoadPool`].
///
/// Every production load in this crate goes through here; calling `Document::load_mem`
/// directly reintroduces the race. Pool creation therefore fails closed: a thread-spawn
/// failure is returned to the caller and a later load may retry, but the known-racy unscoped
/// loader is never used.
fn load_mem_deterministic(raw: &[u8]) -> Result<Document, lopdf::Error> {
    static POOL: LoadPool = LoadPool::new();
    load_mem_with_pool(raw, &POOL, || {
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .thread_name(|_| "distillpdf-load".to_string())
            .build()
    })
}

/// Lopdf-0.44-compatible display for public errors whose nested cause the fork redacts.
fn eager_error_message(error: &lopdf::Error) -> String {
    match error {
        lopdf::Error::Parse(source) => format!("couldn't parse input: {source}"),
        lopdf::Error::IO(source) => format!("IO error: {source}"),
        other => other.to_string(),
    }
}

/// The trailer key whose value is the encryption dictionary.
const ENCRYPT_KEY: &[u8] = b"Encrypt";
/// Same length as `/Encrypt`, so swapping it in leaves every byte offset in the file (and
/// therefore the whole cross-reference table) valid. See [`load_inline_encrypted`].
const ENCRYPT_KEY_MASKED: &[u8] = b"/Encrypx";

/// The encryption dictionary written **directly** in the trailer, if that is this file's
/// shape.
///
/// Most producers write `/Encrypt 9 0 R`; MuPDF (and therefore PyMuPDF, a very common
/// producer) writes the dictionary inline: `/Encrypt<</Filter/Standard/R 4 …>>`. lopdf's
/// `is_encrypted()` only recognises the indirect form — `get_encrypted()` insists on
/// `as_reference()` — so its loader never authenticates, bails out of
/// `load_encrypted_document` with **zero objects parsed**, and hands back a document that
/// reports `is_encrypted() == false` while holding nothing at all. That is the silent blank:
/// 0 pages, empty text, an empty HTML shell, no error.
fn inline_encrypt_dict(doc: &Document) -> Option<lopdf::Dictionary> {
    match doc.trailer.get(ENCRYPT_KEY) {
        Ok(lopdf::Object::Dictionary(d)) => Some(d.clone()),
        _ => None,
    }
}

/// Rewrite every `/Encrypt<<…` trailer key to an inert same-length name.
///
/// Only a *direct* dictionary value can follow, so the `<<` lookahead cannot match the
/// ordinary `/Encrypt 9 0 R` form (nor, realistically, ciphertext). The replacement is
/// byte-for-byte the same length, so all xref offsets survive.
fn mask_inline_encrypt_keys(raw: &[u8]) -> Vec<u8> {
    let mut out = raw.to_vec();
    let key = b"/Encrypt";
    let mut i = 0;
    while let Some(hit) = out[i..].windows(key.len()).position(|w| w == key) {
        let at = i + hit;
        let mut j = at + key.len();
        while out.get(j).is_some_and(|b| b.is_ascii_whitespace()) {
            j += 1;
        }
        if out[j..].starts_with(b"<<") {
            out[at..at + key.len()].copy_from_slice(ENCRYPT_KEY_MASKED);
        }
        i = at + key.len();
    }
    out
}

/// Load and decrypt a file whose encryption dictionary sits inline in the trailer.
///
/// lopdf gives us no way to point it at a direct `/Encrypt` dictionary, so we hand it the
/// shape it does understand: mask the trailer key (offset-preserving, so the file still
/// parses) and reload, which takes the plain path and yields the full object graph with its
/// strings and streams still ciphertext; then re-attach the dictionary we already parsed as a
/// real indirect object and run lopdf's own authentication and per-object decryption over it
/// ([`decrypt_leniently`]). No re-implemented crypto, no lopdf version bump.
fn load_inline_encrypted(raw: &[u8], encrypt: lopdf::Dictionary) -> Result<Document, Error> {
    let mut doc = load_mem_deterministic(&mask_inline_encrypt_keys(raw)).map_err(|_| Error::Encrypted)?;
    let id = doc.add_object(lopdf::Object::Dictionary(encrypt));
    doc.trailer.set(ENCRYPT_KEY.to_vec(), lopdf::Object::Reference(id));
    decrypt_leniently(&mut doc, id)?;
    Ok(doc)
}

/// Decrypt every object with the empty user password, skipping the ones that will not
/// decrypt instead of failing the whole document.
///
/// This is `Document::decrypt` with lopdf's *loader* semantics rather than its API
/// semantics: `decrypt()` propagates the first per-object failure, while the loader
/// (`reader.rs::load_encrypted_document`) ignores them. Leniency is the correct behaviour
/// here — a file saved with `/EncryptMetadata false` legitimately contains an object that is
/// **not** encrypted, and blanket-decrypting it fails on padding. Authentication itself stays
/// strict: a password we cannot satisfy is still [`Error::Encrypted`].
fn decrypt_leniently(doc: &mut Document, encrypt_id: lopdf::ObjectId) -> Result<(), Error> {
    doc.authenticate_password("").map_err(|_| Error::Encrypted)?;
    let state = lopdf::EncryptionState::decode(&*doc, "").map_err(|_| Error::Encrypted)?;
    for (&id, obj) in doc.objects.iter_mut() {
        if id != encrypt_id {
            let _ = lopdf::encryption::decrypt_object(&state, id, obj);
        }
    }
    // Object streams are themselves encrypted, so their contents could not be read during the
    // load; harvest them now that the containers are plaintext.
    let mut nested = Vec::new();
    for obj in doc.objects.values_mut() {
        if let Ok(stream) = obj.as_stream_mut() {
            if stream.dict.has_type(b"ObjStm") {
                if let Ok(os) = lopdf::ObjectStream::new(stream) {
                    nested.extend(os.objects);
                }
            }
        }
    }
    for (id, obj) in nested {
        doc.objects.entry(id).or_insert(obj);
    }
    doc.trailer.remove(ENCRYPT_KEY);
    doc.objects.remove(&encrypt_id);
    Ok(())
}

/// Make sure a loaded document is actually readable, encryption-wise — the one place the
/// "protected PDF" cases are resolved, for both `open` and `from_bytes`.
///
/// Three outcomes, and none of them is the blank document this used to return:
/// * The loader already decrypted it. lopdf tries the empty user password itself, so every
///   owner-password-only file (the common "protected" PDF a reader opens without prompting)
///   arrives decrypted — RC4-40 (R2), RC4-128 (R3/R4), AES-128 (R4/AESV2), AES-256 (R6/AESV3).
/// * `/Encrypt` is still there. Either it is an indirect dictionary the loader could not use
///   (real user password, or a revision lopdf does not implement) — retry the empty password
///   explicitly and report [`Error::Encrypted`] if that fails — or it is the inline dictionary
///   of [`inline_encrypt_dict`], which [`load_inline_encrypted`] decrypts properly.
/// * Belt and braces: whatever the shape, a file that carried `/Encrypt` and still has no
///   readable page tree is exactly the silent-blank symptom, so it is an error too.
fn ensure_decrypted(raw: &[u8], doc: &mut Document) -> Result<(), Error> {
    let had_encrypt = doc.trailer.has(ENCRYPT_KEY);
    if doc.is_encrypted() {
        doc.decrypt("").map_err(|_| Error::Encrypted)?;
    } else if let Some(encrypt) = inline_encrypt_dict(doc) {
        *doc = load_inline_encrypted(raw, encrypt)?;
    }
    if had_encrypt && doc.get_pages().is_empty() {
        return Err(Error::Encrypted);
    }
    Ok(())
}

impl PdfDocument {
    fn finish_open(
        doc: Document,
        raw: Arc<[u8]>,
        source: Option<PathBuf>,
        make_access: impl FnOnce(Arc<Document>, Arc<[u8]>) -> Arc<dyn DocumentAccess>,
    ) -> Self {
        let doc = Arc::new(doc);
        let access = make_access(Arc::clone(&doc), Arc::clone(&raw));
        PdfDocument {
            doc,
            raw,
            access,
            source,
            ocr_cache: Default::default(),
        }
    }

    fn from_bytes_with_access_factory(
        data: &[u8],
        make_access: impl FnOnce(Arc<Document>, Arc<[u8]>) -> Arc<dyn DocumentAccess>,
    ) -> Result<Self, Error> {
        let raw: Arc<[u8]> = Arc::from(data);
        let mut doc = load_mem_deterministic(&raw)
            .map_err(|error| Error::Parse(eager_error_message(&error)))?;
        ensure_decrypted(&raw, &mut doc)?;
        Ok(Self::finish_open(doc, raw, None, make_access))
    }

    /// Open a PDF from a filesystem path. Only loads/parses the container.
    pub fn open(path: &str) -> Result<Self, Error> {
        let raw: Arc<[u8]> = std::fs::read(path).map_err(Error::Read)?.into();
        let mut doc = load_mem_deterministic(&raw).map_err(|e| Error::Open(eager_error_message(&e)))?;
        ensure_decrypted(&raw, &mut doc)?;
        Ok(Self::finish_open(
            doc,
            raw,
            Some(PathBuf::from(path)),
            |document, source| Arc::new(EagerDocumentAdapter::new(document, source)),
        ))
    }

    /// Open a PDF from raw bytes. There is no source path.
    pub fn from_bytes(data: &[u8]) -> Result<Self, Error> {
        Self::from_bytes_with_access_factory(data, |document, source| {
            Arc::new(EagerDocumentAdapter::new(document, source))
        })
    }

    /// Number of pages.
    pub fn page_count(&self) -> usize {
        self.access.pages().map_or(0, |pages| pages.len())
    }

    /// Extract plain text from all pages (concatenated, page order). Hybrid: our
    /// ToUnicode-aware extractor primary, lopdf fallback per page.
    ///
    /// Pages are extracted in PARALLEL — each is an independent read-only walk of the
    /// document with its own [`crate::WalkBudget`], the same property that already makes the
    /// span pass in [`crate::html`] parallel. The result is byte-identical to the sequential
    /// loop by construction, not by luck: nothing crosses pages, and the pieces are re-sorted
    /// by page number before they are joined, so completion order is never observed.
    pub fn extract_text(&self) -> String {
        let pages = self.doc.get_pages();
        let mut per_page: Vec<(u32, String)> = pages
            .par_iter()
            .map(|(&p, &page_id)| {
                let mine = text::extract_page(self.access.as_ref(), page_id, &self.raw).unwrap_or_default();
                let s = if mine.trim().chars().count() >= 2 {
                    mine
                } else {
                    self.doc.extract_text(&[p]).unwrap_or_default() // per-page lopdf fallback
                };
                (p, s)
            })
            .collect();
        per_page.sort_by_key(|(p, _)| *p);
        let mut out = String::new();
        for (_, s) in &per_page {
            out.push_str(s);
            out.push('\n');
        }
        out
    }

    /// Extract text from a single 1-indexed page (hybrid).
    pub fn extract_page_text(&self, page: u32) -> Result<String, Error> {
        let page_id = *self.doc.get_pages().get(&page).ok_or(Error::NoPage(Some(page)))?;
        let mine = text::extract_page(self.access.as_ref(), page_id, &self.raw).unwrap_or_default();
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
            out.push_str(&text::extract_page(self.access.as_ref(), page_id, &self.raw).unwrap_or_default());
            out.push('\n');
        }
        out
    }

    /// Diagnostic: raw spans (text, x, width, size) for a 1-indexed page.
    pub fn dbg_spans(&self, page: u32) -> Result<Vec<(String, f32, f32, f32)>, Error> {
        let page_id = *self.doc.get_pages().get(&page).ok_or(Error::NoPage(None))?;
        Ok(text::extract_spans(self.access.as_ref(), page_id, &self.raw)
            .into_iter()
            .map(|s| (s.text, s.x, s.width, s.size))
            .collect())
    }

    /// Diagnostic: spans with y for a 1-indexed page (text, x, y, width, size).
    #[allow(clippy::type_complexity)] // a flat diagnostic tuple mirroring the Python `_dbg_spans_xy`
    pub fn dbg_spans_xy(&self, page: u32) -> Result<Vec<(String, f32, f32, f32, f32)>, Error> {
        let page_id = *self.doc.get_pages().get(&page).ok_or(Error::NoPage(None))?;
        Ok(text::extract_spans(self.access.as_ref(), page_id, &self.raw)
            .into_iter()
            .map(|s| (s.text, s.x, s.y, s.width, s.size))
            .collect())
    }

    /// What the **figure-ink gate** did to this document: `(accepted, suppressed, pages)`.
    ///
    /// `suppressed` counts the clusters that cleared the strong SIZE bar and were demoted to
    /// weak candidates because they carry neither graphic ink nor a real palette (see
    /// `vector::passes_ink_gate`) — page furniture, ruled tables, invisible white-rect
    /// layers. A demoted cluster is **not deleted**: a figure caption beside it still
    /// promotes it back at render time, so this is the gate's reach, not a loss count.
    /// `pages` lists the 1-indexed pages where a demotion happened, in page order.
    ///
    /// Reported, never silent: the corpus gate reads this to keep the number visible per
    /// document, so a filter can never quietly start eating real figures.
    pub fn figure_gate_stats(&self) -> (u32, u32, Vec<u32>) {
        let (mut accepted, mut suppressed, mut pages) = (0u32, 0u32, Vec::new());
        let map = self.doc.get_pages();
        let mut nums: Vec<u32> = map.keys().copied().collect();
        nums.sort_unstable();
        for n in nums {
            let (strong, weak) = crate::vector::positioned_vectors(&self.doc, self.access.as_ref(), map[&n]);
            let dropped = weak.iter().filter(|v| v.demoted()).count() as u32;
            accepted += strong.len() as u32;
            suppressed += dropped;
            if dropped > 0 {
                pages.push(n);
            }
        }
        (accepted, suppressed, pages)
    }

    /// Every stream in this document whose encoded bytes did **not** decode cleanly.
    ///
    /// The answer to "is the page I just rendered the whole page?". Two decode failures in
    /// lopdf 0.40 are invisible to the reader that suffers them: a **truncated** `FlateDecode`
    /// stream is reported as `Ok` with the partial output (so a page renders short, silently),
    /// and a filter chain lopdf cannot apply — `ASCIIHexDecode` is the live case — degrades to
    /// the *encoded* bytes verbatim. Both are detected here independently of lopdf and
    /// reported per stream ([`StreamIssue`]); an intact document returns an empty list.
    ///
    /// Deliberately **on demand**, not on the render path: the truncation check costs a second
    /// full inflate per stream, a price no page should pay to answer a question almost every
    /// document answers with "nothing wrong".
    pub fn stream_integrity(&self) -> Vec<crate::pdfobj::StreamIssue> {
        crate::pdfobj::stream_issues(self.access.as_ref())
    }

    /// Diagnostic for one 1-indexed page.
    pub fn debug_page(&self, page: u32) -> Result<String, Error> {
        let page_id = *self.doc.get_pages().get(&page).ok_or(Error::NoPage(Some(page)))?;
        Ok(text::debug_page(self.access.as_ref(), page_id, &self.raw))
    }

    /// Extract images from all pages.
    pub fn extract_images(&self) -> Vec<ImageInfo> {
        extract::extract_images(&self.doc, self.access.as_ref())
    }

    /// Extract per-page font info.
    pub fn extract_fonts(&self) -> Vec<FontInfo> {
        extract::extract_fonts(self.access.as_ref())
    }

    /// Extract tables from all pages.
    pub fn extract_tables(&self) -> Vec<TableInfo> {
        extract::extract_tables(&self.doc, self.access.as_ref(), &self.raw)
    }

    /// Extract hyperlinks from all pages.
    pub fn extract_links(&self) -> Vec<links::Link> {
        links::extract_links(&self.doc)
    }

    /// Render the document to HTML.
    pub fn render(&self, mode: html::Mode, images: bool, toc: bool) -> String {
        html::to_html(&self.doc, self.access.as_ref(), &self.raw, mode, images, toc)
    }

    /// The detected-heading outline: `(level, title, page, anchor_id)` in reading order.
    pub fn toc(&self, mode: html::Mode) -> Vec<(u8, String, u32, String)> {
        nav::toc(&html::to_html(&self.doc, self.access.as_ref(), &self.raw, mode, false, true))
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
        nav::section(&html::to_html(&self.doc, self.access.as_ref(), &self.raw, mode, images, true), name)
    }

    /// Structured front-matter of an academic paper (page 1).
    pub fn front_matter(&self) -> frontmatter::FrontMatter {
        frontmatter::extract_front_matter(&self.doc, self.access.as_ref(), &self.raw)
    }

    /// OCR plan: per page, whether OCR is needed and (if so) the page raster bytes.
    pub fn ocr_plan(&self) -> Vec<OcrPlanEntry> {
        let mut out = Vec::new();
        for (&pno, &page_id) in &self.doc.get_pages() {
            let decision = ocr::detect::decide(&self.doc, self.access.as_ref(), page_id, &self.raw);
            let needs = !matches!(decision, ocr::detect::OcrDecision::NotNeeded);
            let (w, h) = ocr::page_size_pts(self.access.as_ref(), page_id);
            let image = if needs {
                ocr::page_main_image(&self.doc, self.access.as_ref(), page_id).map(|(b, _)| b)
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
            let mut doc = load_mem_deterministic(&self.raw).map_err(|e| e.to_string())?;
            let (helv, helv_b) = ocr::pdf::add_fonts(&mut doc);
            let pages = doc.get_pages();
            for (&pno, &page_id) in &pages {
                let Some(dt) = ocr.get(&pno) else { continue };
                let (w, h) = ocr::page_size_pts(self.access.as_ref(), page_id);
                if remove_raster {
                    // Clean reflow: replace the page's content with our text + cropped figures.
                    let image = ocr::page_main_image(&self.doc, self.access.as_ref(), page_id).map(|(_, img)| img);
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
        let (model, asset_bytes) = model::build::build_model(
            &self.doc,
            self.access.as_ref(),
            &self.raw,
            &file,
            generated_at,
            opts.profile,
        );
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
pub(crate) mod tests {
    use super::*;
    use crate::access::tests::{AccessCounts, FaultAccess, FaultPoint};
    use std::sync::atomic::Ordering;

    /// The owned encrypted fixtures (`tests/gen_fixtures.py::gen_encrypted`). They live in
    /// their own subfolder so the Python whole-fixture-set sweeps skip them.
    fn enc_fixture(name: &str) -> String {
        format!("{}/../tests/fixtures_pdf/encrypted/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    const ENC_SENTENCE: &str = "Encrypted fixture sentinel phrase for distillPDF.";

    #[test]
    fn actual_access_factory_opens_once_and_a_faulted_consumer_never_retries() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures_pdf/sec_structure.pdf"
        ))
        .unwrap();
        let counts = Arc::new(AccessCounts::default());
        let factory_counts = Arc::clone(&counts);
        let document = PdfDocument::from_bytes_with_access_factory(
            &bytes,
            move |eager_document, source| {
                factory_counts.opens.fetch_add(1, Ordering::Relaxed);
                let eager: Arc<dyn DocumentAccess> = Arc::new(EagerDocumentAdapter::new(
                    eager_document,
                    source,
                ));
                Arc::new(FaultAccess::new(
                    eager,
                    Some(FaultPoint::Pages),
                    Arc::clone(&factory_counts),
                ))
            },
        )
        .unwrap();
        assert_eq!(counts.opens.load(Ordering::Relaxed), 1);
        assert_eq!(document.page_count(), 0);
        assert_eq!(document.page_count(), 0);
        assert_eq!(counts.opens.load(Ordering::Relaxed), 1, "no eager retry");
        assert_eq!(counts.page_reads.load(Ordering::Relaxed), 2);
        let _ = document.stream_integrity();
        assert_eq!(counts.object_lists.load(Ordering::Relaxed), 1);
        assert_eq!(counts.opens.load(Ordering::Relaxed), 1, "no fallback opener");
    }

    /// Every committed fixture PDF, in sorted path order — the sweep corpus a unit test uses
    /// when its claim is about *all* documents (parallelism agreeing with the sequential path,
    /// a short-circuit reporting what the full walk reports) rather than one authored shape.
    /// Shared with `extract`'s sweeps so the list is derived once.
    pub(crate) fn fixture_pdfs() -> Vec<std::path::PathBuf> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf");
        let mut out: Vec<std::path::PathBuf> = Vec::new();
        for d in [std::path::PathBuf::from(dir), std::path::Path::new(dir).join("adversarial")] {
            let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(&d)
                .expect("fixture dir readable")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf")))
                .collect();
            found.sort();
            out.append(&mut found);
        }
        assert!(out.len() > 40, "expected the full fixture corpus, got {}", out.len());
        out
    }

    /// Build a PDF that makes lopdf's object-stream merge race observable: object 5 is defined
    /// **twice**, in two different `/Type /ObjStm` containers with different values, and the
    /// xref does not list it as a compressed object — so lopdf's container filter keeps both
    /// copies and the winner is decided by rayon thread-completion order. The filler objects
    /// exist only to give the parallel loader enough entries to actually split the work.
    fn racing_objstm_pdf() -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut offs: Vec<(u32, usize)> = Vec::new();
        out.extend_from_slice(b"%PDF-1.5\n");
        let obj = |out: &mut Vec<u8>, offs: &mut Vec<(u32, usize)>, n: u32, body: &str| {
            offs.push((n, out.len()));
            out.extend_from_slice(format!("{n} 0 obj\n{body}\nendobj\n").as_bytes());
        };
        obj(&mut out, &mut offs, 1, "<< /Type /Catalog /Pages 2 0 R >>");
        obj(&mut out, &mut offs, 2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
        obj(&mut out, &mut offs, 3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
        for n in 100..400u32 {
            obj(&mut out, &mut offs, n, "<< /Filler true >>");
        }
        // Two object streams, each claiming object 5 with a different structure type.
        for (n, val) in [(10u32, "Figure"), (20u32, "Artifact")] {
            let data = format!("5 0 << /S /{val} >>");
            offs.push((n, out.len()));
            out.extend_from_slice(
                format!(
                    "{n} 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n{data}\nendstream\nendobj\n",
                    data.len()
                )
                .as_bytes(),
            );
        }
        let startxref = out.len();
        let maxid = 400u32;
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", maxid + 1).as_bytes());
        for id in 1..=maxid {
            match offs.iter().find(|(n, _)| *n == id) {
                Some((_, o)) => out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes()),
                None => out.extend_from_slice(b"0000000000 65535 f \n"),
            }
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF", maxid + 1).as_bytes(),
        );
        out
    }

    /// The same bytes must always load to the same object map.
    ///
    /// lopdf resolves an object number defined in two object streams by *thread-completion
    /// order*, so on stock `Document::load_mem` this fixture loads two different ways at
    /// roughly 50/50 (measured: 30/30 over 60 loads, and 3 distinct maps over 40 loads of a
    /// real USGS file, which flipped a table header in and out of `to_html`). Everything in
    /// this crate loads through [`load_mem_deterministic`], which confines that race to a
    /// private one-thread pool. A regression here means a call site went back to
    /// `Document::load_mem` — or that our pool stopped covering lopdf's `par_iter`.
    #[test]
    fn the_same_bytes_always_load_to_the_same_object_map() {
        let raw = racing_objstm_pdf();
        let fingerprint = |doc: &Document| -> String {
            doc.objects.iter().map(|(id, o)| format!("{id:?}={o:?};")).collect()
        };
        let first = fingerprint(&load_mem_deterministic(&raw).expect("fixture loads"));
        for i in 1..60 {
            let got = fingerprint(&load_mem_deterministic(&raw).expect("fixture loads"));
            assert_eq!(got, first, "object map changed on load {i} of the same bytes");
        }
        // Guard the fixture itself: if lopdf ever stops keeping both copies, this test would
        // pass vacuously and stop protecting anything.
        assert!(first.contains("/S"), "fixture no longer exercises the object-stream merge");
    }

    #[test]
    fn loader_pool_failure_is_fail_closed_retryable_reused_and_rayon_safe() {
        let raw = racing_objstm_pdf();
        let pool = LoadPool::new();

        // Rayon exposes a spawn hook, so fail construction directly and deterministically
        // instead of hoping to exhaust the machine's real thread allowance. Parsing must not
        // proceed unscoped, and the failed attempt must not poison/cache the empty cell.
        let failed = load_mem_with_pool(&raw, &pool, || {
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .spawn_handler(|_| Err(std::io::Error::other("injected loader thread failure")))
                .build()
        });
        let err = failed.expect_err("pool creation failure must fail the load");
        assert!(matches!(err, lopdf::Error::IO(_)), "pool failure must remain an IO error: {err}");
        let lopdf::Error::IO(source) = &err else { unreachable!() };
        assert!(source.to_string().contains("injected loader thread failure"));
        assert!(pool.pool.get().is_none(), "a transient build failure must leave the pool retryable");

        // Initialize while already executing in another rayon registry. `ThreadPool::install`
        // has an explicit cross-registry path; importantly, get_or_build releases `init` before
        // entering it, so the private one-thread pool cannot wait while holding our mutex.
        let outer = rayon::ThreadPoolBuilder::new().num_threads(2).build().expect("outer pool builds");
        let first = outer
            .install(|| {
                load_mem_with_pool(&raw, &pool, || {
                    rayon::ThreadPoolBuilder::new()
                        .num_threads(1)
                        .thread_name(|_| "distillpdf-test-load".to_string())
                        .build()
                })
            })
            .expect("a later load retries successfully inside rayon");
        assert!(pool.pool.get().is_some(), "successful retry must cache the pool");

        // Once initialized, the lock-free fast path must not invoke another builder.
        let again = load_mem_with_pool(&raw, &pool, || -> Result<_, rayon::ThreadPoolBuildError> {
            panic!("cached loader pool should be reused")
        })
        .expect("cached pool loads");
        let fingerprint = |doc: &Document| -> String {
            doc.objects.iter().map(|(id, o)| format!("{id:?}={o:?};")).collect()
        };
        assert_eq!(fingerprint(&again), fingerprint(&first));
    }

    #[test]
    fn parallel_page_text_joins_exactly_as_the_sequential_loop_did() {
        // `extract_text` fans its pages out over rayon. The join must not be able to observe
        // completion order, so compare it against an independent sequential oracle written
        // here — and repeat it, because a nondeterministic order shows up as an occasional
        // disagreement, not a permanent one (`img::cluster` cost this project a whole batch
        // of byte-identical claims by returning a HashMap's values).
        let mut multipage = 0usize;
        for path in fixture_pdfs() {
            let Ok(pdf) = PdfDocument::open(path.to_str().expect("utf-8 fixture path")) else {
                continue; // encrypted / deliberately damaged
            };
            let mut want = String::new();
            for (&p, &page_id) in &pdf.doc.get_pages() {
                let mine = text::extract_page(pdf.access.as_ref(), page_id, &pdf.raw).unwrap_or_default();
                if mine.trim().chars().count() >= 2 {
                    want.push_str(&mine);
                } else {
                    want.push_str(&pdf.doc.extract_text(&[p]).unwrap_or_default());
                }
                want.push('\n');
            }
            if pdf.page_count() > 1 {
                multipage += 1;
            }
            for run in 0..5 {
                assert_eq!(pdf.extract_text(), want, "run {run} of {} disagrees with the sequential join", path.display());
            }
        }
        assert!(multipage >= 10, "the sweep must cover documents with pages to parallelise, got {multipage}");
    }

    #[test]
    fn owner_password_only_files_open_and_extract() {
        // Empty user password + an owner password — the "protected" PDF every reader opens
        // without prompting. One file per scheme lopdf 0.40 supports, in BOTH trailer shapes:
        // `/Encrypt 9 0 R` and the MuPDF/PyMuPDF `/Encrypt<<…>>` inline dictionary, which used
        // to load as a document with no objects at all.
        for name in [
            "rc4_40.pdf",
            "rc4_128.pdf",
            "aes_128.pdf",
            "aes_256.pdf",
            "inline_encrypt_aes_128.pdf",
            "inline_encrypt_rc4_128.pdf",
        ] {
            let path = enc_fixture(name);
            let doc = PdfDocument::open(&path).unwrap_or_else(|e| panic!("{name} must open: {e}"));
            assert!(doc.page_count() > 0, "{name}: no pages");
            let text = doc.extract_text();
            assert!(text.contains(ENC_SENTENCE), "{name}: text was {text:?}");
            // The same file through the bytes path.
            let bytes = std::fs::read(&path).unwrap();
            let from_bytes = PdfDocument::from_bytes(&bytes).unwrap_or_else(|e| panic!("{name} from_bytes: {e}"));
            assert!(from_bytes.extract_text().contains(ENC_SENTENCE), "{name}: from_bytes text mismatch");
        }
    }

    #[test]
    fn user_password_file_errors_instead_of_returning_blank() {
        // Both trailer shapes: the indirect `/Encrypt 7 0 R` file and the inline-dictionary
        // one. Neither may come back as a readable-looking, empty document.
        for name in ["userpw.pdf", "inline_encrypt_userpw.pdf"] {
            let path = enc_fixture(name);
            let err = PdfDocument::open(&path).err().unwrap_or_else(|| panic!("{name} must not open"));
            assert!(matches!(err, Error::Encrypted), "{name}: got {err:?}");
            assert_eq!(
                err.to_string(),
                "encrypted PDF: needs a password, or uses an encryption scheme distillpdf cannot decrypt"
            );
            let bytes = std::fs::read(&path).unwrap();
            let err = PdfDocument::from_bytes(&bytes).err().unwrap_or_else(|| panic!("{name} from_bytes must fail"));
            assert!(matches!(err, Error::Encrypted), "{name}: got {err:?}");
        }
    }

    #[test]
    fn masking_only_touches_a_direct_encrypt_dictionary() {
        // The indirect form must survive untouched — masking it would strip a file's
        // encryption dictionary out of the trailer for no reason.
        let indirect = b"trailer << /Root 1 0 R /Encrypt 9 0 R >>".to_vec();
        assert_eq!(mask_inline_encrypt_keys(&indirect), indirect);
        let inline = b"trailer << /Root 1 0 R /Encrypt<< /Filter /Standard >> >>";
        let masked = mask_inline_encrypt_keys(inline);
        assert_eq!(masked.len(), inline.len(), "masking must preserve every byte offset");
        assert!(masked.windows(8).all(|w| w != b"/Encrypt"));
        assert!(masked.windows(8).any(|w| w == b"/Encrypx"));
        // Whitespace between the key and the dictionary is still the direct form.
        assert!(mask_inline_encrypt_keys(b"/Encrypt\n<< /R 4 >>").starts_with(b"/Encrypx"));
    }

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
