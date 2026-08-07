//! The pure-Rust document core — `PdfDocument` and its typed operations.
//!
//! This is the layer the PyO3 binding (`src/lib.rs`) is a thin wrapper over, and the future
//! public surface a Rust embedder (kglite's `knowledge_tree`) consumes: a reusable handle
//! (`open`/`from_bytes`), typed results, structured [`Error`]s, typed `.dpdf` load, and a
//! typed [`DistillOptions`]. NO pyo3 appears here — the binding does all Python-object
//! assembly and maps [`Error`] → `PyValueError`.

use lopdf::{BytesSource, Document, FileSource, RandomAccessSource, SourceError, SourceResult};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::access::{
    AccessError, DocumentAccess, EagerDocumentAdapter, IndexedAdapterCounters,
    IndexedDocumentAdapter, PageRef,
};
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
    /// Runtime-selectable immutable access route. L2 uses the eager oracle adapter; L3 adds
    /// the bounded indexed implementation without reopening consumer signatures.
    pub(crate) access: Arc<dyn DocumentAccess>,
    #[allow(dead_code)] // internal route provenance becomes public only after API approval
    diagnostics: Arc<RouteDiagnostics>,
    /// Source path (`open`); `None` when constructed from bytes.
    pub(crate) source: Option<PathBuf>,
    /// Cached OCR results: `{1-based page: DocTags}`, populated once by `set_ocr`.
    pub(crate) ocr_cache: Mutex<HashMap<u32, String>>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenRoute {
    EagerFile,
    EagerBytes,
    IndexedFile,
    IndexedBytes,
    IndexedSnapshot,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenReason {
    PublicCompatibility,
    InternalMeasurement,
    ExplicitSnapshot,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceMode {
    FileDescriptor,
    SharedBytes,
    EagerMaterializedFile,
    FullSnapshot,
}

#[derive(Default)]
struct RouteSourceCounters {
    requests: AtomicU64,
    reads: AtomicU64,
    max_request: AtomicU64,
}

struct ObservedSource {
    inner: Arc<dyn RandomAccessSource>,
    counters: Arc<RouteSourceCounters>,
}

impl RandomAccessSource for ObservedSource {
    fn len(&self) -> SourceResult<u64> {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.len()
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
        const PHYSICAL_READ_BYTES: usize = 64 * 1024;
        let mut read = 0;
        while read < out.len() {
            let request = (out.len() - read).min(PHYSICAL_READ_BYTES);
            let physical_offset = offset.checked_add(read as u64).ok_or(
                SourceError::RangeOverflow {
                    offset,
                    length: out.len() as u64,
                },
            )?;
            self.counters.requests.fetch_add(1, Ordering::Relaxed);
            self.counters.reads.fetch_add(1, Ordering::Relaxed);
            self.counters
                .max_request
                .fetch_max(request as u64, Ordering::Relaxed);
            let actual = self
                .inner
                .read_at(physical_offset, &mut out[read..read + request])?;
            if actual > request {
                return Err(SourceError::InvalidReadCount {
                    returned: actual,
                    buffer_len: request,
                });
            }
            read += actual;
            if actual < request {
                break;
            }
        }
        Ok(read)
    }

    fn validate_unchanged(&self) -> SourceResult<()> {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.validate_unchanged()
    }
}

#[allow(dead_code)]
pub(crate) struct RouteDiagnostics {
    pub(crate) route: OpenRoute,
    pub(crate) reason: OpenReason,
    pub(crate) source_mode: SourceMode,
    eager_opens: AtomicU64,
    indexed_opens: AtomicU64,
    fallback_opens: AtomicU64,
    source: Arc<RouteSourceCounters>,
    indexed: OnceLock<Arc<IndexedAdapterCounters>>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteDiagnosticsSnapshot {
    pub(crate) route: OpenRoute,
    pub(crate) reason: OpenReason,
    pub(crate) source_mode: SourceMode,
    pub(crate) eager_opens: u64,
    pub(crate) indexed_opens: u64,
    pub(crate) fallback_opens: u64,
    pub(crate) source_requests: u64,
    pub(crate) source_reads: u64,
    pub(crate) source_max_request: u64,
    pub(crate) page_map_builds: u64,
    pub(crate) index_estimated_bytes: u64,
    pub(crate) index_objects: u64,
    pub(crate) index_pages: u64,
    pub(crate) document_object_o_admitted_bytes: u64,
}

#[allow(dead_code)]
impl RouteDiagnostics {
    fn new(route: OpenRoute, reason: OpenReason, source_mode: SourceMode) -> Arc<Self> {
        Arc::new(Self {
            route,
            reason,
            source_mode,
            eager_opens: AtomicU64::new(0),
            indexed_opens: AtomicU64::new(0),
            fallback_opens: AtomicU64::new(0),
            source: Arc::new(RouteSourceCounters::default()),
            indexed: OnceLock::new(),
        })
    }

    pub(crate) fn snapshot(&self) -> RouteDiagnosticsSnapshot {
        let indexed = self.indexed.get();
        RouteDiagnosticsSnapshot {
            route: self.route,
            reason: self.reason,
            source_mode: self.source_mode,
            eager_opens: self.eager_opens.load(Ordering::Relaxed),
            indexed_opens: self.indexed_opens.load(Ordering::Relaxed),
            fallback_opens: self.fallback_opens.load(Ordering::Relaxed),
            source_requests: self.source.requests.load(Ordering::Relaxed),
            source_reads: self.source.reads.load(Ordering::Relaxed),
            source_max_request: self.source.max_request.load(Ordering::Relaxed),
            page_map_builds: indexed.map_or(0, |counters| {
                counters.page_map_builds.load(Ordering::Relaxed)
            }),
            index_estimated_bytes: indexed.map_or(0, |counters| {
                counters.index_estimated_bytes.load(Ordering::Relaxed)
            }),
            index_objects: indexed
                .map_or(0, |counters| counters.index_objects.load(Ordering::Relaxed)),
            index_pages: indexed
                .map_or(0, |counters| counters.index_pages.load(Ordering::Relaxed)),
            document_object_o_admitted_bytes: indexed.map_or(0, |counters| {
                counters
                    .retained_object_admitted_bytes
                    .load(Ordering::Relaxed)
            }),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum RouteFailure {
    Source(SourceError),
    Access(AccessError),
}

#[allow(dead_code)]
pub(crate) struct RouteOpenError {
    pub(crate) failure: RouteFailure,
    pub(crate) diagnostics: Arc<RouteDiagnostics>,
}

impl std::fmt::Debug for RouteOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RouteOpenError")
            .field("failure", &self.failure)
            .field("route", &self.diagnostics.route)
            .finish()
    }
}

#[allow(dead_code)]
pub(crate) struct IndexedOpenControl {
    access: Arc<IndexedDocumentAdapter>,
    diagnostics: Arc<RouteDiagnostics>,
    source_owner: Option<Arc<[u8]>>,
}

#[allow(dead_code)]
impl IndexedOpenControl {
    pub(crate) fn diagnostics(&self) -> RouteDiagnosticsSnapshot {
        self.diagnostics.snapshot()
    }

    pub(crate) fn pages(&self) -> Result<Vec<PageRef>, AccessError> {
        self.access.pages()
    }

    pub(crate) fn source_sha256(&self) -> Result<String, AccessError> {
        self.access.source_sha256()
    }

    #[cfg(test)]
    fn shared_bytes(&self) -> Option<&Arc<[u8]>> {
        self.source_owner.as_ref()
    }

    #[cfg(test)]
    fn check_page_content(&self, page: lopdf::ObjectId) -> Result<(), AccessError> {
        self.access.page_content(page).map(|_| ())
    }

    #[cfg(test)]
    fn checked_page_content_matches(
        &self,
        page: lopdf::ObjectId,
        expected: &[u8],
    ) -> Result<bool, AccessError> {
        self.access
            .page_content(page)
            .map(|content| content.as_ref() == expected)
    }

    #[cfg(test)]
    fn checked_recovered_stream_matches(
        &self,
        object: u32,
        expected: &[u8],
    ) -> Result<bool, AccessError> {
        self.access
            .recover_source_stream(object)
            .map(|stream| stream.is_some_and(|stream| stream.as_ref() == expected))
    }

    #[cfg(test)]
    fn checked_fingerprint(&self) -> Result<String, AccessError> {
        checked_access_fingerprint(self.access.as_ref())
    }
}

#[cfg(test)]
fn canonical_object(value: &lopdf::Object, output: &mut Vec<u8>) {
    use lopdf::Object;
    match value {
        Object::Null => output.extend_from_slice(b"null;"),
        Object::Boolean(value) => output.extend_from_slice(if *value { b"true;" } else { b"false;" }),
        Object::Integer(value) => output.extend_from_slice(format!("i{value};").as_bytes()),
        Object::Real(value) => output.extend_from_slice(format!("r{value:?};").as_bytes()),
        Object::Name(value) => {
            output.extend_from_slice(b"n");
            output.extend_from_slice(format!("{}:", value.len()).as_bytes());
            output.extend_from_slice(value);
        }
        Object::String(value, format) => {
            output.extend_from_slice(format!("s{format:?}:{}:", value.len()).as_bytes());
            output.extend_from_slice(value);
        }
        Object::Array(values) => {
            output.extend_from_slice(format!("a{}[", values.len()).as_bytes());
            for value in values {
                canonical_object(value, output);
            }
            output.extend_from_slice(b"]");
        }
        Object::Dictionary(dictionary) => canonical_dictionary(dictionary, output),
        Object::Stream(stream) => {
            output.extend_from_slice(b"stream{");
            canonical_dictionary(&stream.dict, output);
            output.extend_from_slice(format!("bytes{}:", stream.content.len()).as_bytes());
            output.extend_from_slice(&stream.content);
            output.extend_from_slice(b"}");
        }
        Object::Reference((object, generation)) => {
            output.extend_from_slice(format!("ref{object}:{generation};").as_bytes());
        }
    }
}

#[cfg(test)]
fn canonical_dictionary(dictionary: &lopdf::Dictionary, output: &mut Vec<u8>) {
    let mut entries: Vec<_> = dictionary.iter().collect();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    output.extend_from_slice(format!("d{}{{", entries.len()).as_bytes());
    for (key, value) in entries {
        output.extend_from_slice(format!("k{}:", key.len()).as_bytes());
        output.extend_from_slice(key);
        canonical_object(value, output);
    }
    output.extend_from_slice(b"}");
}

#[cfg(test)]
fn checked_access_fingerprint(access: &dyn DocumentAccess) -> Result<String, AccessError> {
    use sha2::{Digest, Sha256};

    let mut canonical = Vec::new();
    {
        let root = access.trailer_entry(b"Root")?;
        canonical.extend_from_slice(b"trailer-root:");
        root.read(|value| canonical_object(value, &mut canonical))?;
    }

    let catalog = access.catalog()?;
    canonical.extend_from_slice(b"catalog:");
    catalog.read(|dictionary| canonical_dictionary(dictionary, &mut canonical))?;
    if catalog.read(|dictionary| dictionary.has(b"Probe"))? {
        canonical.extend_from_slice(b"probe:");
        catalog
            .entry(access, b"Probe")?
            .read(|value| canonical_object(value, &mut canonical))?;
    }

    let object_ids = access.object_ids();
    canonical.extend_from_slice(format!("objects{}:", object_ids.len()).as_bytes());
    for (object, generation) in object_ids {
        canonical.extend_from_slice(format!("{object}:{generation};").as_bytes());
    }

    let pages = access.pages()?;
    canonical.extend_from_slice(format!("pages{}:", pages.len()).as_bytes());
    for page in pages {
        canonical.extend_from_slice(
            format!("page{}@{}:{};", page.number, page.id.0, page.id.1).as_bytes(),
        );
        let content = access.page_content(page.id)?;
        canonical.extend_from_slice(format!("content{}:", content.len()).as_bytes());
        canonical.extend_from_slice(content.as_ref());

        let resources = access.page_resource_chain(page.id)?;
        canonical.extend_from_slice(format!("resources{}:", resources.len()).as_bytes());
        for resource in resources {
            resource.read(|dictionary| canonical_dictionary(dictionary, &mut canonical))?;
        }

        let fallback = access.fallback_page_text(page.number)?;
        canonical.extend_from_slice(format!("text{}:", fallback.len()).as_bytes());
        canonical.extend_from_slice(fallback.as_bytes());
    }
    Ok(format!("{:x}", Sha256::digest(canonical)))
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
pub(crate) fn load_mem_deterministic(raw: &[u8]) -> Result<Document, lopdf::Error> {
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

fn open_indexed_source(
    source: Arc<dyn RandomAccessSource>,
    source_owner: Option<Arc<[u8]>>,
    diagnostics: Arc<RouteDiagnostics>,
    password: Option<Vec<u8>>,
) -> Result<IndexedOpenControl, RouteOpenError> {
    diagnostics.indexed_opens.store(1, Ordering::Relaxed);
    let observed: Arc<dyn RandomAccessSource> = Arc::new(ObservedSource {
        inner: source,
        counters: Arc::clone(&diagnostics.source),
    });
    let access = IndexedDocumentAdapter::open(observed, password).map_err(|failure| {
        RouteOpenError {
            failure: RouteFailure::Access(failure),
            diagnostics: Arc::clone(&diagnostics),
        }
    })?;
    let access = Arc::new(access);
    assert!(
        diagnostics.indexed.set(access.counters()).is_ok(),
        "route diagnostics indexed counters are assigned once"
    );
    Ok(IndexedOpenControl {
        access,
        diagnostics,
        source_owner,
    })
}

#[allow(dead_code)] // internal L3 route authority; public constructors remain eager
pub(crate) fn open_indexed_file_internal(
    path: &Path,
    password: Option<Vec<u8>>,
) -> Result<IndexedOpenControl, RouteOpenError> {
    let diagnostics = RouteDiagnostics::new(
        OpenRoute::IndexedFile,
        OpenReason::InternalMeasurement,
        SourceMode::FileDescriptor,
    );
    let source: Arc<dyn RandomAccessSource> = Arc::new(FileSource::open(path).map_err(|failure| {
        RouteOpenError {
            failure: RouteFailure::Source(failure),
            diagnostics: Arc::clone(&diagnostics),
        }
    })?);
    open_indexed_source(
        source,
        None,
        diagnostics,
        password,
    )
}

#[allow(dead_code)] // internal L3 route authority; public constructors remain eager
pub(crate) fn open_indexed_bytes_internal(
    bytes: Arc<[u8]>,
    password: Option<Vec<u8>>,
) -> Result<IndexedOpenControl, RouteOpenError> {
    let diagnostics = RouteDiagnostics::new(
        OpenRoute::IndexedBytes,
        OpenReason::InternalMeasurement,
        SourceMode::SharedBytes,
    );
    let source: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(Arc::clone(&bytes)));
    open_indexed_source(
        source,
        Some(bytes),
        diagnostics,
        password,
    )
}

#[allow(dead_code)] // the only indexed route authorized to materialize a complete file
pub(crate) fn open_indexed_snapshot_internal(
    path: &Path,
    password: Option<Vec<u8>>,
) -> Result<IndexedOpenControl, RouteOpenError> {
    let diagnostics = RouteDiagnostics::new(
        OpenRoute::IndexedSnapshot,
        OpenReason::ExplicitSnapshot,
        SourceMode::FullSnapshot,
    );
    let bytes: Arc<[u8]> = std::fs::read(path)
        .map_err(SourceError::Io)
        .map_err(|failure| RouteOpenError {
            failure: RouteFailure::Source(failure),
            diagnostics: Arc::clone(&diagnostics),
        })?
        .into();
    let source: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(Arc::clone(&bytes)));
    open_indexed_source(
        source,
        Some(bytes),
        diagnostics,
        password,
    )
}

impl PdfDocument {
    fn finish_open(
        access: Arc<dyn DocumentAccess>,
        diagnostics: Arc<RouteDiagnostics>,
        source: Option<PathBuf>,
    ) -> Self {
        PdfDocument {
            access,
            diagnostics,
            source,
            ocr_cache: Default::default(),
        }
    }

    fn finish_eager_open(
        doc: Document,
        raw: Arc<[u8]>,
        source: Option<PathBuf>,
        diagnostics: Arc<RouteDiagnostics>,
        make_access: impl FnOnce(Arc<Document>, Arc<[u8]>) -> Arc<dyn DocumentAccess>,
    ) -> Self {
        let doc = Arc::new(doc);
        let access = make_access(Arc::clone(&doc), raw);
        Self::finish_open(access, diagnostics, source)
    }

    fn from_bytes_with_access_factory(
        data: &[u8],
        make_access: impl FnOnce(Arc<Document>, Arc<[u8]>) -> Arc<dyn DocumentAccess>,
    ) -> Result<Self, Error> {
        let diagnostics = RouteDiagnostics::new(
            OpenRoute::EagerBytes,
            OpenReason::PublicCompatibility,
            SourceMode::SharedBytes,
        );
        diagnostics.eager_opens.store(1, Ordering::Relaxed);
        let raw: Arc<[u8]> = Arc::from(data);
        let mut doc = load_mem_deterministic(&raw)
            .map_err(|error| Error::Parse(eager_error_message(&error)))?;
        ensure_decrypted(&raw, &mut doc)?;
        Ok(Self::finish_eager_open(
            doc,
            raw,
            None,
            diagnostics,
            make_access,
        ))
    }

    /// Open a PDF from a filesystem path. Only loads/parses the container.
    pub fn open(path: &str) -> Result<Self, Error> {
        let diagnostics = RouteDiagnostics::new(
            OpenRoute::EagerFile,
            OpenReason::PublicCompatibility,
            SourceMode::EagerMaterializedFile,
        );
        diagnostics.eager_opens.store(1, Ordering::Relaxed);
        let raw: Arc<[u8]> = std::fs::read(path).map_err(Error::Read)?.into();
        let mut doc = load_mem_deterministic(&raw).map_err(|e| Error::Open(eager_error_message(&e)))?;
        ensure_decrypted(&raw, &mut doc)?;
        Ok(Self::finish_eager_open(
            doc,
            raw,
            Some(PathBuf::from(path)),
            diagnostics,
            |document, source| Arc::new(EagerDocumentAdapter::new(document, source)),
        ))
    }

    /// Open a PDF from raw bytes. There is no source path.
    pub fn from_bytes(data: &[u8]) -> Result<Self, Error> {
        Self::from_bytes_with_access_factory(data, |document, source| {
            Arc::new(EagerDocumentAdapter::new(document, source))
        })
    }

    #[cfg(test)]
    fn route_diagnostics(&self) -> RouteDiagnosticsSnapshot {
        self.diagnostics.snapshot()
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
        let pages = self.access.pages_or_empty();
        let mut per_page: Vec<(u32, String)> = pages
            .par_iter()
            .map(|page| {
                let mine = text::extract_page(self.access.as_ref(), page.id).unwrap_or_default();
                let s = if mine.trim().chars().count() >= 2 {
                    mine
                } else {
                    self.access.fallback_page_text_or_empty(page.number)
                };
                (page.number, s)
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
        let page_id = self.access.pages_or_empty().into_iter()
            .find(|entry| entry.number == page).map(|entry| entry.id)
            .ok_or(Error::NoPage(Some(page)))?;
        let mine = text::extract_page(self.access.as_ref(), page_id).unwrap_or_default();
        Ok(if mine.trim().chars().count() >= 2 {
            mine
        } else {
            self.access.fallback_page_text_or_empty(page)
        })
    }

    /// Diagnostic: force our ToUnicode extractor for all pages.
    pub fn mine_text(&self) -> String {
        let mut out = String::new();
        for page in self.access.pages_or_empty() {
            out.push_str(&text::extract_page(self.access.as_ref(), page.id).unwrap_or_default());
            out.push('\n');
        }
        out
    }

    /// Diagnostic: raw spans (text, x, width, size) for a 1-indexed page.
    pub fn dbg_spans(&self, page: u32) -> Result<Vec<(String, f32, f32, f32)>, Error> {
        let page_id = self.access.pages_or_empty().into_iter()
            .find(|entry| entry.number == page).map(|entry| entry.id)
            .ok_or(Error::NoPage(None))?;
        Ok(text::extract_spans(self.access.as_ref(), page_id)
            .map_err(|error| Error::Model(error.to_string()))?
            .into_iter()
            .map(|s| (s.text, s.x, s.width, s.size))
            .collect())
    }

    /// Diagnostic: spans with y for a 1-indexed page (text, x, y, width, size).
    #[allow(clippy::type_complexity)] // a flat diagnostic tuple mirroring the Python `_dbg_spans_xy`
    pub fn dbg_spans_xy(&self, page: u32) -> Result<Vec<(String, f32, f32, f32, f32)>, Error> {
        let page_id = self.access.pages_or_empty().into_iter()
            .find(|entry| entry.number == page).map(|entry| entry.id)
            .ok_or(Error::NoPage(None))?;
        Ok(text::extract_spans(self.access.as_ref(), page_id)
            .map_err(|error| Error::Model(error.to_string()))?
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
        let mut page_map = self.access.pages_or_empty();
        page_map.sort_by_key(|page| page.number);
        for page in page_map {
            let (strong, weak) = crate::vector::positioned_vectors(self.access.as_ref(), page.id);
            let dropped = weak.iter().filter(|v| v.demoted()).count() as u32;
            accepted += strong.len() as u32;
            suppressed += dropped;
            if dropped > 0 {
                pages.push(page.number);
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
        let page_id = self.access.pages_or_empty().into_iter()
            .find(|entry| entry.number == page).map(|entry| entry.id)
            .ok_or(Error::NoPage(Some(page)))?;
        text::debug_page(self.access.as_ref(), page_id)
            .map_err(|error| Error::Model(error.to_string()))
    }

    /// Extract images from all pages.
    pub fn extract_images(&self) -> Vec<ImageInfo> {
        extract::extract_images(self.access.as_ref())
    }

    /// Extract per-page font info.
    pub fn extract_fonts(&self) -> Vec<FontInfo> {
        extract::extract_fonts(self.access.as_ref())
    }

    /// Extract tables from all pages.
    pub fn extract_tables(&self) -> Vec<TableInfo> {
        extract::extract_tables(self.access.as_ref())
    }

    /// Extract hyperlinks from all pages.
    pub fn extract_links(&self) -> Vec<links::Link> {
        links::extract_links(self.access.as_ref())
    }

    /// Render the document to HTML.
    pub fn render(&self, mode: html::Mode, images: bool, toc: bool) -> String {
        html::to_html(self.access.as_ref(), mode, images, toc)
    }

    /// The detected-heading outline: `(level, title, page, anchor_id)` in reading order.
    pub fn toc(&self, mode: html::Mode) -> Vec<(u8, String, u32, String)> {
        nav::toc(&html::to_html(self.access.as_ref(), mode, false, true))
    }

    /// The PDF's OWN `/Outlines` bookmarks as `(level, title, page, anchor)`.
    pub fn outline(&self) -> Vec<(u8, String, u32, String)> {
        links::outline(self.access.as_ref())
            .into_iter()
            .map(|e| ((e.level + 1), e.title, e.page, format!("page-{}", e.page)))
            .collect()
    }

    /// HTML of a single section resolved by `name`.
    pub fn section(&self, mode: html::Mode, name: &str, images: bool) -> Option<String> {
        nav::section(&html::to_html(self.access.as_ref(), mode, images, true), name)
    }

    /// Structured front-matter of an academic paper (page 1).
    pub fn front_matter(&self) -> frontmatter::FrontMatter {
        frontmatter::extract_front_matter(self.access.as_ref())
    }

    /// OCR plan: per page, whether OCR is needed and (if so) the page raster bytes.
    pub fn ocr_plan(&self) -> Vec<OcrPlanEntry> {
        let mut out = Vec::new();
        for page in self.access.pages_or_empty() {
            let decision = ocr::detect::decide(self.access.as_ref(), page.id);
            let needs = !matches!(decision, ocr::detect::OcrDecision::NotNeeded);
            let (w, h) = ocr::page_size_pts(self.access.as_ref(), page.id);
            let image = if needs {
                ocr::page_main_image(self.access.as_ref(), page.id).map(|(b, _)| b)
            } else {
                None
            };
            out.push(OcrPlanEntry { page: page.number, needs_ocr: needs, reason: format!("{decision:?}"), width_pts: w, height_pts: h, image });
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
        ocr::searchable::build(self.access.as_ref(), ocr, remove_raster)
            .map_err(Error::Model)
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
            self.access.as_ref(),
            &file,
            generated_at,
            opts.profile,
        )
        .map_err(|error| Error::Model(error.to_string()))?;
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(Error::Mkdir)?;
        }
        model::container::save(&model, &dest, &asset_bytes, None).map_err(Error::Model)?;
        Ok(dest.to_string_lossy().into_owned())
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
    use crate::access::AccessKind;
    use lopdf::{dictionary, Object};
    use std::io::{Seek, Write};
    use std::process::Command;
    use std::sync::atomic::Ordering;

    /// The owned encrypted fixtures (`tests/gen_fixtures.py::gen_encrypted`). They live in
    /// their own subfolder so the Python whole-fixture-set sweeps skip them.
    fn enc_fixture(name: &str) -> String {
        format!("{}/../tests/fixtures_pdf/encrypted/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    const ENC_SENTENCE: &str = "Encrypted fixture sentinel phrase for distillPDF.";

    fn route_fixture() -> (PathBuf, Vec<u8>) {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures_pdf/sec_structure.pdf"
        ));
        let raw = std::fs::read(&path).unwrap();
        (path, raw)
    }

    fn route_temp(label: &str, raw: &[u8]) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "distillpdf-l3a-route-{}-{label}-{}.pdf",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, raw).unwrap();
        path
    }

    struct GeneratedDir(PathBuf);

    impl GeneratedDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "distillpdf-l3a-terminal-{}-{label}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for GeneratedDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn generate_lazy_fixtures(output: &Path, arguments: &[&str]) {
        let generator = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/lazy_engine_fixtures.py"
        );
        let status = Command::new("python3")
            .arg(generator)
            .arg("generate")
            .arg("--out")
            .arg(output)
            .args(arguments)
            .status()
            .expect("run deterministic lazy fixture generator");
        assert!(status.success(), "lazy fixture generation failed");
    }

    fn indexed_outcome(
        opened: Result<IndexedOpenControl, RouteOpenError>,
    ) -> Result<String, AccessKind> {
        match opened {
            Ok(control) => control
                .checked_fingerprint()
                .map_err(|error| error.kind),
            Err(error) => match error.failure {
                RouteFailure::Access(error) => Err(error.kind),
                RouteFailure::Source(_) => Err(AccessKind::SourceIo),
            },
        }
    }

    fn eager_outcome(raw: &[u8]) -> Result<String, AccessKind> {
        let document = PdfDocument::from_bytes(raw).map_err(|_| AccessKind::Backend)?;
        checked_access_fingerprint(document.access.as_ref()).map_err(|error| error.kind)
    }

    fn inherited_resources_pdf(parent_depth: usize, malformed_parent: bool) -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        let content = document.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            b"BT /F1 12 Tf (resource-depth) Tj ET".to_vec(),
        ));
        let font = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        }));
        let resources = document.add_object(Object::Dictionary(dictionary! {
            "Font" => dictionary! { "F1" => Object::Reference(font) },
            "DepthMarker" => parent_depth as i64,
        }));
        let page = document.new_object_id();
        let parent = if malformed_parent {
            (999_999, 0)
        } else {
            let mut next = None;
            for depth in (1..=parent_depth).rev() {
                let mut dictionary = dictionary! { "Type" => "Pages" };
                if let Some(next) = next {
                    dictionary.set("Parent", Object::Reference(next));
                }
                if depth == parent_depth {
                    dictionary.set("Resources", Object::Reference(resources));
                }
                next = Some(document.add_object(Object::Dictionary(dictionary)));
            }
            next.expect("resource fixture has at least one parent")
        };
        document.objects.insert(
            page,
            Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => Object::Reference(parent),
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Contents" => Object::Reference(content),
            }),
        );
        let pages = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Pages", "Count" => 1,
            "Kids" => vec![Object::Reference(page)],
        }));
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages),
        }));
        document.trailer.set("Root", Object::Reference(catalog));
        let mut raw = Vec::new();
        document.save_to(&mut raw).unwrap();
        raw
    }

    #[test]
    fn l3a_terminal_all_small_fixtures_have_checked_file_bytes_parity() {
        let _test_lock = crate::access::indexed_test_lock();
        let generated = GeneratedDir::new("small");
        generate_lazy_fixtures(generated.path(), &["--profile", "small"]);
        let names = [
            "classic.pdf",
            "xref-stream.pdf",
            "incremental.pdf",
            "object-stream.pdf",
            "reference-one-hop.pdf",
            "reference-at-limit.pdf",
            "reference-over-limit.pdf",
            "reference-dangling.pdf",
            "reference-cycle.pdf",
            "generation-match.pdf",
            "generation-mismatch.pdf",
            "stream-missing-length.pdf",
            "stream-short-length.pdf",
        ];
        for name in names {
            let path = generated.path().join(name);
            let raw = std::fs::read(&path).unwrap();
            let file = open_indexed_file_internal(&path, None);
            let file_diagnostics = file
                .as_ref()
                .map(|control| {
                    assert!(control.shared_bytes().is_none(), "{name}");
                    control.diagnostics()
                })
                .unwrap_or_else(|error| error.diagnostics.snapshot());
            let bytes = open_indexed_bytes_internal(Arc::from(raw.clone()), None);
            let bytes_diagnostics = bytes
                .as_ref()
                .map(|control| control.diagnostics())
                .unwrap_or_else(|error| error.diagnostics.snapshot());
            if matches!(name, "stream-missing-length.pdf" | "stream-short-length.pdf") {
                let decoded: &[u8] = b"";
                let recovered = b"BT /F1 12 Tf 72 720 Td (malformed stream) Tj ET";
                for control in [file.as_ref().unwrap(), bytes.as_ref().unwrap()] {
                    let page = control.pages().unwrap()[0].id;
                    assert!(control.checked_page_content_matches(page, decoded).unwrap(), "{name}");
                    assert!(control.checked_recovered_stream_matches(4, recovered).unwrap(), "{name}");
                }
            }
            let file_outcome = indexed_outcome(file);
            let bytes_outcome = indexed_outcome(bytes);
            assert_eq!(file_outcome, bytes_outcome, "file/bytes {name}");
            match name {
                "reference-over-limit.pdf" => {
                    assert_eq!(file_outcome, Err(AccessKind::ResourceLimit), "{name}");
                }
                "reference-dangling.pdf" | "generation-mismatch.pdf" => {
                    assert_eq!(file_outcome, Err(AccessKind::Backend), "{name}");
                }
                "reference-cycle.pdf" => {
                    assert_eq!(file_outcome, Err(AccessKind::Backend), "{name}");
                }
                "stream-missing-length.pdf" | "stream-short-length.pdf" => {
                    assert!(file_outcome.is_ok(), "{name}");
                }
                _ => assert_eq!(file_outcome, eager_outcome(&raw), "eager parity {name}"),
            }
            for diagnostics in [file_diagnostics, bytes_diagnostics] {
                assert_eq!(diagnostics.indexed_opens, 1, "{name}");
                assert_eq!(diagnostics.fallback_opens, 0, "{name}");
            }
            assert!(file_diagnostics.source_max_request <= 64 * 1024, "{name}");
        }
    }

    #[test]
    fn l3a_terminal_inherited_resources_enforce_frozen_parent_boundary() {
        let _test_lock = crate::access::indexed_test_lock();
        let at_limit = inherited_resources_pdf(100, false);
        let eager = eager_outcome(&at_limit);
        assert!(eager.is_ok());
        let indexed = indexed_outcome(open_indexed_bytes_internal(Arc::from(at_limit), None));
        assert_eq!(indexed, eager);

        let over_limit = inherited_resources_pdf(101, false);
        let indexed = indexed_outcome(open_indexed_bytes_internal(Arc::from(over_limit), None));
        assert_eq!(indexed, Err(AccessKind::ResourceLimit));

        let malformed = inherited_resources_pdf(1, true);
        let indexed = indexed_outcome(open_indexed_bytes_internal(Arc::from(malformed), None));
        assert_eq!(indexed, Err(AccessKind::Backend));
    }

    #[test]
    fn l3a_terminal_scale_axes_obey_frozen_metadata_formula() {
        let _test_lock = crate::access::indexed_test_lock();
        let generated = GeneratedDir::new("scale");
        for axis in ["objects", "pages"] {
            for count in [1_000_u64, 5_000, 10_000] {
                generate_lazy_fixtures(
                    generated.path(),
                    &["--profile", "scale", "--axis", axis, "--count", &count.to_string()],
                );
                let path = generated.path().join(format!("{axis}-{count}.pdf"));
                let raw: Arc<[u8]> = std::fs::read(&path).unwrap().into();
                let file = open_indexed_file_internal(&path, None).unwrap();
                let bytes = open_indexed_bytes_internal(raw, None).unwrap();
                let expected_objects = if axis == "objects" { count + 5 } else { 2 * count + 3 };
                let expected_pages = if axis == "pages" { count } else { 1 };
                for diagnostics in [file.diagnostics(), bytes.diagnostics()] {
                    assert_eq!(diagnostics.index_objects, expected_objects, "{axis}-{count}");
                    assert_eq!(diagnostics.index_pages, expected_pages, "{axis}-{count}");
                    assert_eq!(diagnostics.page_map_builds, 1, "{axis}-{count}");
                    assert_eq!(diagnostics.indexed_opens, 1, "{axis}-{count}");
                    assert_eq!(diagnostics.fallback_opens, 0, "{axis}-{count}");
                    let cap = 33_554_432_u64
                        + 1_536_u64 * expected_objects
                        + 3_072_u64 * expected_pages;
                    assert!(
                        diagnostics.index_estimated_bytes <= cap,
                        "{axis}-{count}: retained={} cap={cap}",
                        diagnostics.index_estimated_bytes
                    );
                }
                assert!(file.shared_bytes().is_none(), "{axis}-{count}");
                assert!(
                    file.diagnostics().source_max_request <= 64 * 1024,
                    "{axis}-{count}: max request {}",
                    file.diagnostics().source_max_request
                );
            }
        }
    }

    #[test]
    fn indexed_file_bytes_and_snapshot_are_single_open_explicit_routes() {
        let (fixture, raw) = route_fixture();
        let shared: Arc<[u8]> = Arc::from(raw.clone());
        let bytes = open_indexed_bytes_internal(Arc::clone(&shared), None).unwrap();
        assert!(Arc::ptr_eq(bytes.shared_bytes().unwrap(), &shared));
        let expected_sha = bytes.source_sha256().unwrap();
        let bytes_diag = bytes.diagnostics();
        assert_eq!(bytes_diag.route, OpenRoute::IndexedBytes);
        assert_eq!(bytes_diag.source_mode, SourceMode::SharedBytes);
        assert_eq!(bytes_diag.eager_opens, 0);
        assert_eq!(bytes_diag.indexed_opens, 1);
        assert_eq!(bytes_diag.fallback_opens, 0);
        assert_eq!(bytes_diag.page_map_builds, 1);
        assert!(bytes_diag.index_objects > 0);
        assert!(bytes_diag.index_pages > 0);
        assert!(bytes_diag.index_estimated_bytes > 0);
        assert_eq!(bytes_diag.document_object_o_admitted_bytes, 0);

        let file = open_indexed_file_internal(&fixture, None).unwrap();
        assert!(file.shared_bytes().is_none());
        assert_eq!(file.pages().unwrap().len(), bytes.pages().unwrap().len());
        assert_eq!(file.source_sha256().unwrap(), expected_sha);
        let file_diag = file.diagnostics();
        assert_eq!(file_diag.route, OpenRoute::IndexedFile);
        assert_eq!(file_diag.source_mode, SourceMode::FileDescriptor);
        assert_eq!(file_diag.indexed_opens, 1);
        assert_eq!(file_diag.fallback_opens, 0);
        assert!(file_diag.source_requests >= file_diag.source_reads);
        assert!(file_diag.source_reads > 0);
        assert!(file_diag.source_max_request <= 64 * 1024);

        let snapshot = open_indexed_snapshot_internal(&fixture, None).unwrap();
        assert!(snapshot.shared_bytes().is_some());
        assert_eq!(snapshot.source_sha256().unwrap(), expected_sha);
        let snapshot_diag = snapshot.diagnostics();
        assert_eq!(snapshot_diag.route, OpenRoute::IndexedSnapshot);
        assert_eq!(snapshot_diag.reason, OpenReason::ExplicitSnapshot);
        assert_eq!(snapshot_diag.source_mode, SourceMode::FullSnapshot);
        assert_eq!(snapshot_diag.indexed_opens, 1);
        assert_eq!(snapshot_diag.fallback_opens, 0);

        let diagnostics = Arc::clone(&file.diagnostics);
        drop(file);
        assert_eq!(
            diagnostics.snapshot().document_object_o_admitted_bytes,
            0
        );
    }

    #[test]
    fn public_constructors_remain_eager_compatibility_routes() {
        let (fixture, raw) = route_fixture();
        let file = PdfDocument::open(fixture.to_str().unwrap()).unwrap();
        let bytes = PdfDocument::from_bytes(&raw).unwrap();
        for (actual, route, mode) in [
            (
                file.route_diagnostics(),
                OpenRoute::EagerFile,
                SourceMode::EagerMaterializedFile,
            ),
            (
                bytes.route_diagnostics(),
                OpenRoute::EagerBytes,
                SourceMode::SharedBytes,
            ),
        ] {
            assert_eq!(actual.route, route);
            assert_eq!(actual.reason, OpenReason::PublicCompatibility);
            assert_eq!(actual.source_mode, mode);
            assert_eq!(actual.eager_opens, 1);
            assert_eq!(actual.indexed_opens, 0);
            assert_eq!(actual.fallback_opens, 0);
        }
    }

    #[test]
    fn indexed_file_descriptor_survives_path_replacement_and_fails_on_mutation() {
        let (_, raw) = route_fixture();
        let expected = open_indexed_bytes_internal(Arc::from(raw.clone()), None)
            .unwrap()
            .source_sha256()
            .unwrap();

        let path = route_temp("replace", &raw);
        let displaced = path.with_extension("held.pdf");
        let control = open_indexed_file_internal(&path, None).unwrap();
        std::fs::rename(&path, &displaced).unwrap();
        std::fs::write(&path, b"replacement path bytes").unwrap();
        assert_eq!(control.source_sha256().unwrap(), expected);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&displaced).unwrap();

        let path = route_temp("rewrite", &raw);
        let control = open_indexed_file_internal(&path, None).unwrap();
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        file.write_all(b"%QDF-").unwrap();
        file.sync_all().unwrap();
        let error = control.source_sha256().unwrap_err();
        assert_eq!(error.kind, AccessKind::SourceChanged);
        std::fs::remove_file(&path).unwrap();

        let path = route_temp("truncate", &raw);
        let control = open_indexed_file_internal(&path, None).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len((raw.len() / 2) as u64)
            .unwrap();
        let error = control.source_sha256().unwrap_err();
        assert_eq!(error.kind, AccessKind::SourceChanged);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn indexed_route_keeps_post_open_source_io_typed_and_never_falls_back() {
        struct SwitchedSource {
            bytes: Arc<[u8]>,
            mode: Arc<AtomicU64>,
        }
        impl RandomAccessSource for SwitchedSource {
            fn len(&self) -> SourceResult<u64> {
                Ok(self.bytes.len() as u64)
            }
            fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
                match self.mode.load(Ordering::Acquire) {
                    1 => return Ok(0),
                    2 => return Ok(out.len() + 1),
                    3 => return Err(SourceError::Io(std::io::Error::other("injected I/O"))),
                    _ => {}
                }
                let start = offset as usize;
                let take = out.len().min(self.bytes.len().saturating_sub(start));
                out[..take].copy_from_slice(&self.bytes[start..start + take]);
                Ok(take)
            }
        }

        let (_, raw) = route_fixture();
        for failure_mode in 1..=3 {
            let mode = Arc::new(AtomicU64::new(0));
            let diagnostics = RouteDiagnostics::new(
                OpenRoute::IndexedBytes,
                OpenReason::InternalMeasurement,
                SourceMode::SharedBytes,
            );
            let source: Arc<dyn RandomAccessSource> = Arc::new(SwitchedSource {
                bytes: Arc::from(raw.clone()),
                mode: Arc::clone(&mode),
            });
            let control = open_indexed_source(source, None, diagnostics, None).unwrap();
            mode.store(failure_mode, Ordering::Release);
            let error = control.source_sha256().unwrap_err();
            assert_eq!(error.kind, AccessKind::SourceIo);
            assert_eq!(control.diagnostics().indexed_opens, 1);
            assert_eq!(control.diagnostics().fallback_opens, 0);
        }
    }

    #[test]
    fn indexed_route_preserves_encryption_open_categories() {
        for name in [
            "rc4_40.pdf",
            "rc4_128.pdf",
            "aes_128.pdf",
            "aes_256.pdf",
            "inline_encrypt_aes_128.pdf",
            "inline_encrypt_rc4_128.pdf",
        ] {
            let raw: Arc<[u8]> = std::fs::read(enc_fixture(name)).unwrap().into();
            let control = open_indexed_bytes_internal(raw, None)
                .unwrap_or_else(|error| panic!("{name} owner-password route failed: {error:?}"));
            assert!(!control.pages().unwrap().is_empty(), "{name}");
            assert_eq!(control.diagnostics().indexed_opens, 1);
            assert_eq!(control.diagnostics().fallback_opens, 0);
        }

        for name in ["userpw.pdf", "inline_encrypt_userpw.pdf"] {
            let raw: Arc<[u8]> = std::fs::read(enc_fixture(name)).unwrap().into();
            let error = open_indexed_bytes_internal(Arc::clone(&raw), None)
                .err()
                .expect("password required");
            assert!(matches!(
                error.failure,
                RouteFailure::Access(AccessError {
                    kind: AccessKind::PasswordRequired,
                    ..
                })
            ));
            assert_eq!(error.diagnostics.snapshot().indexed_opens, 1);
            assert_eq!(error.diagnostics.snapshot().fallback_opens, 0);

            let error = open_indexed_bytes_internal(raw, Some(b"wrong".to_vec()))
                .err()
                .expect("wrong password");
            assert!(matches!(
                error.failure,
                RouteFailure::Access(AccessError {
                    kind: AccessKind::InvalidPassword,
                    ..
                })
            ));
        }

        let mut document = Document::with_version("1.7");
        let pages = document.add_object(dictionary! {
            "Type" => "Pages", "Count" => 0, "Kids" => Vec::<lopdf::Object>::new(),
        });
        let catalog = document.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages,
        });
        document.trailer.set("Root", catalog);
        document.trailer.set("Encrypt", 7);
        let mut raw = Vec::new();
        document.save_to(&mut raw).unwrap();
        let error = open_indexed_bytes_internal(Arc::from(raw), None)
            .err()
            .expect("invalid Encrypt value");
        assert!(matches!(
            error.failure,
            RouteFailure::Access(AccessError {
                kind: AccessKind::InvalidEncryptDictionary,
                ..
            })
        ));

        let mut corrupted = std::fs::read(enc_fixture("inline_encrypt_userpw.pdf")).unwrap();
        let endstream = corrupted
            .windows(b"endstream".len())
            .position(|window| window == b"endstream")
            .expect("generated AES fixture has a page-content stream");
        let mut last_payload = endstream;
        while corrupted
            .get(last_payload.saturating_sub(1))
            .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
        {
            last_payload -= 1;
        }
        corrupted[last_payload - 1] ^= 0xff;
        let control = open_indexed_bytes_internal(
            Arc::from(corrupted),
            Some(b"secret".to_vec()),
        )
        .expect("encryption bootstrap remains valid");
        let page = control.pages().unwrap()[0].id;
        let error = control
            .check_page_content(page)
            .expect_err("corrupted AES padding must fail object decryption");
        assert_eq!(error.kind, AccessKind::ObjectDecryption);
    }

    #[test]
    fn indexed_constructor_region_has_no_eager_retry_or_implicit_snapshot() {
        let source = include_str!("doc.rs");
        let terminal_start = source.find("fn check_page_content(").unwrap();
        let terminal_end = source[terminal_start..]
            .find("/// A private one-thread rayon pool")
            .map(|offset| terminal_start + offset)
            .unwrap();
        let checked_terminal = &source[terminal_start..terminal_end];
        for forbidden in [
            "std::fs::read",
            "read_to_end",
            "load_mem_deterministic",
            "EagerDocumentAdapter",
            "materialize_source_bounded",
            "Arc<[u8]>",
            "source_owner",
            "fallback_open",
            "Default::default",
            "unwrap_or_default",
            "_or_empty",
        ] {
            assert!(!checked_terminal.contains(forbidden), "terminal forbidden {forbidden}");
        }
        let indexed_start = source.find("fn open_indexed_source(").unwrap();
        let snapshot_start = source
            .find("pub(crate) fn open_indexed_snapshot_internal(")
            .unwrap();
        let implementation_end = source[snapshot_start..]
            .find("\nimpl PdfDocument")
            .map(|offset| snapshot_start + offset)
            .unwrap();
        let bounded_routes = &source[indexed_start..snapshot_start];
        let all_indexed_routes = &source[indexed_start..implementation_end];
        for forbidden in [
            "load_mem_deterministic",
            "Document::load",
            "EagerDocumentAdapter",
            "std::fs::read",
            "_or_empty",
        ] {
            assert!(!bounded_routes.contains(forbidden), "forbidden {forbidden}");
        }
        assert_eq!(all_indexed_routes.matches("std::fs::read").count(), 1);
        assert!(!all_indexed_routes.contains("load_mem_deterministic"));
        assert!(!all_indexed_routes.contains("EagerDocumentAdapter"));
        assert!(!all_indexed_routes.contains("fallback_open"));
        assert!(!all_indexed_routes.contains("_or_empty"));
    }

    #[test]
    fn indexed_route_open_failures_keep_route_and_zero_fallback_provenance() {
        let missing = route_temp("missing", b"");
        std::fs::remove_file(&missing).unwrap();
        let error = open_indexed_file_internal(&missing, None)
            .err()
            .expect("missing file");
        assert!(matches!(error.failure, RouteFailure::Source(SourceError::Io(_))));
        let diagnostics = error.diagnostics.snapshot();
        assert_eq!(diagnostics.route, OpenRoute::IndexedFile);
        assert_eq!(diagnostics.indexed_opens, 0);
        assert_eq!(diagnostics.fallback_opens, 0);

        let error = open_indexed_bytes_internal(Arc::from(&b"not a PDF"[..]), None)
            .err()
            .expect("invalid bytes");
        assert!(matches!(error.failure, RouteFailure::Access(_)));
        let diagnostics = error.diagnostics.snapshot();
        assert_eq!(diagnostics.route, OpenRoute::IndexedBytes);
        assert_eq!(diagnostics.indexed_opens, 1);
        assert_eq!(diagnostics.fallback_opens, 0);
    }

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
            let pages = pdf.access.pages().expect("public eager route pages");
            for page in pages {
                let mine = text::extract_page(pdf.access.as_ref(), page.id).unwrap_or_default();
                if mine.trim().chars().count() >= 2 {
                    want.push_str(&mine);
                } else {
                    want.push_str(
                        &pdf.access
                            .fallback_page_text(page.number)
                            .unwrap_or_default(),
                    );
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
