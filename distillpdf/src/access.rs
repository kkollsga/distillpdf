//! Owned PDF object/source access boundary.
//!
//! Extraction code must not retain a borrow into lopdf's eager `Document`: L3 replaces the
//! backend with on-demand owned resolution, where no such document-wide borrow exists.  Short
//! reads therefore happen through [`ObjectHandle::read`], while values that escape a read are
//! explicitly owned.  The eager implementation remains the compatibility oracle through L9.

use lopdf::{
    BytesSource, Dictionary, Document, Object, ObjectId, RandomAccessSource, SourceError,
    SourceResult,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

const SOURCE_CHUNK_BYTES: usize = 64 * 1024;
const MAX_RECOVERED_STREAM_BYTES: u64 = 64 * 1024 * 1024;
const RECOVERY_INDEX_ENTRY_BYTES: u64 = 128;
const MAX_RECOVERY_INDEX_ENTRIES: usize = 65_536;
const RECOVERY_METADATA_BUDGET_BYTES: u64 = 128 * 1024 * 1024;
const RECOVERY_PAYLOAD_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceFailureKind {
    SourceChanged,
    Bounds,
    ResourceLimit,
    Backend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceFailure {
    kind: SourceFailureKind,
    detail: Arc<str>,
}

impl SourceFailure {
    fn new(kind: SourceFailureKind, detail: impl Into<Arc<str>>) -> Self {
        Self { kind, detail: detail.into() }
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
        let kind = match error {
            SourceError::SourceChanged => SourceFailureKind::SourceChanged,
            SourceError::RangeOverflow { .. } | SourceError::OutOfBounds { .. } => {
                SourceFailureKind::Bounds
            }
            SourceError::ReadLimitExceeded { .. }
            | SourceError::PlatformLimitExceeded { .. }
            | SourceError::AllocationFailed { .. } => SourceFailureKind::ResourceLimit,
            SourceError::UnexpectedEof { .. }
            | SourceError::InvalidReadCount { .. }
            | SourceError::Io(_) => SourceFailureKind::Backend,
            _ => SourceFailureKind::Backend,
        };
        Self::new(kind, Arc::<str>::from(error.to_string()))
    }
}

#[derive(Debug)]
struct RecoveryBudget {
    limit: u64,
    used: Mutex<u64>,
    available: Condvar,
}

impl RecoveryBudget {
    fn new(limit: u64) -> Self {
        Self { limit, used: Mutex::new(0), available: Condvar::new() }
    }

    fn acquire(&'static self, bytes: u64) -> Result<RecoveryCharge, SourceFailure> {
        if bytes > self.limit {
            return Err(SourceFailure::resource(format!(
                "recovery request {bytes} exceeds process-wide limit {}",
                self.limit
            )));
        }
        let mut used = self
            .used
            .lock()
            .map_err(|_| SourceFailure::new(SourceFailureKind::Backend, "recovery budget lock poisoned"))?;
        while used.saturating_add(bytes) > self.limit {
            used = self
                .available
                .wait(used)
                .map_err(|_| SourceFailure::new(SourceFailureKind::Backend, "recovery budget lock poisoned"))?;
        }
        *used += bytes;
        Ok(RecoveryCharge { budget: self, bytes })
    }
}

struct RecoveryCharge {
    budget: &'static RecoveryBudget,
    bytes: u64,
}

impl RecoveryCharge {
    fn shrink_to(&mut self, bytes: u64) {
        debug_assert!(bytes <= self.bytes);
        let released = self.bytes.saturating_sub(bytes);
        if released == 0 {
            return;
        }
        let mut used = self.budget.used.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *used = used.saturating_sub(released);
        self.bytes = bytes;
        self.budget.available.notify_all();
    }
}

impl Drop for RecoveryCharge {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *used = used.saturating_sub(self.bytes);
        self.budget.available.notify_all();
    }
}

fn recovery_metadata_budget() -> &'static RecoveryBudget {
    static BUDGET: OnceLock<RecoveryBudget> = OnceLock::new();
    BUDGET.get_or_init(|| RecoveryBudget::new(RECOVERY_METADATA_BUDGET_BYTES))
}

fn recovery_payload_budget() -> &'static RecoveryBudget {
    static BUDGET: OnceLock<RecoveryBudget> = OnceLock::new();
    BUDGET.get_or_init(|| RecoveryBudget::new(RECOVERY_PAYLOAD_BUDGET_BYTES))
}

pub(crate) struct RecoveredStream {
    bytes: Vec<u8>,
    _charge: RecoveryCharge,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckedSourceRange {
    offset: u64,
    length: u64,
}

struct SourceScan {
    streams: BTreeMap<u32, CheckedSourceRange>,
    sha256: String,
    _metadata_charge: RecoveryCharge,
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
        Self { source, scan: OnceLock::new() }
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
        let charge = recovery_payload_budget().acquire(range.length)?;
        let bytes = read_bounded_range(
            self.source.as_ref(),
            range.offset,
            range.length,
            MAX_RECOVERED_STREAM_BYTES,
        )
        .map_err(SourceFailure::from)?;
        Ok(Some(RecoveredStream { bytes, _charge: charge }))
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
}

fn read_bounded_range(
    source: &dyn RandomAccessSource,
    offset: u64,
    length: u64,
    limit: u64,
) -> SourceResult<Vec<u8>> {
    if length > limit {
        return Err(SourceError::ReadLimitExceeded { requested: length, limit });
    }
    let source_len = source.len()?;
    let end = offset
        .checked_add(length)
        .ok_or(SourceError::RangeOverflow { offset, length })?;
    if end > source_len {
        return Err(SourceError::OutOfBounds { offset, length, source_len });
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
    let maximum_metadata = (MAX_RECOVERY_INDEX_ENTRIES as u64)
        .checked_mul(RECOVERY_INDEX_ENTRY_BYTES)
        .expect("fixed recovery metadata cap fits u64");
    let mut metadata_charge = recovery_metadata_budget().acquire(maximum_metadata)?;
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
    metadata_charge.shrink_to(
        (scanner.streams.len() as u64).saturating_mul(RECOVERY_INDEX_ENTRY_BYTES),
    );
    Ok(SourceScan {
        streams: scanner.streams,
        sha256,
        _metadata_charge: metadata_charge,
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
        if self.object.is_some() && self.recent_ends_with(b"stream") {
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
    fn object(object: ObjectId, error: impl fmt::Display) -> Self {
        let detail = error.to_string();
        Self {
            phase: AccessPhase::Resolve,
            page: None,
            object,
            kind: AccessKind::Backend,
            detail,
        }
    }

    fn typed(object: ObjectId, kind: AccessKind, error: impl fmt::Display) -> Self {
        let mut failure = Self::object(object, error);
        failure.kind = kind;
        failure
    }

    fn source(error: SourceFailure) -> Self {
        let kind = match error.kind {
            SourceFailureKind::SourceChanged => AccessKind::SourceChanged,
            SourceFailureKind::Bounds => AccessKind::Bounds,
            SourceFailureKind::ResourceLimit => AccessKind::ResourceLimit,
            SourceFailureKind::Backend => AccessKind::Backend,
        };
        Self::typed((0, 0), kind, error)
    }

    fn at(mut self, phase: AccessPhase, page: Option<u32>) -> Self {
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
    LEGACY_SUPPRESSION.iter().find_map(|(candidate, disposition)| {
        (*candidate == phase).then_some(*disposition)
    })
}

fn suppress_default<T: Default>(
    result: Result<T, AccessError>,
    disposition: Suppression,
) -> T {
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
/// graph. `Owned` is the lazy-reader shape: one independently owned resolved object. Neither
/// variant exposes an object-derived borrow beyond the closure passed to [`Self::read`].
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
    #[allow(dead_code)] // constructed by the L3 indexed adapter
    Owned { object: Arc<Object>, id: ObjectId },
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

    #[allow(dead_code)] // resource consumers migrate incrementally through L2b
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
            return Err(AccessError::typed(id, AccessKind::Type, "resolved object is not a stream"));
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
                        AccessError::typed(id, AccessKind::Bounds, format!("array index {index} is out of bounds"))
                    })?,
            };
        }
        Ok(inspect(object))
    }

    #[allow(dead_code)] // constructed by the L3 indexed adapter; unit-tested in L2
    fn owned(id: ObjectId, object: Object) -> Self {
        Self {
            owner: ObjectOwner::Owned {
                object: Arc::new(object),
                id,
            },
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
    #[allow(dead_code)] // consumer migrations begin in L2b
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
            step.ok_or_else(|| AccessError::typed(self.root_id(), AccessKind::Type, "object has no dictionary"))?,
        )
    }

    /// An array entry that stays attached to the root object which owns it.
    #[allow(dead_code)] // consumer migrations begin in L2b
    pub(crate) fn array_entry(
        &self,
        access: &dyn DocumentAccess,
        index: usize,
    ) -> Result<Self, AccessError> {
        self.child(access, PathStep::ArrayIndex(index))
    }

    fn root_id(&self) -> ObjectId {
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
        let root = self.trailer_entry(b"Root")
            .map_err(|error| error.at(AccessPhase::Catalog, None))?;
        DictionaryHandle::new(root)
            .map_err(|error| error.at(AccessPhase::Catalog, None))
    }
    fn page(&self, id: ObjectId) -> Result<DictionaryHandle, AccessError> {
        let page = self.object(id)
            .map_err(|error| error.at(AccessPhase::Page, None))?;
        DictionaryHandle::new(page)
            .map_err(|error| error.at(AccessPhase::Page, None))
    }
    /// Materialize a page's decoded content with the selected backend's exact fallback policy.
    fn page_content(&self, page: ObjectId) -> Result<Vec<u8>, AccessError>;
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
    #[cfg(test)]
    fn source(&self) -> Arc<dyn RandomAccessSource>;
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
        self.source_recovery()
            .sha256()
            .map_err(AccessError::source)
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
pub(crate) fn test_adapter_with_source(
    document: &Document,
    raw: &[u8],
) -> EagerDocumentAdapter {
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
        let value = self
            .document
            .trailer
            .get(key)
            .map_err(|error| AccessError::object((0, 0), error).at(AccessPhase::Trailer, None))?;
        if let Object::Reference(id) = value {
            self.object(*id)
        } else {
            Ok(ObjectHandle::eager_trailer_entry(
                Arc::clone(&self.document),
                key,
            ))
        }
    }

    fn page_content(&self, page: ObjectId) -> Result<Vec<u8>, AccessError> {
        self.document
            .get_dictionary(page)
            .map_err(|error| AccessError::object(page, error).at(AccessPhase::PageContent, None))?;
        Ok(self.document.get_page_content(page))
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
        self.document
            .extract_text(&[page])
            .map_err(|error| AccessError::object((0, 0), error).at(AccessPhase::FallbackText, Some(page)))
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

    #[cfg(test)]
    fn source(&self) -> Arc<dyn RandomAccessSource> {
        Arc::clone(&self.recovery.source)
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

/// An immutable encoded-source slice that carries its source owner with it.
#[derive(Clone)]
#[allow(dead_code)] // encoded stream descriptors start returning this in L3
pub(crate) struct SourceRange {
    source: Arc<dyn RandomAccessSource>,
    offset: u64,
    length: u64,
}

#[allow(dead_code)]
impl SourceRange {
    pub(crate) fn new(source: Arc<dyn RandomAccessSource>, offset: u64, length: u64) -> Self {
        Self {
            source,
            offset,
            length,
        }
    }

    /// Materialize the range only after enforcing the caller's explicit allocation limit.
    pub(crate) fn read(&self, limit: u64) -> SourceResult<Vec<u8>> {
        self.source.read_range(self.offset, self.length, limit)
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
            let counted_source: Arc<dyn RandomAccessSource> = Arc::new(CountingSource {
                inner: inner.source(),
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

        fn failure(&self, point: FaultPoint, object: ObjectId) -> Result<(), AccessError> {
            if self.fault == Some(point) {
                Err(AccessError::typed(
                    object,
                    AccessKind::Injected,
                    format!("injected {point:?} failure"),
                ).at(point.phase(), None))
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

        fn page_content(&self, page: ObjectId) -> Result<Vec<u8>, AccessError> {
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
            self.counts.fallback_text_reads.fetch_add(1, Ordering::Relaxed);
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

        fn source(&self) -> Arc<dyn RandomAccessSource> {
            self.counts.source_requests.fetch_add(1, Ordering::Relaxed);
            Arc::clone(&self.recovery.source)
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
    fn owned_handle_and_source_range_keep_their_owners_alive() {
        let handle = ObjectHandle::owned(
            (7, 0),
            Object::String(b"owned".to_vec(), lopdf::StringFormat::Literal),
        );
        assert_eq!(
            handle.read(|o| o.as_str().unwrap().to_vec()).unwrap(),
            b"owned"
        );

        let (adapter, _) = adapter(Vec::new(), b"abcdef");
        let range = SourceRange::new(adapter.source(), 2, 3);
        drop(adapter);
        assert_eq!(range.read(3).unwrap(), b"cde");
        assert!(range.read(2).is_err());
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

        fn page_content(&self, page: ObjectId) -> Result<Vec<u8>, AccessError> {
            Err(AccessError::object(page, "unexpected page content read"))
        }

        fn pages(&self) -> Result<Vec<PageRef>, AccessError> {
            Ok(Vec::new())
        }

        fn fallback_page_text(&self, page: u32) -> Result<String, AccessError> {
            Err(AccessError::object((page, 0), "unexpected fallback text read"))
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

        fn source(&self) -> Arc<dyn RandomAccessSource> {
            Arc::new(BytesSource::new(Arc::from(&b""[..])))
        }

        fn source_recovery(&self) -> Arc<SourceRecovery> {
            Arc::new(SourceRecovery::new(self.source()))
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
                    let source_error = fault.source().read_range(0, 1, 1).err().unwrap();
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
        assert_eq!(legacy_suppression(catalog_error.phase), Some(Suppression::SkipMetadata));

        let counts = Arc::new(AccessCounts::default());
        let page_fault = FaultAccess::new(
            Arc::new(adapter.clone()),
            Some(FaultPoint::Object),
            Arc::clone(&counts),
        );
        let page_error = page_fault.page(ids[0]).err().unwrap();
        assert_eq!(page_error.phase, AccessPhase::Page);
        assert_eq!(legacy_suppression(page_error.phase), Some(Suppression::SkipNode));

        let counts = Arc::new(AccessCounts::default());
        counts.opens.fetch_add(1, Ordering::Relaxed);
        let counted = FaultAccess::new(Arc::new(adapter), None, counts);
        assert_eq!(counted.object_ids(), ids);
        assert_eq!(counted.counts.object_lists.load(Ordering::Relaxed), 1);
        assert_eq!(counted.source().read_range(1, 3, 3).unwrap(), b"bcd");
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
            digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
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
            b"42 0 obj\n<<>>\nstream\nfirst\nendstream\n42 0 obj\n<<>>\nstream\nsecond\nendstream".as_slice(),
            b"42 0 obj\n<<>>\nstream\nendstream".as_slice(),
        ];
        for raw in ordinary {
            assert_eq!(scanned_range(raw, 42).unwrap(), legacy_recovery_range(raw, 42));
        }

        let mut split = vec![b'x'; SOURCE_CHUNK_BYTES - 3];
        split.extend_from_slice(b"42 0 obj\n<<>>\nstream\r\nsplit\r\nendstream");
        assert_eq!(scanned_range(&split, 42).unwrap(), legacy_recovery_range(&split, 42));

        // Approved correction 1: a requested object number is not a suffix of a larger number.
        let suffix = b"142 0 obj\n<<>>\nstream\nwrong\nendstream";
        assert!(legacy_recovery_range(suffix, 42).is_some());
        assert!(scanned_range(suffix, 42).unwrap().is_none());

        // Approved correction 2: a streamless object cannot steal a later object's stream.
        let spill = b"42 0 obj\n<<>>\nendobj\n43 0 obj\n<<>>\nstream\nwrong\nendstream";
        assert!(legacy_recovery_range(spill, 42).is_some());
        assert!(scanned_range(spill, 42).unwrap().is_none());

        // The frozen 64 MiB recovery cap refuses the range before allocating its payload.
        let mut oversize = b"42 0 obj\n<<>>\nstream\n".to_vec();
        oversize.resize(oversize.len() + MAX_RECOVERED_STREAM_BYTES as usize + 1, b'x');
        oversize.extend_from_slice(b"\nendstream");
        let recovery = SourceRecovery::new(Arc::new(BytesSource::new(oversize.into())));
        let error = recovery.recover_stream(42).err().expect("oversize recovery must fail");
        assert_eq!(error.kind, SourceFailureKind::ResourceLimit);
    }

    #[test]
    fn recovery_metadata_and_process_payload_admission_are_hard_bounded() {
        let mut hostile = Vec::new();
        for object in 0..=MAX_RECOVERY_INDEX_ENTRIES {
            hostile.extend_from_slice(format!("{object} 0 obj stream\nx\nendstream\n").as_bytes());
        }
        let error = scan_source_streams(&BytesSource::new(hostile.into()))
            .err()
            .expect("hostile recovery index must hit its entry cap");
        assert_eq!(error.kind, SourceFailureKind::ResourceLimit);

        let budget: &'static RecoveryBudget = Box::leak(Box::new(RecoveryBudget::new(10)));
        let first = budget.acquire(7).unwrap();
        let started = Arc::new(std::sync::Barrier::new(2));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child = {
            let child_budget = budget;
            let started = Arc::clone(&started);
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                started.wait();
                let _second = child_budget.acquire(6).unwrap();
                finished.store(true, Ordering::Release);
            })
        };
        started.wait();
        std::thread::yield_now();
        assert!(!finished.load(Ordering::Acquire));
        drop(first);
        child.join().unwrap();
        assert!(finished.load(Ordering::Acquire));
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
}
