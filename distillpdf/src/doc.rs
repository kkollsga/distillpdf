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
use crate::table::AnalyzedTable;
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
    /// Route provenance for this handle. Both engines populate it on the write side; the read
    /// side is public as [`PdfDocument::engine`] and crate-internal as `route_diagnostics`.
    diagnostics: Arc<RouteDiagnostics>,
    /// Source path (`open`); `None` when constructed from bytes.
    pub(crate) source: Option<PathBuf>,
    /// Cached OCR results: `{1-based page: DocTags}`, populated once by `set_ocr`.
    pub(crate) ocr_cache: Mutex<HashMap<u32, String>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenRoute {
    EagerFile,
    EagerBytes,
    IndexedFile,
    IndexedBytes,
    IndexedSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenReason {
    PublicCompatibility,
    InternalMeasurement,
    ExplicitSnapshot,
}

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

pub(crate) struct RouteDiagnostics {
    pub(crate) route: OpenRoute,
    #[allow(dead_code)] // reported through `snapshot`, which only tests/measurement read
    pub(crate) reason: OpenReason,
    #[allow(dead_code)] // reported through `snapshot`, which only tests/measurement read
    pub(crate) source_mode: SourceMode,
    eager_opens: AtomicU64,
    indexed_opens: AtomicU64,
    fallback_opens: AtomicU64,
    source: Arc<RouteSourceCounters>,
    indexed: OnceLock<Arc<IndexedAdapterCounters>>,
}

/// Read-side view of [`RouteDiagnostics`]. Populated by live code; consumed only by tests and
/// the internal measurement harness, so it reads as dead in a plain build.
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
}

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
        }
    }
}

#[allow(dead_code)] // payloads exist for the `Debug` report the strict selector surfaces
#[derive(Debug)]
pub(crate) enum RouteFailure {
    Source(SourceError),
    Access(AccessError),
}

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

pub(crate) struct IndexedOpenControl {
    access: Arc<IndexedDocumentAdapter>,
    diagnostics: Arc<RouteDiagnostics>,
    source_owner: Option<Arc<[u8]>>,
}

#[allow(dead_code)] // pre-adoption inspection helpers; used by tests and measurement
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
/// Measured: 40 loads of one USGS file produce **3 distinct object
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

/// Which access engine a [`PdfDocument`] constructor builds.
///
/// The default is [`Engine::Eager`] — the long-standing `lopdf::Document` route — and it stays
/// the default until an owner decision flips it. Pick another one explicitly with
/// [`PdfDocument::open_with_engine`] / [`PdfDocument::from_bytes_with_engine`].
///
/// * [`Engine::Eager`] — parse the whole container up front. Lowest latency on small files.
/// * [`Engine::Lazy`] — the bounded indexed route: build an index and pull objects on demand,
///   so a large file never has to be resident in full. Output is identical to eager. When the
///   indexed open itself refuses a document (xref recovery, an encryption shape the index
///   cannot take, a bounded-decode envelope refusal) the constructor falls back to the eager
///   engine and **counts** it — see [`PdfDocument::engine`], which reports
///   [`EngineRoute::LazyEagerFallback`] for exactly that case. The fallback is never silent.
/// * [`Engine::LazyStrict`] — the same route with the fallback *disabled*, so an indexed open
///   failure surfaces as [`Error::Open`]. This is the measurement/diagnostic variant: it tells
///   a true-lazy run from one that quietly ended up on the eager engine. Not exposed by the
///   Python binding.
///
/// When a caller does not name an engine, the internal `DISTILLPDF_ENGINE` environment variable
/// decides — see [`engine_selection`]. An explicit [`Engine`] always wins over it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Engine {
    #[default]
    Eager,
    Lazy,
    LazyStrict,
}

impl Engine {
    /// Parse the **public** engine grammar: `"eager"` or `"lazy"`. This is the grammar the
    /// Python binding's `engine=` keyword speaks, so the error message here is the one a
    /// Python caller sees; `LazyStrict` is deliberately unreachable through it.
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "eager" => Ok(Engine::Eager),
            "lazy" => Ok(Engine::Lazy),
            other => Err(Error::InvalidEngine(other.to_string())),
        }
    }

    fn prefers_indexed(self) -> bool {
        !matches!(self, Engine::Eager)
    }
}

/// Which engine actually served an open — the honest answer, after any fallback.
///
/// Derived from the route counters the constructors write, so it cannot drift from what really
/// happened. Returned by [`PdfDocument::engine`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineRoute {
    /// The eager engine, because that is what was asked for (or defaulted to).
    Eager,
    /// The bounded indexed engine.
    Lazy,
    /// [`Engine::Lazy`] was asked for, the indexed open refused the document, and the counted
    /// eager fallback served it instead.
    LazyEagerFallback,
}

impl EngineRoute {
    /// The stable string form — also exactly what the Python `Pdf.engine` property returns.
    pub fn as_str(self) -> &'static str {
        match self {
            EngineRoute::Eager => "eager",
            EngineRoute::Lazy => "lazy",
            EngineRoute::LazyEagerFallback => "lazy (eager fallback)",
        }
    }
}

impl std::fmt::Display for EngineRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `DISTILLPDF_ENGINE` grammar, as a pure function so it is testable without the process
/// environment. Unset and every unrecognised value mean eager — the selector can only ever be
/// turned on deliberately. Deliberately NOT the public [`Engine::parse`] grammar: these spellings
/// are internal and may change or disappear.
fn parse_engine_selection(value: Option<&str>) -> Engine {
    match value {
        Some("indexed") => Engine::Lazy,
        Some("indexed-strict") => Engine::LazyStrict,
        _ => Engine::Eager,
    }
}

/// The engine to use when the caller named none: read `DISTILLPDF_ENGINE` once per process.
///
/// **Internal and unstable** — it exists so the whole test suite and the corpus measurement
/// harness can be run over the indexed route without changing what a released build does, and
/// it carries no compatibility promise. An explicit [`Engine`] passed to
/// [`PdfDocument::open_with_engine`] / [`PdfDocument::from_bytes_with_engine`] always wins.
fn engine_selection() -> Engine {
    static SELECTION: OnceLock<Engine> = OnceLock::new();
    *SELECTION.get_or_init(|| {
        parse_engine_selection(std::env::var("DISTILLPDF_ENGINE").ok().as_deref())
    })
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

    /// Adopt a successful indexed open as the document's access route.
    ///
    /// The control's `source_owner` is only the *second* handle on the shared bytes — the
    /// `BytesSource` inside the adapter holds its own — so the handle is dropped here rather
    /// than adding a byte-retaining field to [`PdfDocument`].
    fn finish_indexed_open(control: IndexedOpenControl, source: Option<PathBuf>) -> Self {
        let IndexedOpenControl { access, diagnostics, source_owner } = control;
        drop(source_owner);
        Self::finish_open(access, diagnostics, source)
    }

    /// Decide what an indexed open failure means for the selected engine: `Err` under
    /// [`Engine::LazyStrict`], `Ok(())` ("fall back, and count it") under [`Engine::Lazy`].
    fn indexed_failure_disposition(error: RouteOpenError, engine: Engine) -> Result<(), Error> {
        if engine == Engine::LazyStrict {
            return Err(Error::Open(format!(
                "engine=lazy-strict: indexed open failed: {:?}",
                error.failure
            )));
        }
        Ok(())
    }

    #[allow(dead_code)] // access-injection seam for tests; production uses `from_bytes`
    fn from_bytes_with_access_factory(
        data: &[u8],
        make_access: impl FnOnce(Arc<Document>, Arc<[u8]>) -> Arc<dyn DocumentAccess>,
    ) -> Result<Self, Error> {
        Self::from_bytes_eager(data, make_access, false)
    }

    fn from_bytes_eager(
        data: &[u8],
        make_access: impl FnOnce(Arc<Document>, Arc<[u8]>) -> Arc<dyn DocumentAccess>,
        after_indexed_failure: bool,
    ) -> Result<Self, Error> {
        let diagnostics = RouteDiagnostics::new(
            OpenRoute::EagerBytes,
            OpenReason::PublicCompatibility,
            SourceMode::SharedBytes,
        );
        diagnostics.eager_opens.store(1, Ordering::Relaxed);
        if after_indexed_failure {
            diagnostics.fallback_opens.store(1, Ordering::Relaxed);
        }
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
    ///
    /// Uses [`Engine::Eager`] unless the internal `DISTILLPDF_ENGINE` selector says otherwise
    /// (see [`engine_selection`]). To choose deliberately, call [`PdfDocument::open_with_engine`].
    pub fn open(path: &str) -> Result<Self, Error> {
        Self::open_with_selection(path, engine_selection())
    }

    /// Open a PDF from a filesystem path on a named [`Engine`].
    ///
    /// The explicit choice wins outright: the internal `DISTILLPDF_ENGINE` selector is not
    /// consulted. [`PdfDocument::engine`] on the returned handle reports which engine actually
    /// served the open, including a counted [`EngineRoute::LazyEagerFallback`].
    pub fn open_with_engine(path: &str, engine: Engine) -> Result<Self, Error> {
        Self::open_with_selection(path, engine)
    }

    fn open_with_selection(path: &str, engine: Engine) -> Result<Self, Error> {
        let mut after_indexed_failure = false;
        if engine.prefers_indexed() {
            match open_indexed_file_internal(Path::new(path), None) {
                Ok(control) => {
                    return Ok(Self::finish_indexed_open(control, Some(PathBuf::from(path))));
                }
                Err(error) => {
                    Self::indexed_failure_disposition(error, engine)?;
                    after_indexed_failure = true;
                }
            }
        }
        let diagnostics = RouteDiagnostics::new(
            OpenRoute::EagerFile,
            OpenReason::PublicCompatibility,
            SourceMode::EagerMaterializedFile,
        );
        diagnostics.eager_opens.store(1, Ordering::Relaxed);
        if after_indexed_failure {
            diagnostics.fallback_opens.store(1, Ordering::Relaxed);
        }
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
    ///
    /// Selects the engine exactly like [`PdfDocument::open`].
    pub fn from_bytes(data: &[u8]) -> Result<Self, Error> {
        Self::from_bytes_with_selection(data, engine_selection())
    }

    /// Open a PDF from raw bytes on a named [`Engine`] — the `from_bytes` counterpart to
    /// [`PdfDocument::open_with_engine`], with the same precedence rule.
    pub fn from_bytes_with_engine(data: &[u8], engine: Engine) -> Result<Self, Error> {
        Self::from_bytes_with_selection(data, engine)
    }

    fn from_bytes_with_selection(data: &[u8], engine: Engine) -> Result<Self, Error> {
        let mut after_indexed_failure = false;
        if engine.prefers_indexed() {
            match open_indexed_bytes_internal(Arc::from(data), None) {
                Ok(control) => return Ok(Self::finish_indexed_open(control, None)),
                Err(error) => {
                    Self::indexed_failure_disposition(error, engine)?;
                    after_indexed_failure = true;
                }
            }
        }
        Self::from_bytes_eager(
            data,
            |document, source| Arc::new(EagerDocumentAdapter::new(document, source)),
            after_indexed_failure,
        )
    }

    /// Which engine actually served the open that produced this handle.
    ///
    /// Read straight off the route counters both engines write, so it reports a counted eager
    /// fallback as [`EngineRoute::LazyEagerFallback`] rather than claiming the lazy engine ran.
    pub fn engine(&self) -> EngineRoute {
        let snapshot = self.diagnostics.snapshot();
        if snapshot.fallback_opens > 0 {
            EngineRoute::LazyEagerFallback
        } else if snapshot.indexed_opens > 0 {
            EngineRoute::Lazy
        } else {
            EngineRoute::Eager
        }
    }

    /// Route provenance for the open that produced this handle — crate-internal, no public
    /// surface. The `fallback_opens` counter is how a counted eager fallback from the indexed
    /// selector is observed; [`PdfDocument::engine`] is the public read of the same counters.
    #[allow(dead_code)] // diagnostics read side: tests and the internal measurement harness
    pub(crate) fn route_diagnostics(&self) -> RouteDiagnosticsSnapshot {
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

    /// Analyze raw table detections with semantic anchors and normalized display geometry.
    ///
    /// This deliberately shares [`Self::extract_tables`]' raw detector source. Rendered
    /// HTML/Markdown may differ because rendering subsequently filters figure-like grids,
    /// reconciles tagged declarations and attaches captions.
    pub fn analyze_tables(&self) -> Vec<AnalyzedTable> {
        extract::analyze_tables(self.access.as_ref())
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
    use lopdf::{
        dictionary, IndexedObjectLocation, IndexedReader, IndexedReaderError, IndexedReaderOptions,
        Object, ScalarResolutionPermit,
    };
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

    fn objstm_reader(path: &Path) -> IndexedReader {
        let raw = std::fs::read(path).unwrap();
        IndexedReader::open(BytesSource::from(raw)).unwrap()
    }

    fn prepare_objstm(
        reader: &IndexedReader,
        container: lopdf::ObjectId,
    ) -> Result<(u64, u64), IndexedReaderError> {
        let permit = ScalarResolutionPermit::new(64 * 1024 * 1024);
        match reader.prepare_object_stream_with_permit(container, &permit) {
            Ok(owner) => {
                let retained = owner.retained_bytes();
                let stats = permit.stats();
                assert_eq!(owner.container_id(), container);
                assert_eq!(stats.current_bytes, retained);
                assert!(stats.peak_bytes <= 64 * 1024 * 1024);
                drop(owner);
                assert_eq!(permit.close().unwrap().current_bytes, 0);
                Ok((retained, stats.peak_bytes))
            }
            Err(error) => {
                assert_eq!(permit.close().unwrap().current_bytes, 0);
                Err(error)
            }
        }
    }

    fn assert_objstm_reader_cache_neutral(reader: &IndexedReader, case: &str) {
        assert_eq!(reader.cache_stats(), Default::default(), "{case}");
        assert_eq!(reader.object_cache_stats(), Default::default(), "{case}");
        assert_eq!(
            reader.object_stream_cache_stats(),
            Default::default(),
            "{case}"
        );
    }

    struct ObjStmTracingSource {
        bytes: Arc<[u8]>,
        requests: Mutex<Vec<(u64, usize)>>,
    }

    impl RandomAccessSource for ObjStmTracingSource {
        fn len(&self) -> SourceResult<u64> {
            u64::try_from(self.bytes.len()).map_err(|_| SourceError::RangeOverflow {
                offset: 0,
                length: u64::MAX,
            })
        }

        fn read_at(&self, offset: u64, output: &mut [u8]) -> SourceResult<usize> {
            let offset =
                usize::try_from(offset).map_err(|_| SourceError::PlatformLimitExceeded {
                    requested: offset,
                    limit: usize::MAX as u64,
                })?;
            let length = output.len().min(self.bytes.len().saturating_sub(offset));
            output[..length].copy_from_slice(&self.bytes[offset..offset + length]);
            self.requests
                .lock()
                .unwrap()
                .push((u64::try_from(offset).unwrap(), length));
            Ok(length)
        }
    }

    #[test]
    fn l3b_gate1_objstm_fixture_manifest_and_direct_fork_oracles() {
        let generated = GeneratedDir::new("objstm-gate1");
        generate_lazy_fixtures(generated.path(), &["--profile", "objstm-container"]);
        let actual_manifest =
            std::fs::read_to_string(generated.path().join("manifest.json")).unwrap();
        let frozen_manifest = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/oracles/l3b-objstm-container.json"
        ))
        .unwrap();
        assert_eq!(actual_manifest, frozen_manifest);

        let plain = objstm_reader(&generated.path().join("objstm-plain.pdf"));
        let flate = objstm_reader(&generated.path().join("objstm-flate.pdf"));
        for reader in [&plain, &flate] {
            assert_eq!(
                reader.object_location((7, 0)).unwrap(),
                IndexedObjectLocation::Compressed {
                    container: (6, 0),
                    index: 0,
                }
            );
            assert_objstm_reader_cache_neutral(reader, "plain/Flate before preparation");
        }
        let plain_facts = prepare_objstm(&plain, (6, 0)).unwrap();
        let flate_facts = prepare_objstm(&flate, (6, 0)).unwrap();
        assert!(plain_facts.0 > 0);
        assert!(flate_facts.0 > 0);
        assert_objstm_reader_cache_neutral(&plain, "plain after preparation");
        assert_objstm_reader_cache_neutral(&flate, "Flate after preparation");

        let two = objstm_reader(&generated.path().join("objstm-two-containers.pdf"));
        assert!(prepare_objstm(&two, (6, 0)).is_ok());
        assert!(prepare_objstm(&two, (9, 0)).is_ok());
        assert_objstm_reader_cache_neutral(&two, "two independent containers");

        let encrypted = objstm_reader(&generated.path().join("objstm-r4-rc4.pdf"));
        let encrypted_compressed: Vec<_> = encrypted
            .object_ids()
            .into_iter()
            .filter_map(|id| match encrypted.object_location(id).unwrap() {
                IndexedObjectLocation::Compressed { container, index } => {
                    Some((id, container, index))
                }
                IndexedObjectLocation::Normal => None,
                _ => None,
            })
            .collect();
        assert_eq!(
            encrypted_compressed,
            vec![
                ((3, 0), (2, 0), 0),
                ((4, 0), (2, 0), 1),
                ((5, 0), (2, 0), 2),
                ((6, 0), (2, 0), 3),
            ]
        );
        assert!(prepare_objstm(&encrypted, (2, 0)).is_ok());
        assert_objstm_reader_cache_neutral(&encrypted, "encrypted preparation");

        let cases = [
            "objstm-container-nonstream.pdf",
            "objstm-first-missing.pdf",
            "objstm-first-negative.pdf",
            "objstm-first-past-end.pdf",
            "objstm-n-missing.pdf",
            "objstm-n-negative.pdf",
            "objstm-n-over-131072.pdf",
            "objstm-filter-chain.pdf",
            "objstm-predictor.pdf",
            "objstm-flate-corrupt.pdf",
            "objstm-length-missing.pdf",
            "objstm-length-negative.pdf",
            "objstm-length-indirect-missing.pdf",
            "objstm-endstream-missing.pdf",
            "objstm-endstream-truncated.pdf",
        ];
        for name in cases {
            let reader = objstm_reader(&generated.path().join(name));
            let error = prepare_objstm(&reader, (6, 0)).unwrap_err();
            match name {
                "objstm-container-nonstream.pdf" => assert!(matches!(
                    error,
                    IndexedReaderError::ObjectStreamContainerNotStream {
                        id: (6, 0),
                        container: (6, 0),
                    }
                )),
                "objstm-first-missing.pdf" | "objstm-first-negative.pdf" => match error {
                    IndexedReaderError::ObjectStreamMember {
                        id: (6, 0),
                        container: (6, 0),
                        index: 0,
                        source: lopdf::Error::InvalidObjectStream(detail),
                    } => assert_eq!(detail, "invalid object stream /First"),
                    other => panic!("{name}: {other:?}"),
                },
                "objstm-first-past-end.pdf" => assert!(matches!(
                    error,
                    IndexedReaderError::ObjectStreamMember {
                        id: (6, 0),
                        container: (6, 0),
                        index: 0,
                        source: lopdf::Error::InvalidOffset(52),
                    }
                )),
                "objstm-n-missing.pdf" | "objstm-n-negative.pdf" | "objstm-n-over-131072.pdf" => {
                    match error {
                        IndexedReaderError::ObjectStreamMember {
                            id: (6, 0),
                            container: (6, 0),
                            index: 0,
                            source: lopdf::Error::InvalidObjectStream(detail),
                        } => assert_eq!(detail, "invalid object stream /N"),
                        other => panic!("{name}: {other:?}"),
                    }
                }
                // A chain whose *terminal* filter is not Flate/LZW, and a predictor whose
                // operands are past the bounded decode's caps: both outside the envelope,
                // both refused by name rather than decoded to the wrong bytes.
                "objstm-filter-chain.pdf" | "objstm-predictor.pdf" => assert!(matches!(
                    error,
                    IndexedReaderError::UnsupportedBoundedScalar {
                        id: (6, 0),
                        reason:
                            "object-stream filter chains or decode parameters outside the bounded decode envelope",
                    }
                )),
                "objstm-flate-corrupt.pdf" => match error {
                    IndexedReaderError::ObjectStreamMember {
                        id: (6, 0),
                        container: (6, 0),
                        index: 0,
                        source: lopdf::Error::InvalidObjectStream(detail),
                    } => assert_eq!(detail, "selected object stream member is not present"),
                    other => panic!("{name}: {other:?}"),
                },
                "objstm-length-missing.pdf"
                | "objstm-length-negative.pdf"
                | "objstm-length-indirect-missing.pdf" => assert!(matches!(
                    error,
                    IndexedReaderError::UnsupportedBoundedScalar {
                        id: (6, 0),
                        reason: "object streams without a bounded nonnegative /Length",
                    }
                )),
                "objstm-endstream-missing.pdf" | "objstm-endstream-truncated.pdf" => {
                    assert!(matches!(
                        error,
                        IndexedReaderError::MissingEndstream { id: (6, 0) }
                    ))
                }
                _ => unreachable!(),
            }
            assert_objstm_reader_cache_neutral(&reader, name);
        }

        let generation = objstm_reader(&generated.path().join("objstm-container-generation.pdf"));
        assert!(matches!(
            prepare_objstm(&generation, (6, 1)),
            Err(IndexedReaderError::GenerationMismatch {
                id: (6, 1),
                indexed: 0,
            })
        ));
        assert_objstm_reader_cache_neutral(&generation, "container generation mismatch");

        let authority_raw: Arc<[u8]> =
            std::fs::read(generated.path().join("objstm-xref-authority.pdf"))
                .unwrap()
                .into();
        let duplicate_start = authority_raw
            .windows(b"6 0 obj\n".len())
            .position(|window| window == b"6 0 obj\n")
            .unwrap();
        let selected_start = authority_raw
            .windows(b"10 0 obj\n".len())
            .position(|window| window == b"10 0 obj\n")
            .unwrap();
        let selected_end = authority_raw
            .windows(b"11 0 obj\n".len())
            .position(|window| window == b"11 0 obj\n")
            .unwrap();
        assert!(duplicate_start < selected_start && selected_start < selected_end);
        let duplicate_interval = duplicate_start as u64..selected_start as u64;
        let selected_interval = selected_start as u64..selected_end as u64;
        let source = Arc::new(ObjStmTracingSource {
            bytes: Arc::clone(&authority_raw),
            requests: Mutex::new(Vec::new()),
        });
        let erased: Arc<dyn RandomAccessSource> = source.clone();
        let authority =
            IndexedReader::open_shared(erased, IndexedReaderOptions::default()).unwrap();
        assert_eq!(
            authority.object_location((7, 0)).unwrap(),
            IndexedObjectLocation::Compressed {
                container: (10, 0),
                index: 0,
            }
        );
        source.requests.lock().unwrap().clear();
        let permit = ScalarResolutionPermit::new(64 * 1024 * 1024);
        let prepared = authority
            .prepare_object_stream_with_permit((10, 0), &permit)
            .unwrap();
        assert_eq!(prepared.container_id(), (10, 0));
        assert_eq!(permit.stats().current_bytes, prepared.retained_bytes());
        let reads_after_prepare = source.requests.lock().unwrap().len();
        let selected = prepared.resolve_member((7, 0), 0).unwrap();
        assert_eq!(selected.as_object().as_str().unwrap(), b"compressed-value");
        assert_eq!(source.requests.lock().unwrap().len(), reads_after_prepare);
        let requests = source.requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(
            requests.iter().all(|(offset, returned)| {
                let end = offset
                    .checked_add(u64::try_from(*returned).unwrap())
                    .expect("traced returned interval must not overflow");
                end <= duplicate_interval.start || *offset >= duplicate_interval.end
            }),
            "declared container preparation returned bytes from the undeclared duplicate interval \
             {duplicate_interval:?}: {requests:?}"
        );
        assert!(
            requests.iter().all(|(offset, returned)| {
                let end = offset
                    .checked_add(u64::try_from(*returned).unwrap())
                    .expect("traced returned interval must not overflow");
                *offset >= selected_interval.start && end <= selected_interval.end
            }),
            "declared container preparation returned bytes outside the selected container interval \
             {selected_interval:?}: {requests:?}"
        );
        assert!(
            requests.iter().any(|(offset, returned)| {
                let end = offset.saturating_add(u64::try_from(*returned).unwrap());
                *offset < selected_interval.end && end > selected_interval.start
            }),
            "selected container interval {selected_interval:?} was never returned: {requests:?}"
        );
        drop(requests);
        drop(selected);
        assert_eq!(permit.stats().current_bytes, prepared.retained_bytes());
        drop(prepared);
        assert_eq!(permit.close().unwrap().current_bytes, 0);
        assert_objstm_reader_cache_neutral(&authority, "xref-authoritative selected container");
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
                // A malformed stream's page content is asserted against the *eager* route
                // rather than a frozen literal: the contract is parity, and the lazy route
                // now recovers the same frame eager does.
                let eager = PdfDocument::from_bytes(&raw).unwrap();
                let eager_content = eager
                    .access
                    .page_content(eager.access.pages().unwrap()[0].id)
                    .unwrap();
                let recovered = b"BT /F1 12 Tf 72 720 Td (malformed stream) Tj ET";
                for control in [file.as_ref().unwrap(), bytes.as_ref().unwrap()] {
                    let page = control.pages().unwrap()[0].id;
                    assert!(
                        control
                            .checked_page_content_matches(page, eager_content.as_ref())
                            .unwrap(),
                        "{name}"
                    );
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
        assert_eq!(diagnostics.snapshot().route, OpenRoute::IndexedFile);
    }

    /// The lazy route's output is a pure function of the document — not of the thread count.
    ///
    /// The bug this locks: compressed-object resolution used to draw its allowance from one
    /// process-wide 64 MiB budget with `acquire_available` — "however much is left right now".
    /// One rayon thread always saw the whole budget and every object resolved; several threads
    /// saw race-dependent slices, resolution failed with a resource limit, and the consumers'
    /// legacy `*_or_empty` suppression turned that into a *shorter page*, so the same document
    /// rendered different bytes on every run. `objstm_pages.pdf` is the committed repro (40
    /// pages whose page dicts, resources and mediaboxes all live in two `/ObjStm` containers);
    /// against the pre-fix build it rendered four distinct outputs in four runs, all far short
    /// of eager. The rendering below is pinned to a four-thread pool so the test does not
    /// depend on how many cores the machine running it has.
    #[test]
    fn indexed_render_is_thread_count_independent_and_matches_eager() {
        let _test_lock = crate::access::indexed_test_lock();
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures_pdf/objstm_pages.pdf"
        ));
        let raw = std::fs::read(&path).expect("committed object-stream fixture");
        let eager = PdfDocument::from_bytes(&raw)
            .expect("eager open")
            .render(crate::Mode::Page, false, false);
        assert!(eager.contains("Section 40: object stream residency"), "fixture text");

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("four-thread pool builds");
        let mut renders = Vec::new();
        for _ in 0..2 {
            let control = open_indexed_file_internal(&path, None).expect("indexed open");
            let document = PdfDocument::finish_indexed_open(control, Some(path.clone()));
            renders.push(pool.install(|| document.render(crate::Mode::Page, false, false)));
        }
        assert_eq!(renders[0], renders[1], "two indexed renders at 4 threads differ");
        assert_eq!(renders[0], eager, "indexed render differs from eager");
    }

    /// A file whose cross-reference machinery is destroyed still opens *lazily*, by rescan.
    ///
    /// `damaged_startxref.pdf` and `damaged_startxref_intact.pdf` (see `tests/gen_fixtures.py`)
    /// are the same seven objects, the same classic table, and the same trailer; they differ in
    /// the ten digits of the `startxref` operand, which the damaged one points into the middle
    /// of a content stream. Both of lopdf's readers used to fail closed on that — the eager one
    /// still does, so before this the document did not open on any engine, and the lazy route's
    /// counted eager fallback had nothing to fall back *to*.
    ///
    /// The indexed reader now rebuilds its index from a single forward scan of the body for
    /// `N G obj` headers. Because the twin is byte-identical apart from those ten digits, the
    /// recovered index being the index the intact table describes is checkable as identical
    /// rendered output — against the intact file on *both* engines, not just against itself.
    #[test]
    fn damaged_startxref_recovers_on_the_indexed_route_and_matches_the_intact_twin() {
        let _test_lock = crate::access::indexed_test_lock();
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures_pdf/adversarial");
        let damaged_path = PathBuf::from(format!("{dir}/damaged_startxref.pdf"));
        let intact_path = PathBuf::from(format!("{dir}/damaged_startxref_intact.pdf"));
        let damaged = std::fs::read(&damaged_path).expect("committed damaged fixture");
        let intact = std::fs::read(&intact_path).expect("committed intact twin");
        assert_eq!(damaged.len(), intact.len(), "the twins must differ only in the operand");
        let differing: Vec<usize> = damaged
            .iter()
            .zip(&intact)
            .enumerate()
            .filter(|(_, (left, right))| left != right)
            .map(|(index, _)| index)
            .collect();
        let operand = intact
            .windows(b"startxref\n".len())
            .position(|window| window == b"startxref\n")
            .expect("the intact twin declares a startxref")
            + b"startxref\n".len();
        assert!(
            !differing.is_empty() && differing.iter().all(|index| (operand..operand + 10).contains(index)),
            "the twins must differ only inside the ten startxref digits: {differing:?}"
        );

        // The eager engine has no rescan, so it still cannot open the damaged file at all.
        assert!(
            PdfDocument::from_bytes_with_engine(&damaged, Engine::Eager).is_err(),
            "eager must still fail closed"
        );

        let eager = PdfDocument::from_bytes_with_engine(&intact, Engine::Eager)
            .expect("eager opens the intact twin")
            .render(crate::Mode::Page, false, false);
        assert!(eager.contains("Rescan page 2 line 20"), "fixture text");

        for (name, path) in [("damaged", &damaged_path), ("intact", &intact_path)] {
            let control = open_indexed_file_internal(path, None).expect("indexed open");
            let document = PdfDocument::finish_indexed_open(control, Some(path.to_path_buf()));
            // A recovered open is still an indexed open: nothing was counted as a fallback,
            // so the route the caller is told about is the route that ran.
            let diagnostics = document.route_diagnostics();
            assert_eq!(diagnostics.indexed_opens, 1, "{name}");
            assert_eq!(diagnostics.fallback_opens, 0, "{name}");
            assert_eq!(document.engine(), EngineRoute::Lazy, "{name}");
            assert_eq!(document.page_count(), 2, "{name}");
            assert_eq!(document.render(crate::Mode::Page, false, false), eager, "{name}");
        }
    }

    /// Every object-stream encoding inside the bounded decode envelope keeps its document
    /// on the lazy route, and decodes to what eager decodes.
    ///
    /// `objstm_filter_forms.pdf` (see `tests/gen_fixtures.py`) carries one `/ObjStm` container
    /// per admitted encoding — bare `/FlateDecode`, Flate in a one-element `/Filter` array,
    /// Flate under a PNG predictor, Flate under TIFF Predictor 2, Flate behind an ASCII85 or
    /// ASCIIHex prefix, and the two forms that name their predictor in an **array
    /// `/DecodeParms`** parallel to an array `/Filter` — each holding the page dictionary,
    /// resources and indirect `/MediaBox` of one page. The indexed reader decodes a container
    /// under a charged allowance and so admits only what it can reproduce inside that budget;
    /// before this, the envelope was "no filter or a bare `/FlateDecode`" and *seven* of these
    /// eight containers refused, dropping the whole document to the eager engine.
    #[test]
    fn every_admitted_object_stream_encoding_stays_lazy_and_matches_eager() {
        let _test_lock = crate::access::indexed_test_lock();
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures_pdf/objstm_filter_forms.pdf"
        ));
        let raw = std::fs::read(&path).expect("committed filter-form fixture");
        let eager = PdfDocument::from_bytes_with_engine(&raw, Engine::Eager)
            .expect("eager open")
            .render(crate::Mode::Page, false, false);
        assert!(eager.contains("Container 8: encoding form"), "fixture text");

        let control = open_indexed_file_internal(&path, None).expect("indexed open");
        let document = PdfDocument::finish_indexed_open(control, Some(path.clone()));
        assert_eq!(document.engine(), EngineRoute::Lazy);
        assert_eq!(document.route_diagnostics().fallback_opens, 0);
        assert_eq!(document.page_count(), 8);
        assert_eq!(document.render(crate::Mode::Page, false, false), eager);

        // The route above resolves containers through the shared resolver, which is not the
        // surface the envelope gates. Drive the *bounded* preparation the envelope guards —
        // the one a memory-budgeted consumer uses — once per container, so each admitted
        // encoding is proved individually rather than inferred from the render.
        let reader = IndexedReader::open(BytesSource::from(raw)).expect("indexed reader");
        let containers: std::collections::BTreeSet<lopdf::ObjectId> = reader
            .object_ids()
            .into_iter()
            .filter_map(|id| match reader.object_location(id) {
                Some(IndexedObjectLocation::Compressed { container, .. }) => Some(container),
                _ => None,
            })
            .collect();
        assert_eq!(containers.len(), 8, "one container per encoding form");
        for container in containers {
            prepare_objstm(&reader, container)
                .unwrap_or_else(|error| panic!("container {container:?} refused: {error:?}"));
        }
    }

    /// A content stream and an image whose predictor is named in an **array `/DecodeParms`**
    /// both decode, on both engines.
    ///
    /// ISO 32000-1 7.4.1: whenever `/Filter` is an array, `/DecodeParms` is an array parallel
    /// to it, `null` for the layers that take no parameters. lopdf read the key with
    /// `Object::as_dict`, so that array reached every layer as `None` and the Flate layer ran
    /// with **no predictor** — decoding to wrong bytes with no error at all, on every stream
    /// kind and both engines. `array_decode_parms.pdf` (see `tests/gen_fixtures.py`) is page
    /// 1 as `[/ASCII85Decode /FlateDecode]` + `[null << /Predictor 12 … >>]` carrying a
    /// figure whose image is the same chain over `/Colors 3`, and page 2 as the one-element
    /// spelling `[/FlateDecode]` + `[<< /Predictor 2 … >>]`.
    ///
    /// Before the fix this rendered as *nothing*: the undecoded predictor bytes parse as no
    /// operator at all, so the page came back blank rather than wrong-looking.
    #[test]
    fn array_decode_parms_decode_on_both_engines() {
        let _test_lock = crate::access::indexed_test_lock();
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures_pdf/array_decode_parms.pdf"
        ));
        let raw = std::fs::read(&path).expect("committed array-decode-parms fixture");
        let expected = [
            "Decode Parameters Carried As An Array",
            "them travel as an array parallel to that list, with a null opposite the layer",
            "Figure 1: An RGB ramp whose predictor is named in the array.",
            "A One Element Filter Array",
            "parameters are a one-element array too. This page uses TIFF Predictor 2.",
        ];

        let eager = PdfDocument::from_bytes_with_engine(&raw, Engine::Eager).expect("eager open");
        assert_eq!(eager.page_count(), 2);
        let eager_html = eager.render(crate::Mode::Page, false, false);
        for needle in expected {
            assert!(eager_html.contains(needle), "eager lost {needle:?}");
        }

        // The figure's image must survive too: a dropped predictor turns the ramp into noise,
        // which is not something a text assertion can see.
        let images = eager.extract_images();
        assert_eq!(images.len(), 1, "one image XObject");
        let decoded = image::load_from_memory(&images[0].data).expect("the image decodes").to_rgb8();
        assert_eq!((decoded.width(), decoded.height()), (32, 24));
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 0], "the ramp's origin");
        assert_eq!(decoded.get_pixel(31, 23).0, [248, 230, 14], "the ramp's far corner");

        let control = open_indexed_file_internal(&path, None).expect("indexed open");
        let lazy = PdfDocument::finish_indexed_open(control, Some(path.clone()));
        assert_eq!(lazy.engine(), EngineRoute::Lazy);
        assert_eq!(lazy.render(crate::Mode::Page, false, false), eager_html, "lazy must match eager");
    }

    /// Both engines read the *newest* structure tree of a hybrid-reference file whose
    /// re-tagging revision is reachable only through an inner section's `/XRefStm`.
    ///
    /// `hybrid_xref_revision.pdf` is three revisions deep (see `tests/gen_fixtures.py`): the
    /// base tags `Quarterly Totals` as an `/H2` in a `/Sect` and the table's first row as
    /// `/TD`; revision 2 supersedes those structure elements out of a *second* `/ObjStm`,
    /// making the heading a spanning `/TH` inside the table; revision 3 appends only an
    /// `/Info` dictionary and carries no `/XRefStm` of its own.
    ///
    /// Both engines used to get this wrong, differently, which is how it was found — the one
    /// eager/indexed disagreement in a 196-document corpus sweep, on the re-tagged
    /// `gov_usgs_usgs70277647`. lopdf's eager bootstrap read `/XRefStm` off the newest
    /// trailer only, so a chain whose newest section has none never merged a supplement at
    /// all: it kept no xref entry for the compressed structure elements, and the duplicate
    /// member guard then resolved them from whichever `/ObjStm` came first — the stale one.
    /// The indexed reader had the mirror defect *within* a revision, letting the classic
    /// section's mandatory free mask outrank the supplement that lifts it, so it lost the
    /// declaration entirely and fell back to inference. ISO 32000-1 7.5.8.4 settles both:
    /// a section's supplement supersedes that section, and the section supersedes everything
    /// older.
    ///
    /// The assertions are the two symptoms the corpus document showed, in one file: the
    /// declared `/TH` cells must be present, and the heading must NOT survive as a heading.
    #[test]
    fn hybrid_xref_revision_reads_the_newest_structure_tree_on_both_engines() {
        let _test_lock = crate::access::indexed_test_lock();
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures_pdf/hybrid_xref_revision.pdf"
        ));
        let raw = std::fs::read(&path).expect("committed hybrid-reference fixture");

        let eager = PdfDocument::from_bytes(&raw)
            .expect("eager open")
            .render(crate::Mode::Section, false, false);
        let control = open_indexed_file_internal(&path, None).expect("indexed open");
        let indexed = PdfDocument::finish_indexed_open(control, Some(path.clone()))
            .render(crate::Mode::Section, false, false);

        for (engine, html) in [("eager", &eager), ("indexed", &indexed)] {
            // Revision 2 promoted the header row to `/TH` and pulled the heading in as a
            // spanning `/TH`; reading revision 1 emits `<td>` and a separate heading.
            assert!(
                html.contains(r#"<th scope="colgroup" colspan="3">Quarterly Totals</th>"#),
                "{engine} lost the newest revision's spanning header cell:\n{html}"
            );
            assert!(
                html.contains(r#"<th scope="col">Region</th>"#),
                "{engine} lost the newest revision's declared /TH cells:\n{html}"
            );
            assert!(
                !html.contains("<h2>"),
                "{engine} kept the superseded revision's heading:\n{html}"
            );
            assert!(
                !html.contains("sec-quarterly-totals"),
                "{engine} kept the superseded revision's section anchor:\n{html}"
            );
            assert!(
                html.contains("Figures are provisional until the annual restatement."),
                "{engine} dropped the paragraph both revisions keep:\n{html}"
            );
        }
        assert_eq!(indexed, eager, "engines disagree on the hybrid-reference fixture");
    }

    /// The public constructors carry honest route provenance for whichever engine the internal
    /// `DISTILLPDF_ENGINE` selector picked.
    ///
    /// This used to assert "public constructors are eager, full stop" — a restatement of the old
    /// plan's "indexed is unreachable from production" rule, which the Phase B selector
    /// deliberately retires. The invariants worth keeping are the ones checked here: the
    /// *default* (unset selector) is still eager on both constructors, an indexed route reports
    /// indexed provenance, and no route ever claims both engines at once.
    #[test]
    fn public_constructors_carry_honest_route_provenance_for_the_selected_engine() {
        let (fixture, raw) = route_fixture();
        let file = PdfDocument::open(fixture.to_str().unwrap()).unwrap();
        let bytes = PdfDocument::from_bytes(&raw).unwrap();
        let indexed_selected = engine_selection().prefers_indexed();
        for (actual, eager_route, eager_mode, indexed_route, indexed_mode) in [
            (
                file.route_diagnostics(),
                OpenRoute::EagerFile,
                SourceMode::EagerMaterializedFile,
                OpenRoute::IndexedFile,
                SourceMode::FileDescriptor,
            ),
            (
                bytes.route_diagnostics(),
                OpenRoute::EagerBytes,
                SourceMode::SharedBytes,
                OpenRoute::IndexedBytes,
                SourceMode::SharedBytes,
            ),
        ] {
            // Exactly one engine opened this handle, and the fallback counter is only ever set
            // together with an eager open — a silent fallback would break this.
            assert_eq!(actual.eager_opens + actual.indexed_opens, 1);
            assert!(actual.fallback_opens <= actual.eager_opens);
            if !indexed_selected {
                assert_eq!(actual.route, eager_route);
                assert_eq!(actual.reason, OpenReason::PublicCompatibility);
                assert_eq!(actual.source_mode, eager_mode);
                assert_eq!(actual.eager_opens, 1);
                assert_eq!(actual.indexed_opens, 0);
                assert_eq!(actual.fallback_opens, 0);
            } else if actual.indexed_opens == 1 {
                assert_eq!(actual.route, indexed_route);
                assert_eq!(actual.reason, OpenReason::InternalMeasurement);
                assert_eq!(actual.source_mode, indexed_mode);
                assert_eq!(actual.fallback_opens, 0);
            } else {
                // Counted eager fallback: the indexed open refused this document.
                assert_eq!(actual.route, eager_route);
                assert_eq!(actual.source_mode, eager_mode);
                assert_eq!(actual.fallback_opens, 1);
            }
        }
    }

    /// The selector is off unless `DISTILLPDF_ENGINE` names a route — the property that keeps
    /// released builds eager no matter what else changes in `open`/`from_bytes`.
    #[test]
    fn engine_selector_defaults_to_eager_for_unset_and_unknown_values() {
        for value in [
            None,
            Some(""),
            Some("eager"),
            Some("lazy"),
            Some("INDEXED"),
            Some("indexed "),
            Some("indexed-strictly"),
            Some("1"),
        ] {
            assert_eq!(
                parse_engine_selection(value),
                Engine::Eager,
                "{value:?} must stay eager"
            );
        }
        assert_eq!(parse_engine_selection(Some("indexed")), Engine::Lazy);
        assert_eq!(
            parse_engine_selection(Some("indexed-strict")),
            Engine::LazyStrict
        );
        assert!(!Engine::Eager.prefers_indexed());
        assert!(Engine::Lazy.prefers_indexed());
        assert!(Engine::LazyStrict.prefers_indexed());
        // The process-wide selection agrees with the grammar for this process's environment.
        assert_eq!(
            engine_selection(),
            parse_engine_selection(std::env::var("DISTILLPDF_ENGINE").ok().as_deref())
        );
    }

    /// The two grammars are separate on purpose: `DISTILLPDF_ENGINE` is internal and spells the
    /// lazy route `indexed`, while the public `engine=` keyword spells it `lazy` and cannot name
    /// the strict diagnostic variant at all. Neither may start accepting the other's spellings —
    /// that is how the env var stays unstable and the public grammar stays small.
    #[test]
    fn the_public_engine_grammar_is_two_values_and_is_not_the_env_grammar() {
        assert_eq!(Engine::parse("eager").unwrap(), Engine::Eager);
        assert_eq!(Engine::parse("lazy").unwrap(), Engine::Lazy);
        for rejected in ["indexed", "indexed-strict", "lazy-strict", "LAZY", "lazy ", ""] {
            let message = Engine::parse(rejected).unwrap_err().to_string();
            assert!(
                message.contains("expected \"eager\" or \"lazy\""),
                "{rejected:?} must be rejected with the allowed values listed, got {message}"
            );
        }
        // …and the env grammar does NOT answer to the public spellings (checked as `Eager` in
        // `engine_selector_defaults_to_eager_for_unset_and_unknown_values`).
        assert_eq!(parse_engine_selection(Some("lazy")), Engine::Eager);
    }

    /// An explicit engine wins over `DISTILLPDF_ENGINE`, in **both** directions.
    ///
    /// This is the property that lets the whole suite be re-run under
    /// `DISTILLPDF_ENGINE=indexed` while a caller that asked for `Engine::Eager` still gets the
    /// eager engine — and the reason the env var can stay internal: it only ever decides what an
    /// *unspecified* open does. Both assertions run in either mode, so this test proves the
    /// override in whichever direction the process environment is not already pointing.
    #[test]
    fn an_explicit_engine_overrides_the_environment_selector() {
        let (fixture, raw) = route_fixture();
        let path = fixture.to_str().unwrap();

        for document in [
            PdfDocument::open_with_engine(path, Engine::Eager).unwrap(),
            PdfDocument::from_bytes_with_engine(&raw, Engine::Eager).unwrap(),
        ] {
            assert_eq!(document.engine(), EngineRoute::Eager);
            assert_eq!(document.route_diagnostics().indexed_opens, 0);
        }

        for document in [
            PdfDocument::open_with_engine(path, Engine::Lazy).unwrap(),
            PdfDocument::from_bytes_with_engine(&raw, Engine::Lazy).unwrap(),
        ] {
            // This fixture opens indexed, so `Lazy` really is served lazily rather than falling
            // back — anything else would make the assertion vacuous.
            assert_eq!(document.engine(), EngineRoute::Lazy);
            assert_eq!(document.route_diagnostics().eager_opens, 0);
        }

        // The env-driven constructors keep answering to the environment, not to the last
        // explicit choice: nothing above is sticky.
        assert_eq!(
            PdfDocument::open(path).unwrap().engine(),
            if engine_selection().prefers_indexed() { EngineRoute::Lazy } else { EngineRoute::Eager }
        );
    }

    /// A valid PDF whose catalog object is larger than the indexed reader's per-object decode
    /// envelope (`INDEXED_OBJECT_BYTES`, 4 MiB): the index refuses it at open, the eager engine
    /// reads it without complaint. Written by hand rather than taken from `tests/fixtures_pdf/`
    /// because every committed fixture opens *indexed* — the refusal has to be constructed.
    fn oversized_catalog_pdf() -> Vec<u8> {
        let pad = "x".repeat(5 * 1024 * 1024);
        let content = "BT /F1 12 Tf 72 700 Td (fallback) Tj ET";
        let objects = [
            format!("<< /Type /Catalog /Pages 2 0 R /Pad ({pad}) >>"),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut body = b"%PDF-1.5\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = body.len();
        let size = objects.len() + 1;
        body.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for offset in &offsets {
            body.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        body.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        body
    }

    /// `engine()` reports the route that actually ran — a refused lazy open says
    /// `lazy (eager fallback)`, never `lazy` — and [`Engine::LazyStrict`] refuses instead of
    /// falling back, which is the whole point of having it.
    #[test]
    fn a_refused_lazy_open_falls_back_and_says_so_unless_it_is_strict() {
        let raw = oversized_catalog_pdf();

        let document = PdfDocument::from_bytes_with_engine(&raw, Engine::Lazy).unwrap();
        assert_eq!(document.engine(), EngineRoute::LazyEagerFallback);
        let snapshot = document.route_diagnostics();
        assert_eq!(snapshot.fallback_opens, 1);
        assert_eq!(snapshot.eager_opens, 1);
        assert_eq!(snapshot.indexed_opens, 0);
        // The fallback is a route change, not a content change: the document reads normally.
        assert_eq!(document.page_count(), 1);
        assert!(document.extract_text().contains("fallback"));

        // Eager on the same bytes is the same document — and does NOT claim a fallback, because
        // nothing was refused.
        let eager = PdfDocument::from_bytes_with_engine(&raw, Engine::Eager).unwrap();
        assert_eq!(eager.engine(), EngineRoute::Eager);
        assert_eq!(eager.extract_text(), document.extract_text());

        let strict = PdfDocument::from_bytes_with_engine(&raw, Engine::LazyStrict);
        assert!(
            matches!(strict, Err(Error::Open(ref message)) if message.contains("indexed open failed")),
            "strict must surface the refusal, got {strict:?}",
            strict = strict.map(|document| document.engine())
        );
    }

    /// A valid one-page PDF reached through a chain of `depth` single-kid `/Pages` nodes.
    ///
    /// Nothing here is malformed: every node carries `/Type /Pages`, a one-element `/Kids`,
    /// `/Count 1` and a `/Parent` backlink, and the leaf is an ordinary page. Only the *nesting*
    /// is unusual, which is exactly the axis the indexed reader caps (`INDEXED_PAGE_TREE_DEPTH`).
    fn deep_page_tree_pdf(depth: usize) -> Vec<u8> {
        let content = "BT /F1 12 Tf 72 700 Td (deep page tree) Tj ET";
        let last = 2 + depth - 1;
        let (stream, font) = (last + 2, last + 3);
        let mut objects = vec!["<< /Type /Catalog /Pages 2 0 R >>".to_string()];
        for index in 0..depth {
            let node = 2 + index;
            let parent = if index > 0 {
                format!(" /Parent {} 0 R", node - 1)
            } else {
                String::new()
            };
            objects.push(format!(
                "<< /Type /Pages /Kids [{} 0 R] /Count 1{parent} >>",
                node + 1
            ));
        }
        objects.push(format!(
            "<< /Type /Page /Parent {last} 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font} 0 R >> >> /Contents {stream} 0 R >>"
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
        assert_eq!(objects.len(), font, "object numbering must stay contiguous");

        let mut body = b"%PDF-1.5\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = body.len();
        let size = objects.len() + 1;
        body.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for offset in &offsets {
            body.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        body.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        body
    }

    /// A `/Pages` tree nested past the indexed reader's depth cap must be a *refusal*, not a
    /// silently empty document.
    ///
    /// Before the fix the reader stopped walking at the cap and handed back an empty page map
    /// with no error, so the open "succeeded", the counted eager fallback never fired, and the
    /// caller got a zero-page, zero-text document that still reported `engine == "lazy"`. The
    /// cap now raises a typed structural error, which routes through the existing fallback.
    #[test]
    fn a_page_tree_deeper_than_the_cap_falls_back_instead_of_rendering_blank() {
        let raw = deep_page_tree_pdf(300);

        let eager = PdfDocument::from_bytes_with_engine(&raw, Engine::Eager).unwrap();
        assert_eq!(eager.engine(), EngineRoute::Eager);
        assert_eq!(eager.page_count(), 1);
        assert!(eager.extract_text().contains("deep page tree"));

        let document = PdfDocument::from_bytes_with_engine(&raw, Engine::Lazy).unwrap();
        assert_eq!(document.engine(), EngineRoute::LazyEagerFallback);
        let snapshot = document.route_diagnostics();
        assert_eq!(snapshot.fallback_opens, 1);
        assert_eq!(snapshot.eager_opens, 1);
        assert_eq!(snapshot.indexed_opens, 0);
        // The silent-blank shape is what this locks out: a non-zero page count and the eager
        // text, never an empty document that claims the lazy engine ran.
        assert_eq!(document.page_count(), eager.page_count());
        assert_eq!(document.extract_text(), eager.extract_text());

        let strict = PdfDocument::from_bytes_with_engine(&raw, Engine::LazyStrict);
        assert!(
            matches!(strict, Err(Error::Open(ref message)) if message.contains("indexed open failed")),
            "strict must surface the refusal, got {strict:?}",
            strict = strict.map(|document| document.engine())
        );
    }

    /// The cap is inclusive: a tree that sits exactly at `INDEXED_PAGE_TREE_DEPTH` is still a
    /// lazy open, so the refusal above is a cap boundary and not a general deep-tree failure.
    #[test]
    fn a_page_tree_at_the_cap_still_opens_lazily() {
        let raw = deep_page_tree_pdf(crate::access::INDEXED_PAGE_TREE_DEPTH);
        let document = PdfDocument::from_bytes_with_engine(&raw, Engine::Lazy).unwrap();
        assert_eq!(document.engine(), EngineRoute::Lazy);
        assert_eq!(document.page_count(), 1);
        assert!(document.extract_text().contains("deep page tree"));
    }

    /// A page tree that is deep **and** broad: `depth` nested `/Pages` nodes, each holding the
    /// next node *and* a page of its own, so descending always leaves a sibling behind.
    ///
    /// Leaf order is the tree's left-to-right leaf order, which runs deepest-first here: the
    /// first page is `broad level depth-1` and the last is `broad level 0`.
    fn deep_broad_page_tree_pdf(depth: usize) -> Vec<u8> {
        let font = 2;
        let node = |level: usize| 3 + 3 * level;
        let mut objects = vec![
            format!("<< /Type /Catalog /Pages {} 0 R >>", node(0)),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        for level in 0..depth {
            let (page, stream) = (node(level) + 1, node(level) + 2);
            let kids = if level + 1 < depth {
                format!("[{} 0 R {page} 0 R]", node(level + 1))
            } else {
                format!("[{page} 0 R]")
            };
            let parent = if level > 0 {
                format!(" /Parent {} 0 R", node(level - 1))
            } else {
                String::new()
            };
            objects.push(format!(
                "<< /Type /Pages /Kids {kids} /Count {}{parent} >>",
                depth - level
            ));
            objects.push(format!(
                "<< /Type /Page /Parent {} 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font} 0 R >> >> /Contents {stream} 0 R >>",
                node(level)
            ));
            let content = format!("BT /F1 12 Tf 72 700 Td (broad level {level}) Tj ET");
            objects.push(format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ));
        }

        let mut body = b"%PDF-1.5\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = body.len();
        let size = objects.len() + 1;
        body.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for offset in &offsets {
            body.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        body.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        );
        body
    }

    /// The eager half of the depth question: a tree deep *and* broad enough to retain a frame
    /// per level used to lose every page below the 256th, silently.
    ///
    /// The single-kid chain above never retained a sibling, so it walked to any depth and the
    /// lazy refusal could fall back to a correct eager read. This shape retains one at every
    /// level, and eager dropped 43 of its 300 pages with no error — which made eager an unsafe
    /// floor under exactly the fallback the lazy cap depends on, and a liar as the parity
    /// oracle. Both routes must now report the same 300 pages and the same text, including the
    /// levels that used to vanish.
    #[test]
    fn a_deep_and_broad_page_tree_keeps_every_page_on_both_routes() {
        // Past the indexed cap, so the lazy route must take the counted fallback.
        const DEPTH: usize = 300;
        const _: () = assert!(DEPTH > crate::access::INDEXED_PAGE_TREE_DEPTH);
        let raw = deep_broad_page_tree_pdf(DEPTH);

        let eager = PdfDocument::from_bytes_with_engine(&raw, Engine::Eager).unwrap();
        assert_eq!(eager.engine(), EngineRoute::Eager);
        assert_eq!(eager.page_count(), DEPTH);
        let text = eager.extract_text();
        for level in [0, 1, 255, 256, 257, DEPTH - 1] {
            assert!(
                text.contains(&format!("broad level {level}")),
                "level {level} is missing from the eager text"
            );
        }

        // Past the cap, so the index refuses and the counted fallback runs — onto an eager
        // walk that is now trustworthy.
        let document = PdfDocument::from_bytes_with_engine(&raw, Engine::Lazy).unwrap();
        assert_eq!(document.engine(), EngineRoute::LazyEagerFallback);
        assert_eq!(document.page_count(), eager.page_count());
        assert_eq!(document.extract_text(), text);
    }

    /// An incremental revision that **frees** an object the base revision defines, leaving the
    /// page's `/Contents` array still naming it.
    ///
    /// A reference to a freed object is a reference to null (ISO 32000-1, 7.3.10), so the
    /// second content stream is gone by the rules of the format even though its bytes are
    /// still in the file. Eager used to discard free entries entirely, so the base section's
    /// definition won the merge and the deleted sentence came back — a redaction undone by the
    /// reader, on the default engine.
    #[test]
    fn a_freed_object_is_not_resurrected_by_either_route() {
        let kept = "Public summary paragraph.";
        let redacted = "Case officer home address on file.";
        let stream = |y: u32, text: &str| {
            let content = format!("BT /F1 12 Tf 72 {y} Td ({text}) Tj ET");
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            )
        };
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents [5 0 R 6 0 R] >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            stream(700, kept),
            stream(660, redacted),
        ];

        let mut body = b"%PDF-1.5\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(body.len());
            body.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let base_xref = body.len();
        body.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
        for offset in &offsets {
            body.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        body.extend_from_slice(
            format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{base_xref}\n%%EOF\n")
                .as_bytes(),
        );

        // Revision 2 deletes object 6 and touches nothing else: the free-list head points at
        // it, and its own entry points back at the head carrying the generation a reuse would
        // take. The page still says `/Contents [5 0 R 6 0 R]`.
        let redact_xref = body.len();
        body.extend_from_slice(
            b"xref\n0 1\n0000000006 65535 f \n6 1\n0000000000 00001 f \n",
        );
        body.extend_from_slice(
            format!(
                "trailer\n<< /Size 7 /Root 1 0 R /Prev {base_xref} >>\nstartxref\n{redact_xref}\n%%EOF\n"
            )
            .as_bytes(),
        );

        for engine in [Engine::Eager, Engine::Lazy] {
            let document = PdfDocument::from_bytes_with_engine(&body, engine).unwrap();
            let text = document.extract_text();
            assert!(text.contains(kept), "{engine:?} lost the surviving paragraph");
            assert!(
                !text.contains(redacted),
                "{engine:?} resurrected the freed object"
            );
            assert_eq!(document.page_count(), 1);
        }

        // Same bytes, same rendering, whichever engine reads them.
        let eager = PdfDocument::from_bytes_with_engine(&body, Engine::Eager).unwrap();
        let lazy = PdfDocument::from_bytes_with_engine(&body, Engine::Lazy).unwrap();
        assert_eq!(lazy.engine(), EngineRoute::Lazy);
        assert_eq!(
            lazy.render(html::Mode::Section, false, false),
            eager.render(html::Mode::Section, false, false)
        );
    }

    /// Belt and braces behind the typed limit above: the index never *serves* a zero-page
    /// document, whatever produced the empty page map.
    ///
    /// Every structural limit is supposed to report itself, but a limit that ever truncates
    /// again would land here — an empty page map, a successful open, a blank render labelled
    /// `lazy`. Refusing the shape means the worst case is a counted fallback to eager, which is
    /// the reference for what "no pages" means. Here eager also finds no pages, and the routes
    /// still agree.
    #[test]
    fn an_empty_page_map_is_refused_rather_than_served() {
        let raw = {
            let objects = [
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
            ];
            let mut body = b"%PDF-1.5\n".to_vec();
            let mut offsets = Vec::new();
            for (index, object) in objects.iter().enumerate() {
                offsets.push(body.len());
                body.extend_from_slice(
                    format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes(),
                );
            }
            let xref = body.len();
            let size = objects.len() + 1;
            body.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
            for offset in &offsets {
                body.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
            }
            body.extend_from_slice(
                format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                    .as_bytes(),
            );
            body
        };

        let document = PdfDocument::from_bytes_with_engine(&raw, Engine::Lazy).unwrap();
        assert_eq!(document.engine(), EngineRoute::LazyEagerFallback);
        assert_eq!(document.route_diagnostics().fallback_opens, 1);
        let eager = PdfDocument::from_bytes_with_engine(&raw, Engine::Eager).unwrap();
        assert_eq!(document.page_count(), eager.page_count());
    }

    /// The three route strings are the Python `Pdf.engine` property's values — behaviour-locked
    /// here so a rename in Rust cannot silently change what Python returns.
    #[test]
    fn engine_route_strings_are_locked() {
        assert_eq!(EngineRoute::Eager.as_str(), "eager");
        assert_eq!(EngineRoute::Lazy.as_str(), "lazy");
        assert_eq!(EngineRoute::LazyEagerFallback.as_str(), "lazy (eager fallback)");
        assert_eq!(EngineRoute::LazyEagerFallback.to_string(), "lazy (eager fallback)");
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

    /// Build a PDF that drives the parallel loader through lopdf's object-stream merge: object
    /// 5 is defined **twice**, in two different `/Type /ObjStm` containers with different values,
    /// and the cross-reference stream places it in container 10. Both containers are `Normal`
    /// entries, so both are read — on separate rayon workers — but only container 10's copy is
    /// the one the table names, so the merge has a decision to make on every load. The filler
    /// objects exist only to give the parallel loader enough entries to actually split the work.
    ///
    /// The fixture originally left object 5 out of the table entirely, which made lopdf keep
    /// *both* copies and settle the winner by thread-completion order. The fork now refuses a
    /// member the table places nowhere (`objstm_member_xref_authority_test`), so that shape no
    /// longer reaches the merge at all — hence the cross-reference **stream**, which unlike the
    /// classic table this fixture used before can actually express a compressed entry.
    fn racing_objstm_pdf() -> Vec<u8> {
        const MAX_ID: u32 = 401;
        const XREF_ID: u32 = 401;

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

        // A cross-reference stream, /W [1 4 2]: one type byte, a four-byte offset-or-container,
        // a two-byte generation-or-index. It describes itself at `startxref`.
        let startxref = out.len();
        let mut rows: Vec<u8> = Vec::new();
        let mut row = |kind: u8, second: u32, third: u16, rows: &mut Vec<u8>| {
            rows.push(kind);
            rows.extend_from_slice(&second.to_be_bytes());
            rows.extend_from_slice(&third.to_be_bytes());
        };
        for id in 0..=MAX_ID {
            match id {
                0 => row(0, 0, 65535, &mut rows),
                5 => row(2, 10, 0, &mut rows), // the table places object 5 in container 10
                XREF_ID => row(1, startxref as u32, 0, &mut rows),
                _ => match offs.iter().find(|(n, _)| *n == id) {
                    Some((_, o)) => row(1, *o as u32, 0, &mut rows),
                    None => row(0, 0, 65535, &mut rows),
                },
            }
        }
        out.extend_from_slice(
            format!(
                "{XREF_ID} 0 obj\n<< /Type /XRef /Size {} /Root 1 0 R /Index [0 {}] /W [1 4 2] \
                 /Length {} >>\nstream\n",
                MAX_ID + 1,
                MAX_ID + 1,
                rows.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&rows);
        out.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{startxref}\n%%EOF").as_bytes());
        out
    }

    /// The same bytes must always load to the same object map.
    ///
    /// Object-stream members reach the object map through a rayon `par_iter` whose workers
    /// finish in whatever order the scheduler gives them, and this fixture defines object 5 in
    /// two containers so that ordering has something to decide. Historically lopdf settled it by
    /// *thread-completion order* and the same bytes loaded two different ways at roughly 50/50
    /// (measured: 30/30 over 60 loads, and 3 distinct maps over 40 loads of a real USGS file,
    /// which flipped a table header in and out of `to_html`). Everything in this crate loads
    /// through [`load_mem_deterministic`], which confines that race to a private one-thread pool.
    /// A regression here means a call site went back to `Document::load_mem` — or that our pool
    /// stopped covering lopdf's `par_iter`.
    ///
    /// The answer is pinned as well as its stability: the cross-reference table places object 5
    /// in container 10, so `/Figure` must win every time and container 20's `/Artifact` must
    /// never appear. A deterministic-but-wrong winner is still a defect.
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
        // Guard the fixture itself: if the member ever stopped being expanded at all, this test
        // would pass vacuously and stop protecting anything.
        assert!(first.contains("/S"), "fixture no longer exercises the object-stream merge");
        assert!(first.contains("/Figure"), "the container the xref names must be the one that wins");
        assert!(!first.contains("/Artifact"), "the copy the xref does not name must not be loaded");
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

