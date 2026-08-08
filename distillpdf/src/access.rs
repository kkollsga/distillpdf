//! Owned PDF object/source access boundary.
//!
//! Extraction code must not retain a borrow into lopdf's eager `Document`: L3 replaces the
//! backend with on-demand owned resolution, where no such document-wide borrow exists.  Short
//! reads therefore happen through [`ObjectHandle::read`], while values that escape a read are
//! explicitly owned.  The eager implementation remains the compatibility oracle through L9.

use lopdf::{
    BytesSource, DecompressError, Dictionary, Document, IndexedReader, IndexedReaderCacheOptions,
    IndexedReaderError, IndexedReaderOptions, Object, ObjectId, PageMap, RandomAccessSource,
    SourceError, SourceResult,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

const SOURCE_CHUNK_BYTES: usize = 64 * 1024;
const MAX_RECOVERED_STREAM_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RECOVERY_INDEX_ENTRIES: usize = 65_536;
/// Ceiling on one page's concatenated decoded content, and on one decoded stream payload.
/// A fixed per-document constant: the same document degrades the same way on every run,
/// whatever else the process is doing.
const MAX_PAGE_CONTENT_BYTES: usize = 64 * 1024 * 1024;
const INDEXED_OBJECT_BYTES: u64 = 4 * 1024 * 1024;
const INDEXED_STREAM_BYTES: u64 = 64 * 1024 * 1024;
/// Per-document bounded resolution cache, sized from the source length alone so two opens of
/// the same document configure the same cache and no document's caching depends on another's.
const INDEXED_CACHE_MIN_BYTES: u64 = 8 * 1024 * 1024;
const INDEXED_CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const INDEXED_CACHE_SOURCE_DIVISOR: u64 = 8;
const INDEXED_CACHE_MIN_ENTRIES: usize = 4_096;
const INDEXED_CACHE_MAX_ENTRIES: usize = 65_536;
const INDEXED_CACHE_ENTRY_BYTES: u64 = 4_096;
// Frozen by `tests/lazy_engine_fixtures.py`: 128 reference hops are admitted and 129 fail.
const INDEXED_REFERENCE_DEPTH: usize = 128;
const INDEXED_PAGE_TREE_DEPTH: usize = 256;
const INDEXED_MAX_PAGES: usize = 1_000_000;
const EAGER_RESOURCE_DEPTH: usize = 100;
const INDEX_FIXED_BYTES: u64 = 33_554_432;
const INDEX_OBJECT_BYTES: u64 = 1_536;
const INDEX_PAGE_BYTES: u64 = 3_072;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceFailureKind {
    SourceChanged,
    Bounds,
    ResourceLimit,
    SourceIo,
    Backend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceFailure {
    kind: SourceFailureKind,
    detail: Arc<str>,
}

impl SourceFailure {
    fn new(kind: SourceFailureKind, detail: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    fn resource(detail: impl Into<Arc<str>>) -> Self {
        Self::new(SourceFailureKind::ResourceLimit, detail)
    }
}

impl fmt::Display for SourceFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl From<SourceError> for SourceFailure {
    fn from(error: SourceError) -> Self {
        Self::from(&error)
    }
}

impl From<&SourceError> for SourceFailure {
    fn from(error: &SourceError) -> Self {
        let kind = match *error {
            SourceError::SourceChanged => SourceFailureKind::SourceChanged,
            SourceError::RangeOverflow { .. } | SourceError::OutOfBounds { .. } => {
                SourceFailureKind::Bounds
            }
            SourceError::ReadLimitExceeded { .. }
            | SourceError::PlatformLimitExceeded { .. }
            | SourceError::AllocationFailed { .. } => SourceFailureKind::ResourceLimit,
            SourceError::UnexpectedEof { .. }
            | SourceError::InvalidReadCount { .. }
            | SourceError::Io(_) => SourceFailureKind::SourceIo,
            _ => SourceFailureKind::Backend,
        };
        Self::new(kind, Arc::<str>::from(error.to_string()))
    }
}

pub(crate) struct RecoveredStream {
    bytes: Vec<u8>,
}

impl AsRef<[u8]> for RecoveredStream {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Deref for RecoveredStream {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

pub(crate) struct PageContent {
    bytes: Vec<u8>,
}

impl PageContent {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl AsRef<[u8]> for PageContent {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Deref for PageContent {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl fmt::Debug for PageContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PageContent")
            .field(&self.bytes.len())
            .finish()
    }
}

impl PartialEq<&[u8]> for PageContent {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_ref() == *other
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckedSourceRange {
    offset: u64,
    length: u64,
}

struct SourceScan {
    streams: BTreeMap<u32, CheckedSourceRange>,
    sha256: String,
}

/// Per-document source services shared by every consumer of one access adapter.
///
/// The first recovery/hash caller performs one streaming pass. `OnceLock` is the single-flight
/// owner: concurrent callers wait for that pass and then share only the compact range map and
/// digest, never a whole-source buffer.
pub(crate) struct SourceRecovery {
    source: Arc<dyn RandomAccessSource>,
    scan: OnceLock<Result<SourceScan, SourceFailure>>,
}

impl SourceRecovery {
    pub(crate) fn new(source: Arc<dyn RandomAccessSource>) -> Self {
        Self {
            source,
            scan: OnceLock::new(),
        }
    }

    fn scanned(&self) -> Result<&SourceScan, SourceFailure> {
        self.scan
            .get_or_init(|| scan_source_streams(self.source.as_ref()))
            .as_ref()
            .map_err(Clone::clone)
    }

    fn recover_stream(&self, object: u32) -> Result<Option<RecoveredStream>, SourceFailure> {
        let Some(range) = self.scanned()?.streams.get(&object).copied() else {
            return Ok(None);
        };
        let bytes = read_bounded_range(
            self.source.as_ref(),
            range.offset,
            range.length,
            MAX_RECOVERED_STREAM_BYTES,
        )
        .map_err(SourceFailure::from)?;
        Ok(Some(RecoveredStream { bytes }))
    }

    fn sha256(&self) -> Result<String, SourceFailure> {
        self.scanned().map(|scan| scan.sha256.clone())
    }

    fn materialize(&self, limit: u64) -> SourceResult<Vec<u8>> {
        let length = self.source.len()?;
        read_bounded_range(self.source.as_ref(), 0, length, limit)
    }

    fn len(&self) -> SourceResult<u64> {
        self.source.len()
    }

    /// Ask the *uncached* source whether it still is what it was at open.
    fn validate_unchanged(&self) -> Result<(), SourceFailure> {
        self.source
            .validate_unchanged()
            .map_err(SourceFailure::from)
    }
}

fn read_bounded_range(
    source: &dyn RandomAccessSource,
    offset: u64,
    length: u64,
    limit: u64,
) -> SourceResult<Vec<u8>> {
    if length > limit {
        return Err(SourceError::ReadLimitExceeded {
            requested: length,
            limit,
        });
    }
    let source_len = source.len()?;
    let end = offset
        .checked_add(length)
        .ok_or(SourceError::RangeOverflow { offset, length })?;
    if end > source_len {
        return Err(SourceError::OutOfBounds {
            offset,
            length,
            source_len,
        });
    }
    let output_len = usize::try_from(length).map_err(|_| SourceError::PlatformLimitExceeded {
        requested: length,
        limit: usize::MAX as u64,
    })?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(output_len)
        .map_err(|_| SourceError::AllocationFailed { requested: length })?;
    output.resize(output_len, 0);
    let mut completed = 0usize;
    while completed < output.len() {
        let take = (output.len() - completed).min(SOURCE_CHUNK_BYTES);
        let at = offset
            .checked_add(completed as u64)
            .ok_or(SourceError::RangeOverflow { offset, length })?;
        source.read_exact_at(at, &mut output[completed..completed + take])?;
        completed += take;
    }
    Ok(output)
}

fn scan_source_streams(source: &dyn RandomAccessSource) -> Result<SourceScan, SourceFailure> {
    let length = source.len().map_err(SourceFailure::from)?;
    let mut hash = Sha256::new();
    let mut scanner = StreamScanner::default();
    let mut chunk = vec![0u8; SOURCE_CHUNK_BYTES];
    let mut offset = 0u64;
    while offset < length {
        let take = usize::try_from((length - offset).min(SOURCE_CHUNK_BYTES as u64))
            .expect("source chunk is bounded to 64 KiB");
        source
            .read_exact_at(offset, &mut chunk[..take])
            .map_err(SourceFailure::from)?;
        hash.update(&chunk[..take]);
        for (within, byte) in chunk[..take].iter().copied().enumerate() {
            scanner.push(offset + within as u64, byte);
        }
        offset += take as u64;
        if scanner.overflowed {
            return Err(SourceFailure::resource(format!(
                "recovery stream index exceeds {MAX_RECOVERY_INDEX_ENTRIES} entries"
            )));
        }
    }
    source.validate_unchanged().map_err(SourceFailure::from)?;
    let digest = hash.finalize();
    let mut sha256 = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(sha256, "{byte:02x}");
    }
    Ok(SourceScan {
        streams: scanner.streams,
        sha256,
    })
}

#[derive(Default)]
struct StreamScanner {
    recent: VecDeque<u8>,
    object: Option<u32>,
    pending_stream_start: Option<(u64, bool)>,
    stream_start: Option<u64>,
    streams: BTreeMap<u32, CheckedSourceRange>,
    overflowed: bool,
}

impl StreamScanner {
    fn push(&mut self, position: u64, byte: u8) {
        if let Some((after_keyword, saw_cr)) = self.pending_stream_start {
            if position == after_keyword && byte == b'\r' {
                self.pending_stream_start = Some((position + 1, true));
                self.push_recent(byte);
                return;
            }
            if position == after_keyword && byte == b'\n' {
                self.pending_stream_start = None;
                self.stream_start = Some(position + 1);
                self.push_recent(byte);
                return;
            }
            // Legacy recovery skips at most one CR and then one LF.
            let start = if saw_cr { after_keyword } else { position };
            self.pending_stream_start = None;
            self.stream_start = Some(start);
        }

        self.push_recent(byte);

        if let Some(start) = self.stream_start {
            if self.recent_ends_with(b"endstream") {
                let keyword_start = position + 1 - b"endstream".len() as u64;
                let mut end = keyword_start;
                let prefix_len = self.recent.len() - b"endstream".len();
                let had_lf = prefix_len > 0 && self.recent[prefix_len - 1] == b'\n';
                if had_lf {
                    end -= 1;
                }
                if end > start {
                    let before_newline = prefix_len.saturating_sub(usize::from(had_lf));
                    if before_newline > 0 && self.recent[before_newline - 1] == b'\r' {
                        end -= 1;
                    }
                }
                if let Some(object) = self.object {
                    if end > start && !self.streams.contains_key(&object) {
                        if self.streams.len() == MAX_RECOVERY_INDEX_ENTRIES {
                            self.overflowed = true;
                        } else {
                            self.streams.insert(
                                object,
                                CheckedSourceRange {
                                    offset: start,
                                    length: end - start,
                                },
                            );
                        }
                    }
                }
                self.stream_start = None;
                self.object = None;
            }
            return;
        }

        if self.recent_ends_with(b" 0 obj") {
            let suffix = b" 0 obj".len();
            let before = self.recent.len() - suffix;
            let mut begin = before;
            while begin > 0 && self.recent[begin - 1].is_ascii_digit() {
                begin -= 1;
            }
            if begin < before {
                let mut object = 0u32;
                let mut valid = true;
                for index in begin..before {
                    valid &= object
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(u32::from(self.recent[index] - b'0')))
                        .map(|value| object = value)
                        .is_some();
                }
                self.object = valid.then_some(object);
            }
        }
        if self.object.is_some()
            && self.recent_ends_with(b"stream")
            && self.keyword_has_left_boundary(b"stream")
        {
            self.pending_stream_start = Some((position + 1, false));
        }
    }

    fn push_recent(&mut self, byte: u8) {
        const RECENT_LIMIT: usize = 64;
        if self.recent.len() == RECENT_LIMIT {
            self.recent.pop_front();
        }
        self.recent.push_back(byte);
    }

    fn recent_ends_with(&self, needle: &[u8]) -> bool {
        needle.len() <= self.recent.len()
            && needle
                .iter()
                .rev()
                .zip(self.recent.iter().rev())
                .all(|(expected, actual)| expected == actual)
    }

    fn keyword_has_left_boundary(&self, keyword: &[u8]) -> bool {
        let Some(before) = self.recent.len().checked_sub(keyword.len() + 1) else {
            return true;
        };
        let byte = self.recent[before];
        byte.is_ascii_whitespace()
            || matches!(
                byte,
                b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
            )
    }
}

/// One page entry, detached from the backend's page-map allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PageRef {
    pub(crate) number: u32,
    pub(crate) id: ObjectId,
}

/// Stable failure phase used by the suppression contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccessPhase {
    Resolve,
    Object,
    Trailer,
    Catalog,
    Page,
    PageContent,
    Pages,
    FallbackText,
    Resources,
}

/// Stable failure class. Backend messages remain in `detail`; policy never branches on them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccessKind {
    Type,
    Bounds,
    ResourceLimit,
    SourceChanged,
    SourceIo,
    PasswordRequired,
    InvalidPassword,
    Encryption,
    InvalidEncryptDictionary,
    ObjectDecryption,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by the fault-injection adapter
    Injected,
    Backend,
}

/// A stable internal access failure key `(phase,page,object,kind,detail)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AccessError {
    pub(crate) phase: AccessPhase,
    pub(crate) page: Option<u32>,
    pub(crate) object: ObjectId,
    pub(crate) kind: AccessKind,
    pub(crate) detail: String,
}

impl AccessError {
    pub(crate) fn object(object: ObjectId, error: impl fmt::Display) -> Self {
        let detail = error.to_string();
        Self {
            phase: AccessPhase::Resolve,
            page: None,
            object,
            kind: AccessKind::Backend,
            detail,
        }
    }

    pub(crate) fn typed(object: ObjectId, kind: AccessKind, error: impl fmt::Display) -> Self {
        let mut failure = Self::object(object, error);
        failure.kind = kind;
        failure
    }

    fn source(error: SourceFailure) -> Self {
        let kind = match error.kind {
            SourceFailureKind::SourceChanged => AccessKind::SourceChanged,
            SourceFailureKind::Bounds => AccessKind::Bounds,
            SourceFailureKind::ResourceLimit => AccessKind::ResourceLimit,
            SourceFailureKind::SourceIo => AccessKind::SourceIo,
            SourceFailureKind::Backend => AccessKind::Backend,
        };
        Self::typed((0, 0), kind, error)
    }

    pub(crate) fn at(mut self, phase: AccessPhase, page: Option<u32>) -> Self {
        self.phase = phase;
        self.page = page;
        self
    }
}

impl fmt::Display for AccessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} page {:?} object {} {} {:?}: {}",
            self.phase, self.page, self.object.0, self.object.1, self.kind, self.detail
        )
    }
}

fn indexed_error(object: ObjectId, error: &IndexedReaderError) -> AccessError {
    if let IndexedReaderError::Source(source) = error {
        let mut failure = AccessError::source(SourceFailure::from(source));
        failure.object = object;
        return failure;
    }
    let kind = match error {
        IndexedReaderError::StructureLimitExceeded { .. }
        | IndexedReaderError::EntryLimitExceeded { .. }
        | IndexedReaderError::RevisionLimitExceeded { .. }
        | IndexedReaderError::IndirectHeaderLimitExceeded { .. }
        | IndexedReaderError::ObjectLimitExceeded { .. }
        | IndexedReaderError::ScalarResourceLimit { .. }
        | IndexedReaderError::ScalarResolutionCancelled { .. }
        | IndexedReaderError::ScalarResolutionClosed { .. }
        | IndexedReaderError::StreamLimitExceeded { .. }
        | IndexedReaderError::ResolutionDepthExceeded { .. }
        | IndexedReaderError::ObjectStreamCacheBypass { .. }
        | IndexedReaderError::PageCountLimitExceeded { .. } => AccessKind::ResourceLimit,
        IndexedReaderError::StartXrefOutOfBounds { .. }
        | IndexedReaderError::NegativeStreamLength { .. } => AccessKind::Bounds,
        IndexedReaderError::NotScalarObject { .. }
        | IndexedReaderError::NotStreamObject { .. }
        | IndexedReaderError::UnsupportedBoundedScalar { .. } => AccessKind::Type,
        IndexedReaderError::PasswordRequired => AccessKind::PasswordRequired,
        IndexedReaderError::InvalidPassword => AccessKind::InvalidPassword,
        IndexedReaderError::Encryption(_) => AccessKind::Encryption,
        IndexedReaderError::InvalidEncryptDictionary => AccessKind::InvalidEncryptDictionary,
        IndexedReaderError::ObjectDecryption { .. } => AccessKind::ObjectDecryption,
        _ => AccessKind::Backend,
    };
    AccessError::typed(object, kind, error)
}

fn fatal_lazy_access(error: &AccessError) -> bool {
    matches!(
        error.kind,
        AccessKind::ResourceLimit
            | AccessKind::SourceChanged
            | AccessKind::SourceIo
            | AccessKind::Bounds
            | AccessKind::PasswordRequired
            | AccessKind::InvalidPassword
            | AccessKind::Encryption
            | AccessKind::InvalidEncryptDictionary
            | AccessKind::ObjectDecryption
    )
}

#[derive(Default)]
pub(crate) struct IndexedAdapterCounters {
    pub(crate) page_map_builds: AtomicU64,
    pub(crate) index_estimated_bytes: AtomicU64,
    pub(crate) index_objects: AtomicU64,
    pub(crate) index_pages: AtomicU64,
}

#[cfg(test)]
pub(crate) fn indexed_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Legacy disposition for a failed immutable operation. This table is the checked-in L2d
/// authority: adapters return typed errors; consumers keep the eager route's historical
/// suppression/fallback at these exact operation boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Suppression {
    SkipNode,
    SkipMetadata,
    EmptyPage,
    EmptyDocument,
    EmptyText,
    EmptyResources,
}

pub(crate) const LEGACY_SUPPRESSION: &[(AccessPhase, Suppression)] = &[
    (AccessPhase::Resolve, Suppression::SkipNode),
    (AccessPhase::Object, Suppression::SkipNode),
    (AccessPhase::Trailer, Suppression::SkipMetadata),
    (AccessPhase::Catalog, Suppression::SkipMetadata),
    (AccessPhase::Page, Suppression::SkipNode),
    (AccessPhase::PageContent, Suppression::EmptyPage),
    (AccessPhase::Pages, Suppression::EmptyDocument),
    (AccessPhase::FallbackText, Suppression::EmptyText),
    (AccessPhase::Resources, Suppression::EmptyResources),
];

pub(crate) fn legacy_suppression(phase: AccessPhase) -> Option<Suppression> {
    LEGACY_SUPPRESSION
        .iter()
        .find_map(|(candidate, disposition)| (*candidate == phase).then_some(*disposition))
}

fn suppress_default<T: Default>(result: Result<T, AccessError>, disposition: Suppression) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            debug_assert_eq!(legacy_suppression(error.phase), Some(disposition));
            T::default()
        }
    }
}

/// A resolved object whose storage remains pinned for the lifetime of this handle.
///
/// `Eager` stores the document and requested id rather than cloning an arbitrarily large object
/// graph. `Owned` is the lazy-reader shape and the *only* lazy shape: one `Arc<Object>`, either
/// handed over by the indexed reader's shared resolver or built from a direct trailer value.
/// Neither variant exposes an object-derived borrow beyond the closure passed to [`Self::read`].
#[derive(Clone)]
enum ObjectOwner {
    Eager {
        document: Arc<Document>,
        id: ObjectId,
    },
    EagerTrailerEntry {
        document: Arc<Document>,
        key: Vec<u8>,
    },
    Owned {
        object: Arc<Object>,
        id: ObjectId,
    },
}

#[derive(Clone)]
enum PathStep {
    DictionaryKey(Vec<u8>),
    StreamDictionaryKey(Vec<u8>),
    ArrayIndex(usize),
}

#[derive(Clone)]
pub(crate) struct ObjectHandle {
    owner: ObjectOwner,
    path: Vec<PathStep>,
}

/// A typed object handle that can only be inspected as a stream.
#[derive(Clone)]
pub(crate) struct StreamHandle {
    object: ObjectHandle,
}

/// A dictionary view whose root object owner remains pinned for every short read.
#[derive(Clone)]
pub(crate) struct DictionaryHandle {
    object: ObjectHandle,
}

impl DictionaryHandle {
    fn new(object: ObjectHandle) -> Result<Self, AccessError> {
        let id = object.root_id();
        if !object.read(|value| value.as_dict().is_ok())? {
            return Err(AccessError::typed(
                id,
                AccessKind::Type,
                "resolved object is not a dictionary",
            ));
        }
        Ok(Self { object })
    }

    pub(crate) fn read<R>(&self, inspect: impl FnOnce(&Dictionary) -> R) -> Result<R, AccessError> {
        let id = self.object.root_id();
        self.object
            .read(|value| value.as_dict().map(inspect))?
            .map_err(|error| AccessError::object(id, error))
    }

    pub(crate) fn entry(
        &self,
        access: &dyn DocumentAccess,
        key: &[u8],
    ) -> Result<ObjectHandle, AccessError> {
        self.object.dictionary_entry(access, key)
    }
}

impl StreamHandle {
    fn new(id: ObjectId, object: ObjectHandle) -> Result<Self, AccessError> {
        let is_stream = object.read(|value| value.as_stream().is_ok())?;
        if !is_stream {
            return Err(AccessError::typed(
                id,
                AccessKind::Type,
                "resolved object is not a stream",
            ));
        }
        Ok(Self { object })
    }

    /// Inspect the stream while its object owner is pinned. A type mismatch degrades to `None`.
    pub(crate) fn read<R>(&self, inspect: impl FnOnce(&lopdf::Stream) -> R) -> Option<R> {
        self.object
            .read(|value| value.as_stream().ok().map(inspect))
            .ok()
            .flatten()
    }

    /// A dictionary-valued stream entry that remains pinned to this stream's object owner.
    pub(crate) fn dictionary_entry(
        &self,
        access: &dyn DocumentAccess,
        key: &[u8],
    ) -> Result<DictionaryHandle, AccessError> {
        DictionaryHandle::new(self.object.dictionary_entry(access, key)?)
    }
}

impl ObjectHandle {
    fn eager(document: Arc<Document>, id: ObjectId) -> Self {
        Self {
            owner: ObjectOwner::Eager { document, id },
            path: Vec::new(),
        }
    }

    fn eager_trailer_entry(document: Arc<Document>, key: &[u8]) -> Self {
        Self {
            owner: ObjectOwner::EagerTrailerEntry {
                document,
                key: key.to_vec(),
            },
            path: Vec::new(),
        }
    }

    /// Inspect the resolved value without allowing its borrow to escape the handle.
    pub(crate) fn read<R>(&self, inspect: impl FnOnce(&Object) -> R) -> Result<R, AccessError> {
        let (mut object, id) = match &self.owner {
            ObjectOwner::Eager { document, id } => {
                let object = document
                    .get_object(*id)
                    .map_err(|error| AccessError::object(*id, error))?;
                (object, *id)
            }
            ObjectOwner::EagerTrailerEntry { document, key } => {
                let object = document
                    .trailer
                    .get(key)
                    .map_err(|error| AccessError::object((0, 0), error))?;
                (object, (0, 0))
            }
            ObjectOwner::Owned { object, id } => (object.as_ref(), *id),
        };
        for step in &self.path {
            object = match step {
                PathStep::DictionaryKey(key) => object
                    .as_dict()
                    .and_then(|dictionary| dictionary.get(key))
                    .map_err(|error| AccessError::object(id, error))?,
                PathStep::StreamDictionaryKey(key) => object
                    .as_stream()
                    .and_then(|stream| stream.dict.get(key))
                    .map_err(|error| AccessError::object(id, error))?,
                PathStep::ArrayIndex(index) => object
                    .as_array()
                    .ok()
                    .and_then(|array| array.get(*index))
                    .ok_or_else(|| {
                        AccessError::typed(
                            id,
                            AccessKind::Bounds,
                            format!("array index {index} is out of bounds"),
                        )
                    })?,
            };
        }
        Ok(inspect(object))
    }

    /// Decode this handle's stream under a fixed caller-declared ceiling.
    pub(crate) fn decoded_stream_bytes(&self, maximum: usize) -> Result<Vec<u8>, AccessError> {
        let id = self.root_id();
        Ok(self
            .read(|object| object.as_stream()?.get_plain_content_with_limit(maximum))?
            .map_err(|error| match error {
                lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded { .. }) => {
                    AccessError::typed(
                        id,
                        AccessKind::ResourceLimit,
                        format!("decoded stream exceeds the {maximum}-byte allowance"),
                    )
                }
                other => AccessError::object(id, other),
            })?
            .into_boxed_slice()
            .into_vec())
    }

    fn owned(id: ObjectId, object: Object) -> Self {
        Self::shared(id, Arc::new(object))
    }

    /// The lazy route's single owner shape: the reader's own `Arc<Object>`, shared not cloned.
    fn shared(id: ObjectId, object: Arc<Object>) -> Self {
        Self {
            owner: ObjectOwner::Owned { object, id },
            path: Vec::new(),
        }
    }

    fn child(&self, access: &dyn DocumentAccess, step: PathStep) -> Result<Self, AccessError> {
        let reference = {
            let mut path = self.path.clone();
            path.push(step.clone());
            let candidate = Self {
                owner: self.owner.clone(),
                path,
            };
            candidate.read(|object| object.as_reference().ok())?
        };
        if let Some(id) = reference {
            access.object(id)
        } else {
            let mut path = self.path.clone();
            path.push(step);
            Ok(Self {
                owner: self.owner.clone(),
                path,
            })
        }
    }

    /// A dictionary entry that stays attached to the root object which owns an inline value.
    pub(crate) fn dictionary_entry(
        &self,
        access: &dyn DocumentAccess,
        key: &[u8],
    ) -> Result<Self, AccessError> {
        let step = self.read(|object| match object {
            Object::Dictionary(_) => Some(PathStep::DictionaryKey(key.to_vec())),
            Object::Stream(_) => Some(PathStep::StreamDictionaryKey(key.to_vec())),
            _ => None,
        })?;
        self.child(
            access,
            step.ok_or_else(|| {
                AccessError::typed(self.root_id(), AccessKind::Type, "object has no dictionary")
            })?,
        )
    }

    /// An array entry that stays attached to the root object which owns it.
    pub(crate) fn array_entry(
        &self,
        access: &dyn DocumentAccess,
        index: usize,
    ) -> Result<Self, AccessError> {
        self.child(access, PathStep::ArrayIndex(index))
    }

    pub(crate) fn root_id(&self) -> ObjectId {
        match &self.owner {
            ObjectOwner::Eager { id, .. } | ObjectOwner::Owned { id, .. } => *id,
            ObjectOwner::EagerTrailerEntry { .. } => (0, 0),
        }
    }

    /// The indirect object represented by this handle itself, excluding inline descendants.
    ///
    /// Structure-tree cycle detection must track referenced elements but not direct child
    /// dictionaries, which share their owner's root id and cannot form reference cycles.
    pub(crate) fn indirect_id(&self) -> Option<ObjectId> {
        if !self.path.is_empty() {
            return None;
        }
        match &self.owner {
            ObjectOwner::Eager { id, .. } | ObjectOwner::Owned { id, .. } => Some(*id),
            ObjectOwner::EagerTrailerEntry { .. } => None,
        }
    }
}

/// Backend-neutral access to immutable PDF objects, pages and source bytes.
///
/// The trait is object-safe so eager and lazy implementations are runtime-selectable. Resolved
/// objects cross this boundary only as handles; raw bytes cross as an immutable random-access
/// source rather than a document-wide `&[u8]`.
pub(crate) trait DocumentAccess: Send + Sync {
    fn object(&self, id: ObjectId) -> Result<ObjectHandle, AccessError>;
    fn trailer_entry(&self, key: &[u8]) -> Result<ObjectHandle, AccessError>;
    fn catalog(&self) -> Result<DictionaryHandle, AccessError> {
        let root = self
            .trailer_entry(b"Root")
            .map_err(|error| error.at(AccessPhase::Catalog, None))?;
        DictionaryHandle::new(root).map_err(|error| error.at(AccessPhase::Catalog, None))
    }
    fn page(&self, id: ObjectId) -> Result<DictionaryHandle, AccessError> {
        let page = self
            .object(id)
            .map_err(|error| error.at(AccessPhase::Page, None))?;
        DictionaryHandle::new(page).map_err(|error| error.at(AccessPhase::Page, None))
    }
    /// Materialize a page's decoded content with the selected backend's exact fallback policy.
    fn page_content(&self, page: ObjectId) -> Result<PageContent, AccessError>;
    fn stream(&self, id: ObjectId) -> Result<StreamHandle, AccessError> {
        StreamHandle::new(id, self.object(id)?)
    }
    fn pages(&self) -> Result<Vec<PageRef>, AccessError>;
    fn pages_or_empty(&self) -> Vec<PageRef> {
        suppress_default(self.pages(), Suppression::EmptyDocument)
    }
    /// Backend-compatible fallback text for one 1-indexed page.
    fn fallback_page_text(&self, page: u32) -> Result<String, AccessError>;
    fn fallback_page_text_or_empty(&self, page: u32) -> String {
        suppress_default(self.fallback_page_text(page), Suppression::EmptyText)
    }
    /// Every indexed indirect object id in deterministic order.
    fn object_ids(&self) -> Vec<ObjectId>;
    /// Page `/Resources` dictionaries in outermost-to-page overlay order.
    fn page_resource_chain(&self, page: ObjectId) -> Result<Vec<DictionaryHandle>, AccessError>;
    fn page_resource_chain_or_empty(&self, page: ObjectId) -> Vec<DictionaryHandle> {
        suppress_default(self.page_resource_chain(page), Suppression::EmptyResources)
    }
    fn source_recovery(&self) -> Arc<SourceRecovery>;
    fn source_len(&self) -> SourceResult<u64> {
        self.source_recovery().len()
    }
    /// Recover the encoded bytes of a malformed indirect stream without materializing the PDF.
    fn recover_source_stream(&self, object: u32) -> Result<Option<RecoveredStream>, AccessError> {
        self.source_recovery()
            .recover_stream(object)
            .map_err(AccessError::source)
    }
    /// Incremental SHA-256 from the same single-flight source pass as malformed recovery.
    fn source_sha256(&self) -> Result<String, AccessError> {
        self.source_recovery().sha256().map_err(AccessError::source)
    }
    /// Named whole-source exception for detached writers which must reparse mutable bytes.
    fn materialize_source_bounded(&self, limit: u64) -> SourceResult<Vec<u8>> {
        self.source_recovery().materialize(limit)
    }
}

/// The behavior-preserving adapter over lopdf's fully loaded object graph.
#[derive(Clone)]
pub(crate) struct EagerDocumentAdapter {
    document: Arc<Document>,
    recovery: Arc<SourceRecovery>,
}

impl EagerDocumentAdapter {
    pub(crate) fn new(document: Arc<Document>, raw: Arc<[u8]>) -> Self {
        let source: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(raw));
        Self {
            document,
            recovery: Arc::new(SourceRecovery::new(source)),
        }
    }
}

/// Test-only bridge for pre-boundary fixtures that build lopdf documents in memory.
#[cfg(test)]
#[allow(dead_code)] // compatibility fixture bridge; production never clones a Document here
pub(crate) fn test_adapter(document: &Document) -> EagerDocumentAdapter {
    EagerDocumentAdapter::new(Arc::new(document.clone()), Arc::from(&b""[..]))
}

#[cfg(test)]
pub(crate) fn test_adapter_with_source(document: &Document, raw: &[u8]) -> EagerDocumentAdapter {
    EagerDocumentAdapter::new(Arc::new(document.clone()), Arc::from(raw))
}

#[cfg(test)]
pub(crate) fn test_adapter_with_random_source(
    document: &Document,
    source: Arc<dyn RandomAccessSource>,
) -> EagerDocumentAdapter {
    EagerDocumentAdapter {
        document: Arc::new(document.clone()),
        recovery: Arc::new(SourceRecovery::new(source)),
    }
}

impl DocumentAccess for EagerDocumentAdapter {
    fn object(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
        // Validate now so a successfully-created handle is never a deferred missing-object
        // surprise. The immutable eager document makes the same lookup stable at read time.
        self.document
            .get_object(id)
            .map_err(|error| AccessError::object(id, error).at(AccessPhase::Object, None))?;
        Ok(ObjectHandle::eager(Arc::clone(&self.document), id))
    }

    fn trailer_entry(&self, key: &[u8]) -> Result<ObjectHandle, AccessError> {
        let value =
            self.document.trailer.get(key).map_err(|error| {
                AccessError::object((0, 0), error).at(AccessPhase::Trailer, None)
            })?;
        if let Object::Reference(id) = value {
            self.object(*id)
        } else {
            Ok(ObjectHandle::eager_trailer_entry(
                Arc::clone(&self.document),
                key,
            ))
        }
    }

    fn page_content(&self, page: ObjectId) -> Result<PageContent, AccessError> {
        self.document
            .get_dictionary(page)
            .map_err(|error| AccessError::object(page, error).at(AccessPhase::PageContent, None))?;
        Ok(PageContent::new(self.document.get_page_content(page)))
    }

    fn pages(&self) -> Result<Vec<PageRef>, AccessError> {
        Ok(self
            .document
            .get_pages()
            .into_iter()
            .map(|(number, id)| PageRef { number, id })
            .collect())
    }

    fn fallback_page_text(&self, page: u32) -> Result<String, AccessError> {
        self.document.extract_text(&[page]).map_err(|error| {
            AccessError::object((0, 0), error).at(AccessPhase::FallbackText, Some(page))
        })
    }

    fn object_ids(&self) -> Vec<ObjectId> {
        self.document.objects.keys().copied().collect()
    }

    fn page_resource_chain(&self, page: ObjectId) -> Result<Vec<DictionaryHandle>, AccessError> {
        let (own, inherited) = self
            .document
            .get_page_resources(page)
            .map_err(|error| AccessError::object(page, error).at(AccessPhase::Resources, None))?;
        let mut out: Vec<DictionaryHandle> = inherited
            .iter()
            .rev()
            .filter_map(|id| DictionaryHandle::new(self.object(*id).ok()?).ok())
            .collect();
        if own.is_some() {
            let page_handle = self.object(page)?;
            if let Ok(resources) = page_handle.dictionary_entry(self, b"Resources") {
                if let Ok(resources) = DictionaryHandle::new(resources) {
                    out.push(resources);
                }
            }
        }
        Ok(out)
    }

    fn source_recovery(&self) -> Arc<SourceRecovery> {
        Arc::clone(&self.recovery)
    }
}

/// Bounded random-access adapter over lopdf's immutable indexed reader.
///
/// This route is intentionally internal and selected only by L3a's explicit measurement controls;
/// public constructors remain eager. Every indirect object is resolved under a call-local permit;
/// the returned handle pins the bounded owner and its permit charges.
// Boundary-audit clone authority: Arc clones retain source/recovery/counter owners; the cached
// PageMap Result must be cloneable; raw stream cloning is the eager-compatible fallback for a
// non-limit decode error.
pub(crate) struct IndexedDocumentAdapter {
    reader: IndexedReader,
    recovery: Arc<SourceRecovery>,
    page_map: OnceLock<Result<Arc<PageMap>, AccessError>>,
    counters: Arc<IndexedAdapterCounters>,
}

/// Bounded cache budget for one document, derived from its source length alone.
///
/// Deliberately a pure function of the document: cache pressure inside the reader changes only
/// whether a resolved value is *retained*, never whether it resolves, and no other document or
/// thread in the process can shift these numbers.
fn indexed_cache_options(source_len: u64) -> IndexedReaderCacheOptions {
    let bytes = (source_len / INDEXED_CACHE_SOURCE_DIVISOR)
        .clamp(INDEXED_CACHE_MIN_BYTES, INDEXED_CACHE_MAX_BYTES);
    let entries = usize::try_from(source_len / INDEXED_CACHE_ENTRY_BYTES)
        .unwrap_or(INDEXED_CACHE_MAX_ENTRIES)
        .clamp(INDEXED_CACHE_MIN_ENTRIES, INDEXED_CACHE_MAX_ENTRIES);
    IndexedReaderCacheOptions::new(bytes, entries)
}

impl IndexedDocumentAdapter {
    pub(crate) fn open(
        source: Arc<dyn RandomAccessSource>,
        password: Option<Vec<u8>>,
    ) -> Result<Self, AccessError> {
        let options = IndexedReaderOptions {
            object_bytes: INDEXED_OBJECT_BYTES,
            stream_bytes: INDEXED_STREAM_BYTES,
            encoded_stream_bytes: None,
            endstream_tail_bytes: 64,
            reference_depth: INDEXED_REFERENCE_DEPTH,
            page_tree_depth: INDEXED_PAGE_TREE_DEPTH,
            max_pages: INDEXED_MAX_PAGES,
            password,
        };
        let source_len = source.len().map_err(|error| {
            AccessError::source(SourceFailure::from(error)).at(AccessPhase::Object, None)
        })?;
        let reader = IndexedReader::open_shared_cached(
            Arc::clone(&source),
            options,
            indexed_cache_options(source_len),
        )
        .map_err(|error| indexed_error((0, 0), &error))?;
        let (map, stats) = reader
            .page_map_with_stats()
            .map_err(|error| indexed_error((0, 0), &error).at(AccessPhase::Pages, None))?;
        let cap = INDEX_FIXED_BYTES
            .saturating_add(INDEX_OBJECT_BYTES.saturating_mul(stats.object_count() as u64))
            .saturating_add(INDEX_PAGE_BYTES.saturating_mul(stats.page_count() as u64));
        if stats.estimated_retained_bytes() > cap {
            return Err(AccessError::typed(
                (0, 0),
                AccessKind::ResourceLimit,
                format!(
                    "indexed metadata retains {} bytes, exceeding frozen cap {cap}",
                    stats.estimated_retained_bytes()
                ),
            ));
        }
        let map = Arc::new(map);
        let counters = Arc::new(IndexedAdapterCounters::default());
        counters.page_map_builds.store(1, Ordering::Relaxed);
        counters
            .index_estimated_bytes
            .store(stats.estimated_retained_bytes(), Ordering::Relaxed);
        counters
            .index_objects
            .store(stats.object_count() as u64, Ordering::Relaxed);
        counters
            .index_pages
            .store(stats.page_count() as u64, Ordering::Relaxed);
        Ok(Self {
            reader,
            recovery: Arc::new(SourceRecovery::new(source)),
            page_map: OnceLock::from(Ok(map)),
            counters,
        })
    }

    /// Index/page-map facts reported by the route diagnostics.
    pub(crate) fn counters(&self) -> Arc<IndexedAdapterCounters> {
        Arc::clone(&self.counters)
    }

    fn cached_page_map(&self) -> Result<Arc<PageMap>, AccessError> {
        self.page_map
            .get_or_init(|| {
                self.counters
                    .page_map_builds
                    .fetch_add(1, Ordering::Relaxed);
                self.reader
                    .page_map()
                    .map(Arc::new)
                    .map_err(|error| indexed_error((0, 0), &error).at(AccessPhase::Pages, None))
            })
            .clone()
    }

    /// One indirect object, resolved by the reader's own shared resolver.
    ///
    /// `resolve_object_shared` hands back the `Arc<Object>` it already owns — deduplicated and,
    /// for a compressed member, grouped with its object-stream container — under the per-reader
    /// bounded caches configured at open. Nothing here consults a process-wide allowance, so the
    /// outcome for a given `id` is a function of the document and this reader's fixed limits
    /// only, never of the thread count or of what else the process is doing.
    fn resolve_shared(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
        self.reader
            .resolve_object_shared(id)
            .map(|object| ObjectHandle::shared(id, object))
            .map_err(|error| indexed_error(id, error.as_ref()))
    }

    /// Follow an indirect-reference chain to its terminal value, as eager `get_object` does.
    ///
    /// The cycle set is built lazily. Virtually every call terminates on the first object — a
    /// reference *to* a reference is rare — and this is the hottest path in the adapter, so
    /// allocating a `HashSet` (and hashing into it) per resolve was pure overhead on the
    /// common case. `seen` therefore stays empty until a second hop actually happens, at which
    /// point it is seeded with every id already walked; the cycle verdict is identical.
    fn resolve_terminal(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
        let mut current = id;
        let mut seen: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
        let mut hops = 0;
        loop {
            if hops > 0 && !seen.insert(current) {
                return Err(AccessError::typed(
                    current,
                    AccessKind::Backend,
                    "indirect reference cycle",
                ));
            }
            let handle = self.resolve_shared(current)?;
            let next = handle.read(|object| object.as_reference().ok())?;
            let Some(next) = next else {
                return Ok(handle);
            };
            // The caller followed the first reference to `id`; this loop admits the remaining
            // hops up to the frozen end-to-end boundary.
            if hops >= INDEXED_REFERENCE_DEPTH.saturating_sub(1) {
                return Err(AccessError::typed(
                    current,
                    AccessKind::ResourceLimit,
                    format!("reference depth exceeds {INDEXED_REFERENCE_DEPTH}"),
                ));
            }
            if hops == 0 {
                seen.insert(id);
            }
            drop(handle);
            current = next;
            hops += 1;
        }
    }

    fn page_id(&self, number: u32) -> Result<ObjectId, AccessError> {
        let index = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
        self.cached_page_map()?
            .get(index)
            .map(|entry| entry.id())
            .ok_or_else(|| {
                AccessError::typed(
                    (0, 0),
                    AccessKind::Bounds,
                    format!("page {number} not found"),
                )
                .at(AccessPhase::FallbackText, Some(number))
            })
    }
}

impl DocumentAccess for IndexedDocumentAdapter {
    fn object(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
        self.resolve_terminal(id)
            .map_err(|error| error.at(AccessPhase::Object, None))
    }

    fn trailer_entry(&self, key: &[u8]) -> Result<ObjectHandle, AccessError> {
        // `trailer_entry_raw_owned` clones only the immutable raw trailer value, bounded by the
        // index the open already admitted, and never dereferences an indirect entry.
        let value = self.reader.trailer_entry_raw_owned(key).ok_or_else(|| {
            AccessError::typed((0, 0), AccessKind::Type, "missing trailer entry")
                .at(AccessPhase::Trailer, None)
        })?;
        if let Object::Reference(id) = value {
            return self
                .object(id)
                .map_err(|error| error.at(AccessPhase::Trailer, None));
        }
        Ok(ObjectHandle::owned((0, 0), value))
    }

    fn page_content(&self, page: ObjectId) -> Result<PageContent, AccessError> {
        // The reader's bounded source cache means a logical read no longer implies a physical
        // one, so a mutated source would otherwise go unnoticed. Ask once per page instead:
        // `FileSource`'s identity check is sticky, so one failure fails everything after it.
        self.recovery
            .validate_unchanged()
            .map_err(|error| AccessError::source(error).at(AccessPhase::PageContent, None))?;
        let page_handle = self
            .page(page)
            .map_err(|error| error.at(AccessPhase::PageContent, None))?;
        enum ContentsShape {
            Missing,
            InlineStream,
            Reference(ObjectId),
            Array,
            Other,
        }
        let contents_shape = page_handle
            .read(|dictionary| match dictionary.get(b"Contents").ok() {
                None => ContentsShape::Missing,
                Some(Object::Stream(_)) => ContentsShape::InlineStream,
                Some(Object::Reference(id)) => ContentsShape::Reference(*id),
                Some(Object::Array(_)) => ContentsShape::Array,
                Some(_) => ContentsShape::Other,
            })
            .map_err(|error| error.at(AccessPhase::PageContent, None))?;
        // Eager `get_page_contents` ignores an inline stream. It follows an indirect stream or
        // accepts a direct/indirect array whose members themselves must be references.
        let contents = match contents_shape {
            ContentsShape::Reference(id) => match self.object(id) {
                Ok(contents) => contents,
                Err(error) if fatal_lazy_access(&error) => {
                    return Err(error.at(AccessPhase::PageContent, None));
                }
                Err(_) => return Ok(PageContent::new(Vec::new())),
            },
            ContentsShape::Array => page_handle
                .entry(self, b"Contents")
                .map_err(|error| error.at(AccessPhase::PageContent, None))?,
            _ => return Ok(PageContent::new(Vec::new())),
        };
        let mut streams = Vec::new();
        let shape = contents
            .read(|object| match object {
                Object::Stream(_) => Some(Vec::new()),
                Object::Array(array) => Some(
                    array
                        .iter()
                        .enumerate()
                        .filter_map(|(index, value)| value.as_reference().ok().map(|_| index))
                        .collect(),
                ),
                _ => None,
            })
            .map_err(|error| error.at(AccessPhase::PageContent, None))?;
        match shape {
            Some(indices) if indices.is_empty() => {
                if contents
                    .read(|object| object.as_stream().is_ok())
                    .unwrap_or(false)
                {
                    streams.push(contents);
                }
            }
            Some(indices) => {
                for index in indices {
                    match contents.array_entry(self, index) {
                        Ok(stream) => streams.push(stream),
                        Err(error) if fatal_lazy_access(&error) => {
                            return Err(error.at(AccessPhase::PageContent, None));
                        }
                        Err(_) => {}
                    }
                }
            }
            None => return Ok(PageContent::new(Vec::new())),
        }

        let mut output: Vec<u8> = Vec::new();
        for stream in streams {
            if !stream
                .read(|object| object.as_stream().is_ok())
                .unwrap_or(false)
            {
                continue;
            }
            // One fixed per-page ceiling shared by every stream of this page. Whatever remains
            // of it is a function of this page's own earlier streams and nothing else.
            let remaining = MAX_PAGE_CONTENT_BYTES
                .saturating_sub(output.len())
                .saturating_sub(1);
            if remaining == 0 {
                return Err(AccessError::typed(
                    page,
                    AccessKind::ResourceLimit,
                    format!("page content exceeds {MAX_PAGE_CONTENT_BYTES} bytes"),
                )
                .at(AccessPhase::PageContent, None));
            }
            let result = stream.read(|object| {
                let Ok(stream) = object.as_stream() else {
                    return Ok(None);
                };
                let payload = match stream.decompressed_content_with_limit(remaining) {
                    Ok(bytes) => bytes,
                    Err(lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded {
                        ..
                    })) => {
                        return Err(AccessError::typed(
                            page,
                            AccessKind::ResourceLimit,
                            format!("page content exceeds the remaining {remaining}-byte allowance"),
                        ));
                    }
                    // Eager compatibility: a stream that fails to decode for any other reason
                    // contributes its raw bytes.
                    Err(_) if stream.content.len() <= remaining => stream.content.clone(),
                    Err(_) => {
                        return Err(AccessError::typed(
                            page,
                            AccessKind::ResourceLimit,
                            format!("page content exceeds the remaining {remaining}-byte allowance"),
                        ));
                    }
                };
                Ok(Some(payload))
            });
            match result {
                Ok(Ok(Some(payload))) => {
                    output.extend_from_slice(&payload);
                    output.push(b'\n');
                }
                Ok(Ok(None)) | Err(_) => {}
                Ok(Err(error)) => {
                    return Err(error.at(AccessPhase::PageContent, None));
                }
            }
        }
        Ok(PageContent::new(output))
    }

    fn pages(&self) -> Result<Vec<PageRef>, AccessError> {
        self.cached_page_map().map(|map| {
            map.iter()
                .enumerate()
                .map(|(index, page)| PageRef {
                    number: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    id: page.id(),
                })
                .collect()
        })
    }

    fn fallback_page_text(&self, page: u32) -> Result<String, AccessError> {
        let page_id = self.page_id(page)?;
        crate::text::fallback_page_text(self, page_id)
            .map_err(|error| error.at(AccessPhase::FallbackText, Some(page)))
    }

    fn object_ids(&self) -> Vec<ObjectId> {
        self.reader.object_ids()
    }

    fn page_resource_chain(&self, page: ObjectId) -> Result<Vec<DictionaryHandle>, AccessError> {
        self.recovery
            .validate_unchanged()
            .map_err(|error| AccessError::source(error).at(AccessPhase::Resources, None))?;
        let mut current = match self.page(page) {
            Ok(page) => page,
            Err(error) if fatal_lazy_access(&error) => {
                return Err(error.at(AccessPhase::Resources, None));
            }
            Err(_) => return Ok(Vec::new()),
        };
        let mut seen = std::collections::HashSet::new();
        let mut resources = Vec::new();
        for depth in 0..=EAGER_RESOURCE_DEPTH {
            let resource_shape = current
                .read(|dictionary| match dictionary.get(b"Resources").ok() {
                    Some(Object::Reference(_)) => 1u8,
                    Some(Object::Dictionary(_)) => 2u8,
                    _ => 0u8,
                })
                .map_err(|error| error.at(AccessPhase::Resources, None))?;
            let include = resource_shape == 1 || (depth == 0 && resource_shape == 2);
            if include {
                match current.entry(self, b"Resources") {
                    Ok(value) => match DictionaryHandle::new(value) {
                        Ok(dictionary) => resources.push(dictionary),
                        Err(error) if fatal_lazy_access(&error) => {
                            return Err(error.at(AccessPhase::Resources, None));
                        }
                        Err(_) => {}
                    },
                    Err(error) if fatal_lazy_access(&error) => {
                        return Err(error.at(AccessPhase::Resources, None));
                    }
                    Err(_) => {}
                }
            }
            let parent = current
                .read(|dictionary| {
                    dictionary
                        .get(b"Parent")
                        .ok()
                        .and_then(|value| value.as_reference().ok())
                })
                .map_err(|error| error.at(AccessPhase::Resources, None))?;
            let Some(parent) = parent else {
                break;
            };
            if !seen.insert(parent) {
                return Err(
                    AccessError::typed(parent, AccessKind::Backend, "page parent cycle")
                        .at(AccessPhase::Resources, None),
                );
            }
            if depth == EAGER_RESOURCE_DEPTH {
                return Err(AccessError::typed(
                    parent,
                    AccessKind::ResourceLimit,
                    "page parent depth exceeded",
                )
                .at(AccessPhase::Resources, None));
            }
            let parent_object = self
                .object(parent)
                .map_err(|error| error.at(AccessPhase::Resources, None))?;
            current = DictionaryHandle::new(parent_object)
                .map_err(|error| error.at(AccessPhase::Resources, None))?;
        }
        resources.reverse();
        Ok(resources)
    }

    fn source_recovery(&self) -> Arc<SourceRecovery> {
        Arc::clone(&self.recovery)
    }
}

/// Inspect a direct object as-is or resolve a reference through the selected backend first.
///
/// This is the owned-boundary replacement for helpers returning `Option<&Object>`. The callback
/// result cannot borrow from its argument, so arrays, dictionaries, streams, names and strings
/// remain pinned for the complete short read and cannot escape accidentally.
pub(crate) fn read_resolved<R>(
    access: &dyn DocumentAccess,
    object: &Object,
    inspect: impl FnOnce(&Object) -> R,
) -> Result<R, AccessError> {
    match object {
        Object::Reference(id) => access.object(*id)?.read(inspect),
        direct => Ok(inspect(direct)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lopdf::dictionary;
    use lopdf::SourceError;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    pub(crate) struct AccessCounts {
        pub(crate) opens: AtomicU64,
        pub(crate) object_reads: AtomicU64,
        pub(crate) object_lists: AtomicU64,
        pub(crate) page_reads: AtomicU64,
        pub(crate) fallback_text_reads: AtomicU64,
        pub(crate) resource_reads: AtomicU64,
        pub(crate) source_requests: AtomicU64,
        pub(crate) source_reads: AtomicU64,
        pub(crate) max_request: AtomicU64,
    }

    struct CountingSource {
        inner: Arc<dyn RandomAccessSource>,
        counts: Arc<AccessCounts>,
        fail_reads: bool,
    }



    impl RandomAccessSource for CountingSource {
        fn len(&self) -> SourceResult<u64> {
            self.inner.len()
        }

        fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
            self.counts.source_reads.fetch_add(1, Ordering::Relaxed);
            self.counts
                .max_request
                .fetch_max(out.len() as u64, Ordering::Relaxed);
            if self.fail_reads {
                return Err(SourceError::UnexpectedEof {
                    offset,
                    expected: out.len() as u64,
                    actual: 0,
                });
            }
            self.inner.read_at(offset, out)
        }

        fn validate_unchanged(&self) -> SourceResult<()> {
            self.inner.validate_unchanged()
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum FaultPoint {
        Object,
        Trailer,
        PageContent,
        Pages,
        FallbackText,
        Resources,
        Source,
    }

    impl FaultPoint {
        fn phase(self) -> AccessPhase {
            match self {
                Self::Object => AccessPhase::Object,
                Self::Trailer => AccessPhase::Trailer,
                Self::PageContent => AccessPhase::PageContent,
                Self::Pages => AccessPhase::Pages,
                Self::FallbackText => AccessPhase::FallbackText,
                Self::Resources => AccessPhase::Resources,
                Self::Source => AccessPhase::Resolve,
            }
        }
    }

    /// Test-only counter/fault wrapper. Every future boundary operation must pass through this
    /// shape so L2d can prove suppression without falling back to the eager backend.
    pub(crate) struct FaultAccess {
        inner: Arc<dyn DocumentAccess>,
        fault: Option<FaultPoint>,
        pub(crate) counts: Arc<AccessCounts>,
        recovery: Arc<SourceRecovery>,
    }

    impl FaultAccess {
        pub(crate) fn new(
            inner: Arc<dyn DocumentAccess>,
            fault: Option<FaultPoint>,
            counts: Arc<AccessCounts>,
        ) -> Self {
            let inner_recovery = inner.source_recovery();
            let counted_source: Arc<dyn RandomAccessSource> = Arc::new(CountingSource {
                inner: Arc::clone(&inner_recovery.source),
                counts: Arc::clone(&counts),
                fail_reads: fault == Some(FaultPoint::Source),
            });
            Self {
                inner,
                fault,
                counts,
                recovery: Arc::new(SourceRecovery::new(counted_source)),
            }
        }

        fn test_source(&self) -> Arc<dyn RandomAccessSource> {
            self.counts.source_requests.fetch_add(1, Ordering::Relaxed);
            Arc::clone(&self.recovery.source)
        }

        fn failure(&self, point: FaultPoint, object: ObjectId) -> Result<(), AccessError> {
            if self.fault == Some(point) {
                Err(AccessError::typed(
                    object,
                    AccessKind::Injected,
                    format!("injected {point:?} failure"),
                )
                .at(point.phase(), None))
            } else {
                Ok(())
            }
        }
    }

    impl DocumentAccess for FaultAccess {
        fn object(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
            self.counts.object_reads.fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::Object, id)?;
            self.inner.object(id)
        }

        fn trailer_entry(&self, key: &[u8]) -> Result<ObjectHandle, AccessError> {
            self.counts.object_reads.fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::Trailer, (0, 0))?;
            self.inner.trailer_entry(key)
        }

        fn page_content(&self, page: ObjectId) -> Result<PageContent, AccessError> {
            self.counts.object_reads.fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::PageContent, page)?;
            self.inner.page_content(page)
        }

        fn pages(&self) -> Result<Vec<PageRef>, AccessError> {
            self.counts.page_reads.fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::Pages, (0, 0))?;
            self.inner.pages()
        }

        fn fallback_page_text(&self, page: u32) -> Result<String, AccessError> {
            self.counts
                .fallback_text_reads
                .fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::FallbackText, (page, 0))
                .map_err(|error| error.at(AccessPhase::FallbackText, Some(page)))?;
            self.inner.fallback_page_text(page)
        }

        fn object_ids(&self) -> Vec<ObjectId> {
            self.counts.object_lists.fetch_add(1, Ordering::Relaxed);
            self.inner.object_ids()
        }

        fn page_resource_chain(
            &self,
            page: ObjectId,
        ) -> Result<Vec<DictionaryHandle>, AccessError> {
            self.counts.resource_reads.fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::Resources, page)?;
            self.inner.page_resource_chain(page)
        }

        fn source_recovery(&self) -> Arc<SourceRecovery> {
            Arc::clone(&self.recovery)
        }
    }

    fn adapter(objects: Vec<Object>, raw: &[u8]) -> (EagerDocumentAdapter, Vec<ObjectId>) {
        let mut document = Document::with_version("1.7");
        let ids = objects
            .into_iter()
            .map(|object| document.add_object(object))
            .collect();
        (
            EagerDocumentAdapter::new(Arc::new(document), Arc::from(raw)),
            ids,
        )
    }

    #[test]
    fn direct_and_reference_chains_match_eager_dereference() {
        let (adapter, ids) = adapter(
            vec![
                Object::Integer(41),
                Object::Reference((1, 0)),
                Object::Reference((2, 0)),
            ],
            b"source",
        );
        assert_eq!(
            read_resolved(&adapter, &Object::Integer(7), |o| o.as_i64().unwrap()).unwrap(),
            7
        );
        assert_eq!(
            adapter
                .object(ids[2])
                .unwrap()
                .read(|o| o.as_i64().unwrap())
                .unwrap(),
            41
        );
    }

    #[test]
    fn dangling_cycle_over_limit_and_generation_mismatch_are_errors() {
        let mut objects = vec![Object::Reference((999, 0)), Object::Reference((2, 0))];
        // Lopdf follows at most 128 references. Make a separate 130-hop chain.
        for number in 3..=132_u32 {
            objects.push(Object::Reference((number + 1, 0)));
        }
        objects.push(Object::Integer(9));
        let (adapter, ids) = adapter(objects, b"");

        assert!(adapter.object(ids[0]).is_err());
        assert!(adapter.object(ids[1]).is_err()); // 2 0 R points to itself
        assert!(adapter.object(ids[2]).is_err());
        assert!(adapter.object((ids.last().unwrap().0, 1)).is_err());
    }

    #[test]
    fn owned_handles_keep_their_owners_alive() {
        let handle = ObjectHandle::owned(
            (7, 0),
            Object::String(b"owned".to_vec(), lopdf::StringFormat::Literal),
        );
        assert_eq!(
            handle.read(|o| o.as_str().unwrap().to_vec()).unwrap(),
            b"owned"
        );
    }

    #[test]
    fn typed_stream_handles_reject_non_streams() {
        let (adapter, ids) = adapter(vec![Object::Integer(1)], b"");
        assert!(adapter.stream(ids[0]).is_err());
    }

    #[test]
    fn nested_handles_pin_inline_values_and_resolve_nested_references() {
        let nested = Object::Dictionary(lopdf::dictionary! {
            "Direct" => Object::Array(vec![Object::String(
                b"inline".to_vec(),
                lopdf::StringFormat::Literal,
            )]),
            "Indirect" => Object::Reference((1, 0)),
        });
        let (adapter, ids) = adapter(vec![Object::Integer(42), nested], b"");
        let root = adapter.object(ids[1]).unwrap();
        let direct = root
            .dictionary_entry(&adapter, b"Direct")
            .unwrap()
            .array_entry(&adapter, 0)
            .unwrap();
        let indirect = root.dictionary_entry(&adapter, b"Indirect").unwrap();
        drop(root);
        assert_eq!(
            direct
                .read(|object| object.as_str().unwrap().to_vec())
                .unwrap(),
            b"inline"
        );
        assert_eq!(
            indirect.read(|object| object.as_i64().unwrap()).unwrap(),
            42
        );
    }

    #[test]
    fn trailer_and_catalog_handles_pin_direct_values_and_resolve_references() {
        let mut document = Document::with_version("1.7");
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog",
            "Marker" => Object::Integer(42),
        }));
        document.trailer.set("Root", Object::Reference(catalog));
        document.trailer.set(
            "Info",
            Object::Dictionary(dictionary! { "Producer" => Object::string_literal("distillPDF") }),
        );
        let adapter = EagerDocumentAdapter::new(Arc::new(document), Arc::from(&b""[..]));

        let catalog = adapter.catalog().unwrap();
        let info = adapter.trailer_entry(b"Info").unwrap();
        drop(adapter);

        assert_eq!(
            catalog
                .read(|dict| dict.get(b"Marker").unwrap().as_i64().unwrap())
                .unwrap(),
            42
        );
        assert_eq!(
            info.dictionary_entry(&FaultFreeAccess, b"Producer")
                .unwrap()
                .read(|object| object.as_str().unwrap().to_vec())
                .unwrap(),
            b"distillPDF"
        );
    }

    struct FaultFreeAccess;

    impl DocumentAccess for FaultFreeAccess {
        fn object(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
            Err(AccessError::object(id, "unexpected reference"))
        }

        fn trailer_entry(&self, _key: &[u8]) -> Result<ObjectHandle, AccessError> {
            Err(AccessError::object((0, 0), "unexpected trailer read"))
        }

        fn page_content(&self, page: ObjectId) -> Result<PageContent, AccessError> {
            Err(AccessError::object(page, "unexpected page content read"))
        }

        fn pages(&self) -> Result<Vec<PageRef>, AccessError> {
            Ok(Vec::new())
        }

        fn fallback_page_text(&self, page: u32) -> Result<String, AccessError> {
            Err(AccessError::object(
                (page, 0),
                "unexpected fallback text read",
            ))
        }

        fn object_ids(&self) -> Vec<ObjectId> {
            Vec::new()
        }

        fn page_resource_chain(
            &self,
            _page: ObjectId,
        ) -> Result<Vec<DictionaryHandle>, AccessError> {
            Ok(Vec::new())
        }

        fn source_recovery(&self) -> Arc<SourceRecovery> {
            Arc::new(SourceRecovery::new(Arc::new(BytesSource::new(Arc::from(
                &b""[..],
            )))))
        }
    }

    #[test]
    fn page_handle_and_content_preserve_eager_missing_and_decode_behavior() {
        let mut document = Document::with_version("1.7");
        let content = document.add_object(lopdf::Stream::new(
            Dictionary::new(),
            b"BT (exact) Tj ET".to_vec(),
        ));
        let page = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Contents" => Object::Reference(content),
        }));
        let adapter = EagerDocumentAdapter::new(Arc::new(document), Arc::from(&b""[..]));

        assert!(adapter
            .page(page)
            .unwrap()
            .read(|dict| dict.has(b"Contents"))
            .unwrap());
        assert_eq!(adapter.page_content(page).unwrap(), b"BT (exact) Tj ET\n");
        assert!(adapter.page_content((999, 0)).is_err());
    }

    #[test]
    fn resource_chain_handles_pin_inline_page_and_indirect_parent_dictionaries() {
        let mut document = Document::with_version("1.7");
        let outer_resources = document.add_object(Object::Dictionary(dictionary! {
            "Outer" => Object::Integer(1),
        }));
        let pages = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(Vec::new()),
            "Count" => 1,
            "Resources" => Object::Reference(outer_resources),
        }));
        let page = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages),
            "Resources" => Object::Dictionary(dictionary! {
                "Inner" => Object::Integer(2),
            }),
        }));
        document
            .get_object_mut(pages)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Kids", Object::Array(vec![Object::Reference(page)]));
        let adapter = EagerDocumentAdapter::new(Arc::new(document), Arc::from(&b""[..]));
        let resources = adapter.page_resource_chain(page).unwrap();
        assert_eq!(resources.len(), 2);
        drop(adapter);
        assert!(resources[0].read(|dict| dict.has(b"Outer")).unwrap());
        assert!(resources[1].read(|dict| dict.has(b"Inner")).unwrap());
    }

    #[test]
    fn fault_access_injects_each_operation_and_counts_bounded_source_reads() {
        let (adapter, ids) = adapter(vec![Object::Integer(7)], b"abcdef");
        for point in [
            FaultPoint::Object,
            FaultPoint::Trailer,
            FaultPoint::PageContent,
            FaultPoint::Pages,
            FaultPoint::FallbackText,
            FaultPoint::Resources,
            FaultPoint::Source,
        ] {
            let counts = Arc::new(AccessCounts::default());
            counts.opens.fetch_add(1, Ordering::Relaxed);
            let fault = FaultAccess::new(Arc::new(adapter.clone()), Some(point), counts);
            let error = match point {
                FaultPoint::Object => fault.object(ids[0]).err().unwrap(),
                FaultPoint::Trailer => fault.trailer_entry(b"Root").err().unwrap(),
                FaultPoint::PageContent => fault.page_content(ids[0]).err().unwrap(),
                FaultPoint::Pages => fault.pages().err().unwrap(),
                FaultPoint::FallbackText => fault.fallback_page_text(1).err().unwrap(),
                FaultPoint::Resources => fault.page_resource_chain(ids[0]).err().unwrap(),
                FaultPoint::Source => {
                    let source_error = fault.test_source().read_range(0, 1, 1).err().unwrap();
                    assert!(matches!(source_error, SourceError::UnexpectedEof { .. }));
                    assert_eq!(fault.counts.source_reads.load(Ordering::Relaxed), 1);
                    continue;
                }
            };
            assert!(error.detail.contains("injected"));
            assert_eq!(error.phase, point.phase());
            assert_eq!(error.kind, AccessKind::Injected);
            assert!(legacy_suppression(error.phase).is_some());
            assert_eq!(fault.counts.opens.load(Ordering::Relaxed), 1);
        }

        let counts = Arc::new(AccessCounts::default());
        let catalog_fault = FaultAccess::new(
            Arc::new(adapter.clone()),
            Some(FaultPoint::Trailer),
            Arc::clone(&counts),
        );
        let catalog_error = catalog_fault.catalog().err().unwrap();
        assert_eq!(catalog_error.phase, AccessPhase::Catalog);
        assert_eq!(
            legacy_suppression(catalog_error.phase),
            Some(Suppression::SkipMetadata)
        );

        let counts = Arc::new(AccessCounts::default());
        let page_fault = FaultAccess::new(
            Arc::new(adapter.clone()),
            Some(FaultPoint::Object),
            Arc::clone(&counts),
        );
        let page_error = page_fault.page(ids[0]).err().unwrap();
        assert_eq!(page_error.phase, AccessPhase::Page);
        assert_eq!(
            legacy_suppression(page_error.phase),
            Some(Suppression::SkipNode)
        );

        let counts = Arc::new(AccessCounts::default());
        counts.opens.fetch_add(1, Ordering::Relaxed);
        let counted = FaultAccess::new(Arc::new(adapter), None, counts);
        assert_eq!(counted.object_ids(), ids);
        assert_eq!(counted.counts.object_lists.load(Ordering::Relaxed), 1);
        assert_eq!(counted.test_source().read_range(1, 3, 3).unwrap(), b"bcd");
        assert_eq!(counted.counts.source_requests.load(Ordering::Relaxed), 1);
        assert_eq!(counted.counts.source_reads.load(Ordering::Relaxed), 1);
        assert_eq!(counted.counts.max_request.load(Ordering::Relaxed), 3);
        assert_eq!(counted.materialize_source_bounded(6).unwrap(), b"abcdef");
        assert_eq!(counted.counts.source_reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn recovery_index_is_single_flight_chunk_bounded_and_shares_incremental_hash() {
        let mut raw = vec![b'x'; SOURCE_CHUNK_BYTES - 4];
        raw.extend_from_slice(b"42 0 obj\n<<>>\nstream\r\ncmap-data\r\nendstream\n");
        raw.extend(std::iter::repeat_n(b'z', SOURCE_CHUNK_BYTES + 17));
        let expected_hash = {
            let digest = Sha256::digest(&raw);
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let counts = Arc::new(AccessCounts::default());
        let bytes: Arc<[u8]> = Arc::from(raw.clone());
        let inner: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(bytes));
        let counted: Arc<dyn RandomAccessSource> = Arc::new(CountingSource {
            inner,
            counts: Arc::clone(&counts),
            fail_reads: false,
        });
        let recovery = Arc::new(SourceRecovery::new(counted));
        let barrier = Arc::new(std::sync::Barrier::new(12));
        let mut children = Vec::new();
        for index in 0..12 {
            let recovery = Arc::clone(&recovery);
            let barrier = Arc::clone(&barrier);
            let expected_hash = expected_hash.clone();
            children.push(std::thread::spawn(move || {
                barrier.wait();
                if index % 2 == 0 {
                    assert_eq!(recovery.sha256().unwrap(), expected_hash);
                } else {
                    let bytes = recovery.recover_stream(42).unwrap().unwrap();
                    assert_eq!(bytes.as_ref(), b"cmap-data");
                }
            }));
        }
        for child in children {
            child.join().unwrap();
        }
        let scan_reads = raw.len().div_ceil(SOURCE_CHUNK_BYTES) as u64;
        assert_eq!(
            counts.source_reads.load(Ordering::Relaxed),
            scan_reads + 6,
            "one source scan plus one bounded target read per recovery caller"
        );
        assert!(counts.max_request.load(Ordering::Relaxed) <= SOURCE_CHUNK_BYTES as u64);
        assert!(recovery.recover_stream(99).unwrap().is_none());
        assert_eq!(counts.source_reads.load(Ordering::Relaxed), scan_reads + 6);
    }

    fn legacy_recovery_range(raw: &[u8], object: u32) -> Option<CheckedSourceRange> {
        fn find(raw: &[u8], needle: &[u8], from: usize) -> Option<usize> {
            raw.get(from..)?
                .windows(needle.len())
                .position(|window| window == needle)
                .map(|position| position + from)
        }

        let marker = format!("{object} 0 obj");
        let object = find(raw, marker.as_bytes(), 0)?;
        let mut start = find(raw, b"stream", object)? + b"stream".len();
        if raw.get(start) == Some(&b'\r') {
            start += 1;
        }
        if raw.get(start) == Some(&b'\n') {
            start += 1;
        }
        let mut end = find(raw, b"endstream", start)?;
        if end > start && raw[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && raw[end - 1] == b'\r' {
            end -= 1;
        }
        (end > start).then_some(CheckedSourceRange {
            offset: start as u64,
            length: (end - start) as u64,
        })
    }

    fn scanned_range(raw: &[u8], object: u32) -> Result<Option<CheckedSourceRange>, SourceFailure> {
        let source = BytesSource::new(Arc::from(raw));
        Ok(scan_source_streams(&source)?.streams.get(&object).copied())
    }

    #[test]
    fn recovery_scanner_has_differential_legacy_authority_and_named_security_corrections() {
        let ordinary = [
            b"42 0 obj\n<<>>\nstream\nlf\nendstream".as_slice(),
            b"42 0 obj\n<<>>\nstream\r\ncrlf\r\nendstream".as_slice(),
            b"42 0 obj\n<<>>\nstream\rcr-only\rendstream".as_slice(),
            b"42 0 obj\n<<>>\nstreamno-eolendstream".as_slice(),
            b"42 0 obj\n<< /Note (stream) >>\nstream\nactual\nendstream".as_slice(),
            b"42 0 obj\n<<>>\nstream\nfirst\nendstream\n42 0 obj\n<<>>\nstream\nsecond\nendstream"
                .as_slice(),
            b"42 0 obj\n<<>>\nstream\nendstream".as_slice(),
        ];
        for raw in ordinary {
            assert_eq!(
                scanned_range(raw, 42).unwrap(),
                legacy_recovery_range(raw, 42)
            );
        }

        let mut split = vec![b'x'; SOURCE_CHUNK_BYTES - 3];
        split.extend_from_slice(b"42 0 obj\n<<>>\nstream\r\nsplit\r\nendstream");
        assert_eq!(
            scanned_range(&split, 42).unwrap(),
            legacy_recovery_range(&split, 42)
        );

        // Approved correction 1: a requested object number is not a suffix of a larger number.
        let suffix = b"142 0 obj\n<<>>\nstream\nwrong\nendstream";
        assert!(legacy_recovery_range(suffix, 42).is_some());
        assert!(scanned_range(suffix, 42).unwrap().is_none());

        // Approved correction 2: a streamless object cannot steal a later object's stream.
        let spill = b"42 0 obj\n<<>>\nendobj\n43 0 obj\n<<>>\nstream\nwrong\nendstream";
        assert!(legacy_recovery_range(spill, 42).is_some());
        assert!(scanned_range(spill, 42).unwrap().is_none());

        // Approved correction 3: a regular-character-prefixed substring is not the `stream`
        // keyword and cannot steal a later object's actual stream range.
        let regular_prefix = b"1 0 obj\n<< /Probe (malformed-stream) >>\nendobj\n\
            4 0 obj\n<<>>\nstream\npayload\nendstream";
        assert!(legacy_recovery_range(regular_prefix, 1).is_some());
        assert!(scanned_range(regular_prefix, 1).unwrap().is_none());
        assert_eq!(
            scanned_range(regular_prefix, 4).unwrap(),
            legacy_recovery_range(regular_prefix, 4)
        );

        // The frozen 64 MiB recovery cap refuses the range before allocating its payload.
        let mut oversize = b"42 0 obj\n<<>>\nstream\n".to_vec();
        oversize.resize(
            oversize.len() + MAX_RECOVERED_STREAM_BYTES as usize + 1,
            b'x',
        );
        oversize.extend_from_slice(b"\nendstream");
        let recovery = SourceRecovery::new(Arc::new(BytesSource::new(oversize.into())));
        let error = recovery
            .recover_stream(42)
            .err()
            .expect("oversize recovery must fail");
        assert_eq!(error.kind, SourceFailureKind::ResourceLimit);
    }


    #[test]
    fn source_changed_remains_typed_and_never_becomes_model_data() {
        struct ChangedSource;
        impl RandomAccessSource for ChangedSource {
            fn len(&self) -> SourceResult<u64> {
                Ok(1)
            }

            fn read_at(&self, _offset: u64, _out: &mut [u8]) -> SourceResult<usize> {
                Err(SourceError::SourceChanged)
            }

            fn validate_unchanged(&self) -> SourceResult<()> {
                Err(SourceError::SourceChanged)
            }
        }

        let recovery = SourceRecovery::new(Arc::new(ChangedSource));
        let first = recovery.sha256().unwrap_err();
        let second = recovery.sha256().unwrap_err();
        assert_eq!(first.kind, SourceFailureKind::SourceChanged);
        assert_eq!(second, first, "single-flight failure publication is stable");
        assert_eq!(AccessError::source(first).kind, AccessKind::SourceChanged);
    }

    /// A classic-xref PDF whose objects 4..=8 are `body[n]`, so a test can lay out an
    /// arbitrary reference topology without an `lopdf::Document` in the way.
    fn indexed_reference_fixture(bodies: &[(usize, &[u8])]) -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let size = bodies.iter().map(|(number, _)| *number).max().unwrap_or(3) + 1;
        let mut offsets = vec![0usize; size];
        let fixed: Vec<(usize, &[u8])> = vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".as_slice()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice()),
            (3, b"<< /Type /Page /Parent 2 0 R >>".as_slice()),
        ];
        for (number, body) in fixed.iter().chain(bodies.iter()) {
            offsets[*number] = pdf.len();
            pdf.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for offset in offsets.iter().skip(1) {
            if *offset == 0 {
                pdf.extend_from_slice(b"0000000000 65535 f \n");
            } else {
                pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
            }
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        pdf
    }

    #[test]
    fn indexed_reference_chains_resolve_and_cycles_stay_backend_errors() {
        let _test_lock = indexed_test_lock();
        // 4 -> 5 -> 6 = 42 (two hops), 7 -> 7 (self cycle), 8 -> 9 -> 8 (two-object cycle).
        let raw = indexed_reference_fixture(&[
            (4, b"5 0 R".as_slice()),
            (5, b"6 0 R".as_slice()),
            (6, b"42".as_slice()),
            (7, b"7 0 R".as_slice()),
            (8, b"9 0 R".as_slice()),
            (9, b"8 0 R".as_slice()),
        ]);
        let adapter = indexed(&raw, None);
        // The zero-hop and multi-hop paths agree with eager `get_object`.
        assert_eq!(
            adapter.object((6, 0)).unwrap().read(|o| o.as_i64().unwrap()).unwrap(),
            42
        );
        assert_eq!(
            adapter.object((4, 0)).unwrap().read(|o| o.as_i64().unwrap()).unwrap(),
            42
        );
        // The lazily seeded cycle set still names the object it caught and its class.
        for id in [(7u32, 0u16), (8, 0), (9, 0)] {
            let Err(error) = adapter.object(id) else {
                panic!("{id:?} resolved instead of reporting a cycle");
            };
            assert_eq!(error.kind, AccessKind::Backend, "{id:?}");
            assert!(
                error.to_string().contains("indirect reference cycle"),
                "{id:?}: {error}"
            );
        }
    }

    fn indexed_metadata_fixture() -> Vec<u8> {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = [0usize; 7];
        for (number, generation, body) in [
            (
                1usize,
                4u16,
                b"<< /Type /Catalog /Pages 2 0 R >>".as_slice(),
            ),
            (
                2,
                0,
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice(),
            ),
            (3, 0, b"<< /Type /Page /Parent 2 0 R >>".as_slice()),
            (5, 2, b"<< /Producer (indexed-adapter) >>".as_slice()),
            (6, 0, b"<< /Length 4 >>\nstream\nDATA\nendstream".as_slice()),
        ] {
            offsets[number] = pdf.len();
            pdf.extend_from_slice(format!("{number} {generation} obj\n").as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{:010} 00004 n \n", offsets[1]).as_bytes());
        pdf.extend_from_slice(format!("{:010} 00000 n \n", offsets[2]).as_bytes());
        pdf.extend_from_slice(format!("{:010} 00000 n \n", offsets[3]).as_bytes());
        pdf.extend_from_slice(b"0000000000 00007 f \n");
        pdf.extend_from_slice(format!("{:010} 00002 n \n", offsets[5]).as_bytes());
        pdf.extend_from_slice(format!("{:010} 00000 n \n", offsets[6]).as_bytes());
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 7 /Root 1 4 R /Info 5 2 R /Flag [true (owned)] >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }

    fn indexed(raw: &[u8], password: Option<&[u8]>) -> IndexedDocumentAdapter {
        let bytes: Arc<[u8]> = Arc::from(raw);
        let source: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(bytes));
        IndexedDocumentAdapter::open(source, password.map(Vec::from)).unwrap()
    }


    fn differential_document() -> (Document, Vec<u8>, ObjectId) {
        let mut document = Document::with_version("1.7");
        let cmap_conflict = document.add_object(lopdf::Stream::new(
            Dictionary::new(),
            b"1 begincodespacerange\n<00> <ff>\nendcodespacerange\n1 beginbfchar\n<41> <0058>\nendbfchar\n".to_vec(),
        ));
        let cmap_unicode = document.add_object(lopdf::Stream::new(
            Dictionary::new(),
            b"1 begincodespacerange\n<0000> <ffff>\nendcodespacerange\n1 beginbfchar\n<0001> <03a9>\nendbfchar\n".to_vec(),
        ));
        let encoding = Object::Dictionary(dictionary! {
            "Type" => "Encoding",
            "BaseEncoding" => "StandardEncoding",
            "Differences" => vec![Object::Integer(65), Object::Name(b"B".to_vec())],
        });
        let f1 = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => encoding,
            "ToUnicode" => Object::Reference(cmap_conflict),
        }));
        let f2 = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "Unicode",
            "ToUnicode" => Object::Reference(cmap_unicode),
        }));
        let resources = |level: i64| {
            Object::Dictionary(dictionary! {
                "Level" => level,
                "Font" => Object::Dictionary(dictionary! {
                    "F1" => Object::Reference(f1),
                    "F2" => Object::Reference(f2),
                }),
            })
        };
        let outer_resources = document.add_object(resources(1));
        let middle_resources = document.add_object(resources(2));
        let page_resources = document.add_object(resources(3));
        let first = document.add_object(lopdf::Stream::new(
            Dictionary::new(),
            b"BT /F1 12 Tf (A) Tj T* (A) Tj ET\n".to_vec(),
        ));
        let second = document.add_object(lopdf::Stream::new(
            Dictionary::new(),
            b"BT /F1 12 Tf (A) ' 0 0 (A) \" [(A) -200 (A)] TJ ET\nBT /F2 12 Tf <0001> Tj ET\n"
                .to_vec(),
        ));
        let root_pages = document.new_object_id();
        let middle_pages = document.new_object_id();
        let page = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(middle_pages),
            "Resources" => Object::Reference(page_resources),
            "Contents" => vec![Object::Reference(first), Object::Reference(second)],
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }));
        document.objects.insert(
            middle_pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Parent" => Object::Reference(root_pages),
                "Resources" => Object::Reference(middle_resources),
                "Kids" => vec![Object::Reference(page)],
                "Count" => 1,
            }),
        );
        document.objects.insert(
            root_pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Resources" => Object::Reference(outer_resources),
                "Kids" => vec![Object::Reference(middle_pages)],
                "Count" => 1,
            }),
        );
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(root_pages),
        }));
        document.trailer.set("Root", Object::Reference(catalog));
        let mut raw = Vec::new();
        document.save_to(&mut raw).unwrap();
        (document, raw, page)
    }

    fn document_with_page_values(
        page_resources: Object,
        ancestor_resources: Object,
        contents: Object,
    ) -> (Document, Vec<u8>, ObjectId) {
        let mut document = Document::with_version("1.7");
        let pages = document.new_object_id();
        let page = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages),
            "Resources" => page_resources,
            "Contents" => contents,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }));
        document.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Resources" => ancestor_resources,
                "Kids" => vec![Object::Reference(page)],
                "Count" => 1,
            }),
        );
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages),
        }));
        document.trailer.set("Root", Object::Reference(catalog));
        let mut raw = Vec::new();
        document.save_to(&mut raw).unwrap();
        (document, raw, page)
    }

    fn cmap(body: &str) -> Vec<u8> {
        format!(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CMapName /Exact def\n/CMapType 2 def\n4 begincodespacerange\n<00> <FF>\n<0000> <FFFF>\n<000000> <FFFFFF>\n<00000000> <FFFFFFFF>\nendcodespacerange\n{body}\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n"
        )
        .into_bytes()
    }

    fn flate_zeros(target: usize) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        let block = [0u8; 64 * 1024];
        let mut remaining = target;
        while remaining > 0 {
            let take = remaining.min(block.len());
            encoder.write_all(&block[..take]).unwrap();
            remaining -= take;
        }
        encoder.finish().unwrap()
    }

    fn finish_text_document(
        mut document: Document,
        fonts: Vec<(&str, ObjectId)>,
        content: Vec<u8>,
    ) -> (Document, Vec<u8>, ObjectId) {
        let font_dict = Object::Dictionary(Dictionary::from_iter(
            fonts
                .into_iter()
                .map(|(name, id)| (name.as_bytes().to_vec(), Object::Reference(id))),
        ));
        let content = document.add_object(lopdf::Stream::new(Dictionary::new(), content));
        let pages = document.new_object_id();
        let page = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages),
            "Resources" => Object::Dictionary(dictionary! { "Font" => font_dict }),
            "Contents" => Object::Reference(content),
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }));
        document.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1,
            }),
        );
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages),
        }));
        document.trailer.set("Root", Object::Reference(catalog));
        let mut raw = Vec::new();
        document.save_to(&mut raw).unwrap();
        (document, raw, page)
    }

    fn fallback_pair(
        document: Document,
        raw: &[u8],
    ) -> (Result<String, AccessError>, Result<String, AccessError>) {
        let eager = EagerDocumentAdapter::new(Arc::new(document), Arc::from(raw));
        let lazy = indexed(raw, None);
        (eager.fallback_page_text(1), lazy.fallback_page_text(1))
    }

    #[test]
    fn indexed_adapter_bounds_objects_trailer_pages_and_index_formula() {
        let _test_lock = indexed_test_lock();
        let raw = indexed_metadata_fixture();
        let adapter = indexed(&raw, None);
        assert_eq!(
            adapter.object_ids(),
            vec![(1, 4), (2, 0), (3, 0), (5, 2), (6, 0)]
        );
        assert_eq!(
            adapter.pages().unwrap(),
            vec![PageRef {
                number: 1,
                id: (3, 0)
            }]
        );
        assert_eq!(
            adapter.counters().page_map_builds.load(Ordering::Relaxed),
            1
        );
        let held = adapter.object((6, 0)).unwrap();
        assert_eq!(
            adapter
                .trailer_entry(b"Info")
                .unwrap()
                .read(|object| object
                    .as_dict()
                    .unwrap()
                    .get(b"Producer")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_vec())
                .unwrap(),
            b"indexed-adapter"
        );
        let flag = adapter.trailer_entry(b"Flag").unwrap();
        assert_eq!(
            flag.read(|object| object.as_array().unwrap().len())
                .unwrap(),
            2
        );
        // A live handle no longer withholds anything from any other operation: source recovery
        // and a second resolution of the same object both succeed while `held` is alive.
        assert_eq!(
            adapter.recover_source_stream(6).unwrap().unwrap().as_ref(),
            b"DATA"
        );
        let concurrent = adapter.object((6, 0)).unwrap();
        assert_eq!(
            concurrent
                .read(|object| object.as_stream().unwrap().content.clone())
                .unwrap(),
            b"DATA"
        );
        drop(concurrent);
        drop(held);
        drop(flag);
        assert!(adapter.object((5, 1)).is_err());
        let counters = adapter.counters();
        let cap = INDEX_FIXED_BYTES
            + INDEX_OBJECT_BYTES * counters.index_objects.load(Ordering::Relaxed)
            + INDEX_PAGE_BYTES * counters.index_pages.load(Ordering::Relaxed);
        assert!(counters.index_estimated_bytes.load(Ordering::Relaxed) <= cap);
    }



    #[test]
    fn indexed_content_resources_and_fallback_are_eager_differential() {
        let _test_lock = indexed_test_lock();
        let (document, raw, page) = differential_document();
        let eager = EagerDocumentAdapter::new(Arc::new(document), Arc::from(raw.as_slice()));
        let lazy = indexed(&raw, None);

        let eager_content = eager.page_content(page).unwrap();
        let lazy_content = lazy.page_content(page).unwrap();
        assert_eq!(lazy_content.as_ref(), eager_content.as_ref());
        drop(lazy_content);
        let levels = |access: &dyn DocumentAccess| {
            access
                .page_resource_chain(page)
                .unwrap()
                .into_iter()
                .map(|resource| {
                    resource
                        .read(|dictionary| dictionary.get(b"Level").unwrap().as_i64().unwrap())
                        .unwrap()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(levels(&eager), vec![1, 2, 3]);
        assert_eq!(levels(&lazy), levels(&eager));
        let eager_fallback = eager.fallback_page_text(1).unwrap();
        let lazy_fallback = lazy.fallback_page_text(1).unwrap();
        assert_eq!(lazy_fallback, eager_fallback);
        assert!(
            lazy_fallback.contains('B'),
            "Encoding differences must beat conflicting ToUnicode"
        );
    }

    #[test]
    fn indexed_fallback_uses_exact_mixed_width_cmap_authority() {
        let _test_lock = indexed_test_lock();
        let mut document = Document::with_version("1.7");
        let to_unicode = document.add_object(lopdf::Stream::new(
            Dictionary::new(),
            cmap(
                "5 beginbfchar\n<01> <0041>\n<0203> <0042>\n<040506> <0043>\n<0708090A> <D83DDE00>\n<0B> <0044>\nendbfchar",
            ),
        ));
        let font = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "ToUnicode" => Object::Reference(to_unicode),
        }));
        let (document, raw, _) = finish_text_document(
            document,
            vec![("F1", font)],
            b"BT /F1 12 Tf <0102030405060708090AFFFFFFFF0C> Tj ET".to_vec(),
        );
        let (eager, lazy) = fallback_pair(document, &raw);
        assert_eq!(lazy.as_ref().unwrap(), eager.as_ref().unwrap());
        assert_eq!(lazy.unwrap(), "ABC😀��\n");
    }

    #[test]
    fn indexed_fallback_discriminator_does_not_reuse_primary_fontinfo() {
        let _test_lock = indexed_test_lock();
        let mut document = Document::with_version("1.7");
        let to_unicode = document.add_object(lopdf::Stream::new(
            Dictionary::new(),
            cmap("2 beginbfchar\n<0102> <00410042>\n<03> <0043>\nendbfchar"),
        ));
        let font = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "ToUnicode" => Object::Reference(to_unicode),
        }));
        let (document, raw, page) = finish_text_document(
            document,
            vec![("F1", font)],
            b"BT /F1 12 Tf 1 0 0 1 10 10 Tm <010203> Tj ET".to_vec(),
        );
        let eager_access =
            EagerDocumentAdapter::new(Arc::new(document.clone()), Arc::from(raw.as_slice()));
        assert_eq!(
            crate::text::extract_page(&eager_access, page)
                .unwrap()
                .trim(),
            "C"
        );
        let (eager, lazy) = fallback_pair(document, &raw);
        assert_eq!(lazy.as_ref().unwrap(), eager.as_ref().unwrap());
        assert_eq!(lazy.unwrap(), "ABC\n");
    }

    #[test]
    fn indexed_fallback_named_tables_and_utf16_match_eager() {
        let _test_lock = indexed_test_lock();
        for (name, bytes) in [
            ("MacExpertEncoding", vec![0x21, 0x22, 0x23]),
            ("PDFDocEncoding", vec![0x80, 0x81, 0x82]),
            ("UniGB-UTF16-H", vec![0x00, 0x41, 0xD8, 0x00, 0x00]),
        ] {
            let mut document = Document::with_version("1.7");
            let font = document.add_object(Object::Dictionary(dictionary! {
                "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
                "Encoding" => name,
            }));
            let mut content = b"BT /F1 12 Tf <".to_vec();
            for byte in bytes {
                content.extend_from_slice(format!("{byte:02X}").as_bytes());
            }
            content.extend_from_slice(b"> Tj ET");
            let (document, raw, _) = finish_text_document(document, vec![("F1", font)], content);
            let (eager, lazy) = fallback_pair(document, &raw);
            assert_eq!(lazy.as_ref().unwrap(), eager.as_ref().unwrap(), "{name}");
        }

        let mut document = Document::with_version("1.7");
        let font = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => "UnknownEncoding",
        }));
        let (document, raw, _) = finish_text_document(
            document,
            vec![("F1", font)],
            b"BT /F1 12 Tf (A) Tj ET".to_vec(),
        );
        let (eager, lazy) = fallback_pair(document, &raw);
        assert!(eager.is_err());
        assert!(lazy.is_err());
    }

    #[test]
    fn indexed_fallback_differences_strictness_matches_eager() {
        let _test_lock = indexed_test_lock();
        let cases = vec![
            (
                Object::Dictionary(dictionary! {
                    "Type" => "Encoding", "BaseEncoding" => "StandardEncoding",
                    "Differences" => vec![Object::Integer(65), Object::Name(b"B".to_vec())],
                }),
                vec![b'A'],
                "B\n",
            ),
            (
                Object::Dictionary(dictionary! {
                    "Type" => "Encoding", "BaseEncoding" => "StandardEncoding",
                    "Differences" => vec![Object::Integer(65), Object::Name(b"NotAGlyph".to_vec())],
                }),
                vec![b'A'],
                "A\n",
            ),
            (
                Object::Dictionary(dictionary! {
                    "Type" => "Encoding", "Differences" => vec![Object::Integer(-1)],
                }),
                vec![b'A'],
                "A\n",
            ),
            (
                Object::Dictionary(dictionary! {
                    "Type" => "Encoding", "Differences" => vec![Object::Integer(256)],
                }),
                vec![b'A'],
                "A\n",
            ),
            (
                Object::Dictionary(dictionary! {
                    "Type" => "Encoding", "Differences" => vec![Object::Boolean(true)],
                }),
                vec![b'A'],
                "A\n",
            ),
            (
                Object::Dictionary(dictionary! {
                    "Type" => "Encoding", "BaseEncoding" => "WinAnsiEncoding",
                }),
                vec![0x80],
                "\n",
            ),
        ];
        for (encoding, bytes, expected) in cases {
            let mut document = Document::with_version("1.7");
            let font = document.add_object(Object::Dictionary(dictionary! {
                "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
                "Encoding" => encoding,
            }));
            let mut content = b"BT /F1 12 Tf <".to_vec();
            for byte in bytes {
                content.extend_from_slice(format!("{byte:02X}").as_bytes());
            }
            content.extend_from_slice(b"> Tj ET");
            let (document, raw, _) = finish_text_document(document, vec![("F1", font)], content);
            let (eager, lazy) = fallback_pair(document, &raw);
            assert_eq!(lazy.as_ref().unwrap(), eager.as_ref().unwrap());
            assert_eq!(lazy.unwrap(), expected);
        }
    }

    #[test]
    fn indexed_fallback_encoding_cycle_is_lenient_standard() {
        let _test_lock = indexed_test_lock();
        let mut document = Document::with_version("1.7");
        let first = document.new_object_id();
        let second = document.new_object_id();
        document.objects.insert(first, Object::Reference(second));
        document.objects.insert(second, Object::Reference(first));
        let font = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => Object::Reference(first),
        }));
        let (document, raw, _) = finish_text_document(
            document,
            vec![("F1", font)],
            b"BT /F1 12 Tf (A) Tj ET".to_vec(),
        );
        let (eager, lazy) = fallback_pair(document, &raw);
        assert_eq!(lazy.as_ref().unwrap(), eager.as_ref().unwrap());
        assert_eq!(lazy.unwrap(), "A\n");
    }

    #[test]
    fn indexed_fallback_operator_state_matches_eager() {
        let _test_lock = indexed_test_lock();
        let mut document = Document::with_version("1.7");
        let font = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            "Encoding" => "StandardEncoding",
        }));
        let content = b"BT /F1 12 Tf [(A) [(B) -101 (C)] -100 -200.0 (D)] TJ /F1 9 Tf (E) ' 0 0 (F) \" T* /Missing 8 Tf (X) Tj ET".to_vec();
        let (document, raw, _) = finish_text_document(document, vec![("F1", font)], content);
        let (eager, lazy) = fallback_pair(document, &raw);
        assert_eq!(lazy.as_ref().unwrap(), eager.as_ref().unwrap());
    }

    #[test]
    fn indexed_fallback_tounicode_precedence_and_leniency_match_eager() {
        let _test_lock = indexed_test_lock();
        enum ToUnicodeCase {
            Stream(Vec<u8>),
            NonStream,
            Missing,
        }
        let cases = [
            (
                None,
                ToUnicodeCase::Stream(cmap("1 beginbfchar\n<41> <0058>\nendbfchar")),
                "X\n",
            ),
            (None, ToUnicodeCase::Stream(b"not a cmap".to_vec()), "A\n"),
            (None, ToUnicodeCase::NonStream, "A\n"),
            (Some("Identity-H"), ToUnicodeCase::Missing, "A\n"),
            (
                Some("Identity-H"),
                ToUnicodeCase::Stream(cmap("1 beginbfchar\n<41> <0059>\nendbfchar")),
                "Y\n",
            ),
            (
                Some("WinAnsiEncoding"),
                ToUnicodeCase::Stream(cmap("1 beginbfchar\n<41> <005A>\nendbfchar")),
                "A\n",
            ),
        ];
        for (encoding, to_unicode, expected) in cases {
            let mut document = Document::with_version("1.7");
            let to_unicode = match to_unicode {
                ToUnicodeCase::Stream(bytes) => {
                    Some(document.add_object(lopdf::Stream::new(Dictionary::new(), bytes)))
                }
                ToUnicodeCase::NonStream => Some(document.add_object(Object::Integer(7))),
                ToUnicodeCase::Missing => None,
            };
            let mut font = dictionary! {
                "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
            };
            if let Some(encoding) = encoding {
                font.set("Encoding", encoding);
            }
            if let Some(to_unicode) = to_unicode {
                font.set("ToUnicode", Object::Reference(to_unicode));
            }
            let font = document.add_object(Object::Dictionary(font));
            let (document, raw, _) = finish_text_document(
                document,
                vec![("F1", font)],
                b"BT /F1 12 Tf (A) Tj ET".to_vec(),
            );
            let (eager, lazy) = fallback_pair(document, &raw);
            assert_eq!(lazy.as_ref().unwrap(), eager.as_ref().unwrap());
            assert_eq!(lazy.unwrap(), expected);
        }
    }

    #[test]
    fn fallback_resource_cycle_and_fault_are_checked_not_suppressed() {
        let _test_lock = indexed_test_lock();
        let mut document = Document::with_version("1.7");
        let font = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        }));
        let (mut document, _raw, page) = finish_text_document(
            document,
            vec![("F1", font)],
            b"BT /F1 12 Tf (A) Tj ET".to_vec(),
        );
        document
            .get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Parent", Object::Reference(page));
        let mut raw = Vec::new();
        document.save_to(&mut raw).unwrap();
        let eager = EagerDocumentAdapter::new(Arc::new(document), Arc::from(raw.as_slice()));
        let eager_error = crate::text::fallback_page_text(&eager, page).unwrap_err();
        let indexed_adapter = indexed(&raw, None);
        let indexed_error = crate::text::fallback_page_text(&indexed_adapter, page).unwrap_err();
        assert_eq!(indexed_error.kind, eager_error.kind);

        let (_, stable_raw, stable_page) = differential_document();
        let stable: Arc<dyn DocumentAccess> = Arc::new(indexed(&stable_raw, None));
        let fault = FaultAccess::new(
            stable,
            Some(FaultPoint::Resources),
            Arc::new(AccessCounts::default()),
        );
        let error = crate::text::fallback_page_text(&fault, stable_page).unwrap_err();
        assert_eq!(error.phase, AccessPhase::Resources);
    }

    #[test]
    fn indexed_tounicode_payload_is_exact_and_bounded_by_its_declared_ceiling() {
        let _test_lock = indexed_test_lock();
        const MIB: usize = 1024 * 1024;
        let make = |target: usize| {
            let mut document = Document::with_version("1.7");
            let mut stream_dict = Dictionary::new();
            stream_dict.set("Filter", "FlateDecode");
            let stream = document.add_object(lopdf::Stream::new(stream_dict, flate_zeros(target)));
            let font = document.add_object(Object::Dictionary(dictionary! {
                "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
                "ToUnicode" => Object::Reference(stream),
            }));
            let (document, raw, _) = finish_text_document(
                document,
                vec![("F1", font)],
                b"BT /F1 12 Tf (A) Tj ET".to_vec(),
            );
            (document, raw, stream)
        };

        let (_, exact_raw, exact_stream) = make(60 * MIB);
        let exact = indexed(&exact_raw, None);
        let stream = exact.object(exact_stream).unwrap();
        let payload = stream.decoded_stream_bytes(64 * MIB).unwrap();
        assert_eq!(payload.len(), 60 * MIB);
        // Two decodes of the same handle succeed independently: nothing is charged to a shared
        // allowance that the first decode could exhaust for the second.
        assert_eq!(stream.decoded_stream_bytes(64 * MIB).unwrap().len(), 60 * MIB);

        let (_, over_raw, over_stream) = make(64 * MIB + 1);
        let over = indexed(&over_raw, None);
        let fallback_error = over.fallback_page_text(1).unwrap_err();
        assert_eq!(fallback_error.kind, AccessKind::ResourceLimit);
        let error = over
            .object(over_stream)
            .unwrap()
            .decoded_stream_bytes(64 * MIB)
            .err()
            .unwrap();
        assert_eq!(error.kind, AccessKind::ResourceLimit);
    }

    #[test]
    fn indexed_fallback_propagates_source_changed() {
        struct SwitchSource {
            bytes: Arc<[u8]>,
            changed: Arc<std::sync::atomic::AtomicBool>,
        }
        impl RandomAccessSource for SwitchSource {
            fn len(&self) -> SourceResult<u64> {
                Ok(self.bytes.len() as u64)
            }
            fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
                if self.changed.load(Ordering::Acquire) {
                    return Err(SourceError::SourceChanged);
                }
                let start =
                    usize::try_from(offset).map_err(|_| SourceError::PlatformLimitExceeded {
                        requested: offset,
                        limit: usize::MAX as u64,
                    })?;
                if start >= self.bytes.len() {
                    return Ok(0);
                }
                let take = out.len().min(self.bytes.len() - start);
                out[..take].copy_from_slice(&self.bytes[start..start + take]);
                Ok(take)
            }
            fn validate_unchanged(&self) -> SourceResult<()> {
                if self.changed.load(Ordering::Acquire) {
                    Err(SourceError::SourceChanged)
                } else {
                    Ok(())
                }
            }
        }

        let _test_lock = indexed_test_lock();
        let (document, raw, _) = differential_document();
        drop(document);
        let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let source: Arc<dyn RandomAccessSource> = Arc::new(SwitchSource {
            bytes: Arc::from(raw),
            changed: Arc::clone(&changed),
        });
        let adapter = IndexedDocumentAdapter::open(source, None).unwrap();
        changed.store(true, Ordering::Release);
        let error = adapter.fallback_page_text(1).unwrap_err();
        assert_eq!(error.kind, AccessKind::SourceChanged);
    }

    #[test]
    fn indexed_post_open_short_custom_and_io_reads_are_fatal_source_io() {
        struct FailingSource {
            bytes: Arc<[u8]>,
            mode: Arc<std::sync::atomic::AtomicU8>,
        }
        impl RandomAccessSource for FailingSource {
            fn len(&self) -> SourceResult<u64> {
                Ok(self.bytes.len() as u64)
            }
            fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
                match self.mode.load(Ordering::Acquire) {
                    1 => return Ok(0),
                    2 => return Ok(out.len().saturating_add(1)),
                    3 => {
                        return Err(SourceError::Io(std::io::Error::other(
                            "injected positioned read failure",
                        )));
                    }
                    _ => {}
                }
                let start =
                    usize::try_from(offset).map_err(|_| SourceError::PlatformLimitExceeded {
                        requested: offset,
                        limit: usize::MAX as u64,
                    })?;
                if start >= self.bytes.len() {
                    return Ok(0);
                }
                let take = out.len().min(self.bytes.len() - start);
                out[..take].copy_from_slice(&self.bytes[start..start + take]);
                Ok(take)
            }
        }

        let _test_lock = indexed_test_lock();
        let (_, raw, _) = differential_document();
        for mode_value in 1..=3 {
            let mode = Arc::new(std::sync::atomic::AtomicU8::new(0));
            let source: Arc<dyn RandomAccessSource> = Arc::new(FailingSource {
                bytes: Arc::from(raw.clone()),
                mode: Arc::clone(&mode),
            });
            let adapter = IndexedDocumentAdapter::open(source, None).unwrap();
            mode.store(mode_value, Ordering::Release);
            let error = adapter.source_sha256().unwrap_err();
            assert_eq!(error.kind, AccessKind::SourceIo, "mode {mode_value}");
            assert!(fatal_lazy_access(&error));
        }
    }

    #[test]
    fn indexed_parent_resolution_errors_are_resources_phased() {
        let _test_lock = indexed_test_lock();
        let mut document = Document::with_version("1.7");
        let page = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference((999, 0)),
        }));
        let pages = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Pages", "Count" => 1, "Kids" => vec![Object::Reference(page)],
        }));
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages),
        }));
        document.trailer.set("Root", Object::Reference(catalog));
        let mut raw = Vec::new();
        document.save_to(&mut raw).unwrap();
        let adapter = indexed(&raw, None);
        let error = adapter
            .page_resource_chain(page)
            .err()
            .expect("dangling parent must fail");
        assert_eq!(error.phase, AccessPhase::Resources);
        assert_eq!(error.object, (999, 0));
    }

    #[test]
    fn indexed_page_content_invalid_filter_malformed_and_bomb_are_bounded() {
        fn page_pdf(filter: &[u8], content: Vec<u8>) -> (Vec<u8>, ObjectId) {
            let mut document = Document::with_version("1.7");
            let mut stream_dictionary = Dictionary::new();
            stream_dictionary.set("Filter", Object::Name(filter.to_vec()));
            let content = document.add_object(lopdf::Stream::new(stream_dictionary, content));
            let pages = document.new_object_id();
            let page = document.add_object(Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => Object::Reference(pages),
                "Contents" => Object::Reference(content),
            }));
            document.objects.insert(
                pages,
                Object::Dictionary(dictionary! {
                    "Type" => "Pages", "Count" => 1, "Kids" => vec![Object::Reference(page)],
                }),
            );
            let catalog = document.add_object(Object::Dictionary(dictionary! {
                "Type" => "Catalog", "Pages" => Object::Reference(pages),
            }));
            document.trailer.set("Root", Object::Reference(catalog));
            let mut raw = Vec::new();
            document.save_to(&mut raw).unwrap();
            (raw, page)
        }

        let _test_lock = indexed_test_lock();
        for (filter, content, expected) in [
            (&b"NoSuchFilter"[..], &b"RAW"[..], &b"RAW\n"[..]),
            // Lopdf's legacy Flate decoder accepts the malformed prefix as empty partial output.
            (&b"FlateDecode"[..], &b"not-zlib"[..], &b"\n"[..]),
        ] {
            let (raw, page) = page_pdf(filter, content.to_vec());
            assert_eq!(
                indexed(&raw, None).page_content(page).unwrap().as_ref(),
                expected
            );
        }

        let (raw, page) = page_pdf(b"FlateDecode", flate_zeros(64 * 1024 * 1024));
        let error = indexed(&raw, None)
            .page_content(page)
            .expect_err("a decoded content bomb must exceed the page ceiling");
        assert_eq!(error.phase, AccessPhase::PageContent);
        assert_eq!(error.kind, AccessKind::ResourceLimit);
    }

    #[test]
    fn indexed_fallback_page_content_error_overrides_sorted_font_errors() {
        let _test_lock = indexed_test_lock();
        let mut document = Document::with_version("1.7");
        let zed = document.add_object(Object::Dictionary(dictionary! { "Type" => "WrongZ" }));
        let alpha = document.add_object(Object::Dictionary(dictionary! { "Type" => "WrongA" }));
        let (_document, raw, page) = finish_text_document(
            document,
            vec![("ZZ", zed), ("AA", alpha)],
            b"BT ET".to_vec(),
        );
        let lazy: Arc<dyn DocumentAccess> = Arc::new(indexed(&raw, None));
        let sorted_error = lazy.fallback_page_text(1).unwrap_err();
        assert_eq!(
            sorted_error.object, alpha,
            "AA must win BTree encoding order"
        );
        let fault = FaultAccess::new(
            lazy,
            Some(FaultPoint::PageContent),
            Arc::new(AccessCounts::default()),
        );
        let content_error = crate::text::fallback_page_text(&fault, page).unwrap_err();
        assert_eq!(content_error.object, page);
        assert_eq!(content_error.phase, AccessPhase::PageContent);
    }

    #[test]
    fn indexed_resources_ignore_direct_ancestor_like_eager() {
        let _test_lock = indexed_test_lock();
        let (document, raw, page) = document_with_page_values(
            Object::Dictionary(dictionary! { "Level" => 3 }),
            Object::Dictionary(dictionary! { "Level" => 1 }),
            Object::Null,
        );
        let eager = EagerDocumentAdapter::new(Arc::new(document), Arc::from(raw.as_slice()));
        let lazy = indexed(&raw, None);
        let levels = |access: &dyn DocumentAccess| {
            access
                .page_resource_chain(page)
                .unwrap()
                .into_iter()
                .map(|resource| {
                    resource
                        .read(|dict| dict.get(b"Level").unwrap().as_i64().unwrap())
                        .unwrap()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(levels(&eager), vec![3]);
        assert_eq!(levels(&lazy), levels(&eager));
    }

    #[test]
    fn direct_inline_page_stream_is_eager_empty_and_not_indexable_pdf_syntax() {
        let _test_lock = indexed_test_lock();
        let (document, raw, page) = document_with_page_values(
            Object::Dictionary(Dictionary::new()),
            Object::Dictionary(Dictionary::new()),
            Object::Stream(lopdf::Stream::new(
                Dictionary::new(),
                b"BT (hidden) Tj ET".to_vec(),
            )),
        );
        let eager = EagerDocumentAdapter::new(Arc::new(document), Arc::from(raw.as_slice()));
        assert!(eager.page_content(page).unwrap().is_empty());
        let source: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(Arc::from(raw)));
        assert!(IndexedDocumentAdapter::open(source, None).is_err());
    }

    #[test]
    fn indexed_adapter_resolves_compressed_trailer_reference_with_bounded_owner() {
        let _test_lock = indexed_test_lock();
        let mut document = Document::with_version("1.7");
        let pages = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0,
        }));
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages),
        }));
        let info = document.add_object(Object::Dictionary(
            dictionary! { "Producer" => "compressed" },
        ));
        document.trailer.set("Root", Object::Reference(catalog));
        document.trailer.set("Info", Object::Reference(info));
        let options = lopdf::SaveOptions::builder()
            .use_object_streams(true)
            .use_xref_streams(true)
            .build();
        let mut raw = Vec::new();
        document.save_with_options(&mut raw, options).unwrap();
        let adapter = indexed(&raw, None);
        assert_eq!(
            adapter
                .trailer_entry(b"Info")
                .unwrap()
                .read(|object| object
                    .as_dict()
                    .unwrap()
                    .get(b"Producer")
                    .unwrap()
                    .as_name()
                    .unwrap()
                    .to_vec())
                .unwrap(),
            b"compressed"
        );
    }

    #[test]
    fn indexed_encryption_preserves_empty_and_explicit_password_contracts() {
        let _test_lock = indexed_test_lock();
        let fixture = |name: &str| {
            std::fs::read(format!(
                "{}/../tests/fixtures_pdf/encrypted/{name}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap()
        };
        for name in ["rc4_40.pdf", "inline_encrypt_rc4_128.pdf"] {
            let raw = fixture(name);
            assert!(
                IndexedDocumentAdapter::open(Arc::new(BytesSource::new(Arc::from(raw))), None,)
                    .is_ok(),
                "{name}"
            );
        }
        for name in ["userpw.pdf", "inline_encrypt_userpw.pdf"] {
            let raw = fixture(name);
            let source: Arc<dyn RandomAccessSource> =
                Arc::new(BytesSource::new(Arc::from(raw.clone())));
            let error = IndexedDocumentAdapter::open(source, None)
                .err()
                .expect("password required");
            assert_eq!(error.kind, AccessKind::PasswordRequired);
            let source: Arc<dyn RandomAccessSource> =
                Arc::new(BytesSource::new(Arc::from(raw.clone())));
            let error = IndexedDocumentAdapter::open(source, Some(b"wrong".to_vec()))
                .err()
                .expect("wrong password rejected");
            assert_eq!(error.kind, AccessKind::InvalidPassword);
            let source: Arc<dyn RandomAccessSource> = Arc::new(BytesSource::new(Arc::from(raw)));
            assert!(IndexedDocumentAdapter::open(source, Some(b"secret".to_vec())).is_ok());
        }
    }


    #[test]
    fn indexed_route_resolves_through_the_fork_and_never_the_eager_document() {
        let source = include_str!("access.rs");
        let start = source
            .find("pub(crate) struct IndexedDocumentAdapter")
            .unwrap();
        let end = source[start..]
            .find("#[cfg(test)]\npub(crate) mod tests")
            .map(|offset| start + offset)
            .unwrap();
        let indexed = &source[start..end];
        // The lazy route owns no eager document and mints no process-wide allowance: every
        // object comes from the reader's own shared resolver under its per-reader caches.
        assert!(indexed.contains("resolve_object_shared"));
        assert!(indexed.contains("open_shared_cached"));
        assert!(indexed.contains("trailer_entry_raw_owned"));
        assert!(!indexed.contains("Document::"));
        assert!(!indexed.contains("load_mem"));
        assert!(!indexed.contains("provisional_o_budget"));
        assert!(!indexed.contains("ScalarResolutionPermit"));
    }

    /// Cache sizing is a pure function of the document, so two opens of the same bytes — and two
    /// documents open at once — configure exactly the same bounded caches.
    #[test]
    fn indexed_cache_options_depend_only_on_source_length() {
        let small = indexed_cache_options(1024);
        assert_eq!(small.max_bytes(), INDEXED_CACHE_MIN_BYTES);
        assert_eq!(small.max_entries(), INDEXED_CACHE_MIN_ENTRIES);
        let huge = indexed_cache_options(u64::MAX);
        assert_eq!(huge.max_bytes(), INDEXED_CACHE_MAX_BYTES);
        assert_eq!(huge.max_entries(), INDEXED_CACHE_MAX_ENTRIES);
        let mid = indexed_cache_options(256 * 1024 * 1024);
        assert_eq!(mid.max_bytes(), 32 * 1024 * 1024);
        assert_eq!(mid.max_bytes(), indexed_cache_options(256 * 1024 * 1024).max_bytes());
    }
}
