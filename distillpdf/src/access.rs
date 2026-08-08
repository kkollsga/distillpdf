//! Owned PDF object/source access boundary.
//!
//! Extraction code must not retain a borrow into lopdf's eager `Document`: L3 replaces the
//! backend with on-demand owned resolution, where no such document-wide borrow exists.  Short
//! reads therefore happen through [`ObjectHandle::read`], while values that escape a read are
//! explicitly owned.  The eager implementation remains the compatibility oracle through L9.

use crate::object_cells::{
    CellLoadError, ContainerCellError, NegativeDisposition, ObjectCellArena, ObjectCellDomain,
    ResolvedObjectPin, ResolvedObjectStreamPin,
};
use lopdf::{
    BoundedObject, BytesSource, DecompressError, Dictionary, Document, IndexedObjectLocation,
    IndexedReader, IndexedReaderError, IndexedReaderOptions, Object, ObjectId, PageMap,
    RandomAccessSource, ScalarResolutionPermit, SourceError, SourceResult,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

const SOURCE_CHUNK_BYTES: usize = 64 * 1024;
const MAX_RECOVERED_STREAM_BYTES: u64 = 64 * 1024 * 1024;
const RECOVERY_INDEX_ENTRY_BYTES: u64 = 128;
const MAX_RECOVERY_INDEX_ENTRIES: usize = 65_536;
const RECOVERY_METADATA_BUDGET_BYTES: u64 = 128 * 1024 * 1024;
/// Provisional process-wide O budget shared by every retained object or payload.
const PROVISIONAL_O_BYTES: u64 = 64 * 1024 * 1024;
const INDEXED_OBJECT_BYTES: u64 = 4 * 1024 * 1024;
const INDEXED_STREAM_BYTES: u64 = 64 * 1024 * 1024;
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
            | SourceError::Io(_) => SourceFailureKind::SourceIo,
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
    #[cfg(test)]
    wait_entered: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
}

impl RecoveryBudget {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            used: Mutex::new(0),
            available: Condvar::new(),
            #[cfg(test)]
            wait_entered: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn set_wait_entered(&self, sender: std::sync::mpsc::SyncSender<()>) {
        *self.wait_entered.lock().unwrap() = Some(sender);
    }

    fn acquire(&'static self, bytes: u64) -> Result<RecoveryCharge, SourceFailure> {
        if bytes > self.limit {
            return Err(SourceFailure::resource(format!(
                "recovery request {bytes} exceeds process-wide limit {}",
                self.limit
            )));
        }
        let mut used = self.used.lock().map_err(|_| {
            SourceFailure::new(SourceFailureKind::Backend, "recovery budget lock poisoned")
        })?;
        while used.saturating_add(bytes) > self.limit {
            #[cfg(test)]
            if let Some(sender) = self.wait_entered.lock().unwrap().take() {
                sender.send(()).unwrap();
            }
            used = self.available.wait(used).map_err(|_| {
                SourceFailure::new(SourceFailureKind::Backend, "recovery budget lock poisoned")
            })?;
        }
        *used += bytes;
        Ok(RecoveryCharge {
            budget: self,
            bytes,
        })
    }

    fn acquire_available(&'static self, maximum: u64) -> Result<RecoveryCharge, SourceFailure> {
        let mut used = self
            .used
            .lock()
            .map_err(|_| SourceFailure::new(SourceFailureKind::Backend, "budget lock poisoned"))?;
        let bytes = self.limit.saturating_sub(*used).min(maximum);
        if bytes == 0 {
            return Err(SourceFailure::resource(format!(
                "process-wide {}-byte budget has no available capacity",
                self.limit
            )));
        }
        *used += bytes;
        Ok(RecoveryCharge {
            budget: self,
            bytes,
        })
    }

    fn try_acquire(&'static self, bytes: u64) -> Result<RecoveryCharge, SourceFailure> {
        if bytes > self.limit {
            return Err(SourceFailure::resource(format!(
                "request {bytes} exceeds process-wide limit {}",
                self.limit
            )));
        }
        let mut used = self
            .used
            .lock()
            .map_err(|_| SourceFailure::new(SourceFailureKind::Backend, "budget lock poisoned"))?;
        if used.saturating_add(bytes) > self.limit {
            return Err(SourceFailure::resource(format!(
                "request {bytes} exceeds {} available process-wide bytes",
                self.limit.saturating_sub(*used)
            )));
        }
        *used += bytes;
        Ok(RecoveryCharge {
            budget: self,
            bytes,
        })
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
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *used = used.saturating_sub(released);
        self.bytes = bytes;
        self.budget.available.notify_all();
    }
}

impl Drop for RecoveryCharge {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *used = used.saturating_sub(self.bytes);
        self.budget.available.notify_all();
    }
}

fn recovery_metadata_budget() -> &'static RecoveryBudget {
    static BUDGET: OnceLock<RecoveryBudget> = OnceLock::new();
    BUDGET.get_or_init(|| RecoveryBudget::new(RECOVERY_METADATA_BUDGET_BYTES))
}

fn provisional_o_budget() -> &'static RecoveryBudget {
    static BUDGET: OnceLock<RecoveryBudget> = OnceLock::new();
    BUDGET.get_or_init(|| RecoveryBudget::new(PROVISIONAL_O_BYTES))
}

pub(crate) struct RecoveredStream {
    bytes: Vec<u8>,
    _charge: RecoveryCharge,
}

/// Process-wide payload admission retained by a consumer-owned parsed value.
pub(crate) struct PayloadCharge {
    _charge: RecoveryCharge,
}

/// Decoded stream bytes plus the admission that covered their allocation.
pub(crate) struct ChargedStreamBytes {
    bytes: Vec<u8>,
    charge: PayloadCharge,
}

impl ChargedStreamBytes {
    pub(crate) fn into_parts(self) -> (Vec<u8>, PayloadCharge) {
        (self.bytes, self.charge)
    }
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
    _charge: Option<RecoveryCharge>,
}

impl PageContent {
    fn eager(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            _charge: None,
        }
    }

    fn indexed(bytes: Vec<u8>, charge: Option<RecoveryCharge>) -> Self {
        debug_assert_eq!(
            bytes.capacity() as u64,
            charge.as_ref().map_or(0, |charge| charge.bytes)
        );
        Self {
            bytes,
            _charge: charge,
        }
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
        // Recovery can be invoked while another O-owned handle is live. Fail deterministically
        // instead of waiting for capacity that this call stack itself may retain.
        let charge = provisional_o_budget().try_acquire(range.length)?;
        let bytes = read_bounded_range(
            self.source.as_ref(),
            range.offset,
            range.length,
            MAX_RECOVERED_STREAM_BYTES,
        )
        .map_err(SourceFailure::from)?;
        Ok(Some(RecoveredStream {
            bytes,
            _charge: charge,
        }))
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
    metadata_charge
        .shrink_to((scanner.streams.len() as u64).saturating_mul(RECOVERY_INDEX_ENTRY_BYTES));
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
    CellFull,
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

fn indexed_error(object: ObjectId, error: IndexedReaderError) -> AccessError {
    let error = match error {
        IndexedReaderError::Source(source) => {
            let mut failure = AccessError::source(SourceFailure::from(source));
            failure.object = object;
            return failure;
        }
        other => other,
    };
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

fn indexed_cell_error(object: ObjectId, error: IndexedReaderError) -> CellLoadError {
    let disposition = indexed_negative_disposition(&error);
    CellLoadError::new(indexed_error(object, error), disposition)
}

fn indexed_negative_disposition(error: &IndexedReaderError) -> NegativeDisposition {
    match error {
        IndexedReaderError::MissingNormalObject { .. }
        | IndexedReaderError::MissingNormalObjectAtXref { .. }
        | IndexedReaderError::GenerationMismatch { .. }
        | IndexedReaderError::IndirectObjectMismatch { .. }
        | IndexedReaderError::InvalidIndirectObject { .. }
        | IndexedReaderError::IncompleteObject { .. }
        | IndexedReaderError::ObjectLimitExceeded { .. }
        | IndexedReaderError::NotScalarObject { .. }
        | IndexedReaderError::NotStreamObject { .. }
        | IndexedReaderError::UnsupportedBoundedScalar { .. }
        | IndexedReaderError::StreamLimitExceeded { .. }
        | IndexedReaderError::NegativeStreamLength { .. }
        | IndexedReaderError::MissingEndstream { .. }
        | IndexedReaderError::ResolutionCycle { .. }
        | IndexedReaderError::ResolutionDepthExceeded { .. }
        | IndexedReaderError::ObjectDecryption { .. } => NegativeDisposition::Persistent,
        IndexedReaderError::Source(_)
        | IndexedReaderError::InvalidHeader { .. }
        | IndexedReaderError::InvalidStartXref { .. }
        | IndexedReaderError::StartXrefOutOfBounds { .. }
        | IndexedReaderError::InvalidXref { .. }
        | IndexedReaderError::IncompleteXref { .. }
        | IndexedReaderError::InvalidTrailer { .. }
        | IndexedReaderError::StructureLimitExceeded { .. }
        | IndexedReaderError::EntryLimitExceeded { .. }
        | IndexedReaderError::RevisionLimitExceeded { .. }
        | IndexedReaderError::InvalidTrailerOffset { .. }
        | IndexedReaderError::XrefDecompression(_)
        | IndexedReaderError::IndirectHeaderLimitExceeded { .. }
        | IndexedReaderError::ScalarResourceLimit { .. }
        | IndexedReaderError::ScalarResolutionCancelled { .. }
        | IndexedReaderError::ScalarResolutionClosed { .. }
        | IndexedReaderError::ObjectStreamContainerNotStream { .. }
        | IndexedReaderError::ObjectStreamMember { .. }
        | IndexedReaderError::ObjectStreamBatchSetup { .. }
        | IndexedReaderError::ObjectStreamCacheBypass { .. }
        | IndexedReaderError::PasswordRequired
        | IndexedReaderError::InvalidPassword
        | IndexedReaderError::Encryption(_)
        | IndexedReaderError::InvalidEncryptDictionary
        | IndexedReaderError::PageCountLimitExceeded { .. } => NegativeDisposition::FlightOnly,
        #[allow(unreachable_patterns)]
        _ => NegativeDisposition::FlightOnly,
    }
}

fn fatal_lazy_access(error: &AccessError) -> bool {
    matches!(
        error.kind,
        AccessKind::ResourceLimit
            | AccessKind::CellFull
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
    pub(crate) object_resolutions: AtomicU64,
    pub(crate) object_failures: AtomicU64,
    pub(crate) active_resolutions: AtomicU64,
    pub(crate) peak_active_resolutions: AtomicU64,
    pub(crate) container_preparations: AtomicU64,
    pub(crate) container_preparation_successes: AtomicU64,
    pub(crate) container_preparation_failures: AtomicU64,
    pub(crate) container_persistent_native_failures: AtomicU64,
    pub(crate) container_persistent_above_cap_failures: AtomicU64,
    pub(crate) container_flight_only_failures: AtomicU64,
    pub(crate) container_exact_key_invariant_failures: AtomicU64,
    pub(crate) active_container_preparations: AtomicU64,
    pub(crate) peak_active_container_preparations: AtomicU64,
    pub(crate) initial_container_permit_current_bytes: AtomicU64,
    pub(crate) initial_container_permit_peak_bytes: AtomicU64,
    pub(crate) peak_container_permit_bytes: AtomicU64,
    pub(crate) retained_object_estimated_bytes: AtomicU64,
    pub(crate) peak_retained_object_estimated_bytes: AtomicU64,
    pub(crate) retained_object_admitted_bytes: AtomicU64,
    pub(crate) peak_retained_object_admitted_bytes: AtomicU64,
    pub(crate) peak_resolution_bytes: AtomicU64,
    pub(crate) page_map_builds: AtomicU64,
    pub(crate) page_content_ops: AtomicU64,
    pub(crate) index_estimated_bytes: AtomicU64,
    pub(crate) index_objects: AtomicU64,
    pub(crate) index_pages: AtomicU64,
}

struct IndexedContainerAttempt<'a> {
    counters: &'a IndexedAdapterCounters,
}

impl<'a> IndexedContainerAttempt<'a> {
    fn start(counters: &'a IndexedAdapterCounters) -> Self {
        counters
            .container_preparations
            .fetch_add(1, Ordering::Relaxed);
        let active = counters
            .active_container_preparations
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        counters
            .peak_active_container_preparations
            .fetch_max(active, Ordering::Relaxed);
        Self { counters }
    }

    fn record_peak(&self, peak_bytes: u64) {
        self.counters
            .peak_container_permit_bytes
            .fetch_max(peak_bytes, Ordering::Relaxed);
    }

    fn record_success(&self) {
        self.counters
            .container_preparation_successes
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_failure(&self, class: crate::objstm_failures::ObjStmFailureClass) {
        use crate::objstm_failures::ObjStmFailureClass;

        self.counters
            .container_preparation_failures
            .fetch_add(1, Ordering::Relaxed);
        let counter = match class {
            ObjStmFailureClass::PersistentNative => {
                &self.counters.container_persistent_native_failures
            }
            ObjStmFailureClass::PersistentAboveCap => {
                &self.counters.container_persistent_above_cap_failures
            }
            ObjStmFailureClass::FlightOnly => &self.counters.container_flight_only_failures,
            ObjStmFailureClass::ExactKeyInvariant => {
                &self.counters.container_exact_key_invariant_failures
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for IndexedContainerAttempt<'_> {
    fn drop(&mut self) {
        self.counters
            .active_container_preparations
            .fetch_sub(1, Ordering::Relaxed);
    }
}

fn indexed_object_exclusive() -> &'static Mutex<()> {
    static EXCLUSIVE: OnceLock<Mutex<()>> = OnceLock::new();
    EXCLUSIVE.get_or_init(|| Mutex::new(()))
}

fn indexed_page_content_exclusive() -> &'static Mutex<()> {
    static EXCLUSIVE: OnceLock<Mutex<()>> = OnceLock::new();
    EXCLUSIVE.get_or_init(|| Mutex::new(()))
}

fn exclusive(lock: &'static Mutex<()>) -> MutexGuard<'static, ()> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) fn indexed_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    exclusive(LOCK.get_or_init(|| Mutex::new(())))
}

struct IndexedRetainedCharge {
    estimated_bytes: u64,
    admitted_bytes: u64,
    counters: Arc<IndexedAdapterCounters>,
    _budget: RecoveryCharge,
}

impl IndexedRetainedCharge {
    fn new(
        estimated_bytes: u64,
        admitted_bytes: u64,
        counters: Arc<IndexedAdapterCounters>,
        mut budget: RecoveryCharge,
    ) -> Self {
        budget.shrink_to(admitted_bytes);
        let estimated = counters
            .retained_object_estimated_bytes
            .fetch_add(estimated_bytes, Ordering::Relaxed)
            + estimated_bytes;
        counters
            .peak_retained_object_estimated_bytes
            .fetch_max(estimated, Ordering::Relaxed);
        let admitted = counters
            .retained_object_admitted_bytes
            .fetch_add(admitted_bytes, Ordering::Relaxed)
            + admitted_bytes;
        counters
            .peak_retained_object_admitted_bytes
            .fetch_max(admitted, Ordering::Relaxed);
        Self {
            estimated_bytes,
            admitted_bytes,
            counters,
            _budget: budget,
        }
    }
}

impl Drop for IndexedRetainedCharge {
    fn drop(&mut self) {
        self.counters
            .retained_object_estimated_bytes
            .fetch_sub(self.estimated_bytes, Ordering::Relaxed);
        self.counters
            .retained_object_admitted_bytes
            .fetch_sub(self.admitted_bytes, Ordering::Relaxed);
    }
}

fn retained_object_bytes(object: &Object) -> u64 {
    fn visit(object: &Object, depth: usize) -> u64 {
        if depth >= EAGER_RESOURCE_DEPTH {
            return std::mem::size_of::<Object>() as u64;
        }
        let base = std::mem::size_of::<Object>() as u64;
        match object {
            Object::Name(bytes) | Object::String(bytes, _) => {
                base.saturating_add(bytes.len() as u64)
            }
            Object::Array(values) => values.iter().fold(
                base.saturating_add((values.len() * std::mem::size_of::<Object>()) as u64),
                |sum, value| sum.saturating_add(visit(value, depth + 1)),
            ),
            Object::Dictionary(dictionary) => dictionary.iter().fold(base, |sum, (key, value)| {
                sum.saturating_add(key.len() as u64)
                    .saturating_add(visit(value, depth + 1))
            }),
            Object::Stream(stream) => stream.dict.iter().fold(
                base.saturating_add(stream.content.len() as u64),
                |sum, (key, value)| {
                    sum.saturating_add(key.len() as u64)
                        .saturating_add(visit(value, depth + 1))
                },
            ),
            Object::Null
            | Object::Boolean(_)
            | Object::Integer(_)
            | Object::Real(_)
            | Object::Reference(_) => base,
        }
    }
    visit(object, 0)
}

fn append_page_payload(
    page: ObjectId,
    output: &mut Vec<u8>,
    output_charge: &mut Option<RecoveryCharge>,
    payload: Vec<u8>,
    payload_charge: RecoveryCharge,
) -> Result<(), AccessError> {
    let additional = payload.len().checked_add(1).ok_or_else(|| {
        AccessError::typed(
            page,
            AccessKind::ResourceLimit,
            "page content length overflow",
        )
    })?;
    if output.capacity().saturating_sub(output.len()) >= additional {
        output.extend_from_slice(&payload);
        output.push(b'\n');
        drop(payload_charge);
        return Ok(());
    }

    let combined = output.len().checked_add(additional).ok_or_else(|| {
        AccessError::typed(
            page,
            AccessKind::ResourceLimit,
            "page content length overflow",
        )
    })?;
    let new_charge = provisional_o_budget()
        .try_acquire(combined as u64)
        .map_err(AccessError::source)?;
    let mut combined_output = Vec::new();
    combined_output.try_reserve_exact(combined).map_err(|_| {
        AccessError::typed(
            page,
            AccessKind::ResourceLimit,
            "page content allocation failed",
        )
    })?;
    if combined_output.capacity() as u64 > new_charge.bytes {
        return Err(AccessError::typed(
            page,
            AccessKind::ResourceLimit,
            "page content allocator exceeded admitted capacity",
        ));
    }
    combined_output.extend_from_slice(output);
    combined_output.extend_from_slice(&payload);
    combined_output.push(b'\n');
    drop(output_charge.take());
    drop(payload_charge);
    *output = combined_output;
    *output_charge = Some(new_charge);
    Ok(())
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
    Owned {
        object: Arc<Object>,
        id: ObjectId,
        _retained: Option<Arc<IndexedRetainedCharge>>,
    },
    Bounded {
        pin: ResolvedObjectPin,
        id: ObjectId,
    },
    LegacyBounded {
        object: Arc<BoundedObject>,
        id: ObjectId,
        _retained: Arc<IndexedRetainedCharge>,
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
            ObjectOwner::Owned { object, id, .. } => (object.as_ref(), *id),
            ObjectOwner::Bounded { pin, id } => (pin.owner().as_object(), *id),
            ObjectOwner::LegacyBounded { object, id, .. } => (object.as_object(), *id),
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

    /// Decode a stream under the process payload broker, retaining its charge for the caller.
    pub(crate) fn decoded_stream_bytes(
        &self,
        maximum: usize,
    ) -> Result<ChargedStreamBytes, AccessError> {
        let id = self.root_id();
        let mut charge = provisional_o_budget()
            .acquire_available(maximum as u64)
            .map_err(AccessError::source)?;
        let limit = usize::try_from(charge.bytes).unwrap_or(usize::MAX);
        let bytes = self
            .read(|object| object.as_stream()?.get_plain_content_with_limit(limit))?
            .map_err(|error| match error {
                lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded { .. }) => {
                    AccessError::typed(
                        id,
                        AccessKind::ResourceLimit,
                        format!("decoded stream exceeds available {limit}-byte allowance"),
                    )
                }
                other => AccessError::object(id, other),
            })?
            .into_boxed_slice()
            .into_vec();
        if bytes.capacity() as u64 > charge.bytes {
            return Err(AccessError::typed(
                id,
                AccessKind::ResourceLimit,
                "decoded stream allocator exceeded admitted capacity",
            ));
        }
        charge.shrink_to(bytes.capacity() as u64);
        Ok(ChargedStreamBytes {
            bytes,
            charge: PayloadCharge { _charge: charge },
        })
    }

    #[allow(dead_code)] // constructed by the L3 indexed adapter; unit-tested in L2
    fn owned(id: ObjectId, object: Object) -> Self {
        Self {
            owner: ObjectOwner::Owned {
                object: Arc::new(object),
                id,
                _retained: None,
            },
            path: Vec::new(),
        }
    }

    fn owned_charged(
        id: ObjectId,
        object: Object,
        counters: Arc<IndexedAdapterCounters>,
        budget: RecoveryCharge,
    ) -> Self {
        let bytes = retained_object_bytes(&object);
        let admitted = budget.bytes;
        Self {
            owner: ObjectOwner::Owned {
                object: Arc::new(object),
                id,
                _retained: Some(Arc::new(IndexedRetainedCharge::new(
                    bytes, admitted, counters, budget,
                ))),
            },
            path: Vec::new(),
        }
    }

    fn bounded(id: ObjectId, pin: ResolvedObjectPin) -> Self {
        Self {
            owner: ObjectOwner::Bounded { pin, id },
            path: Vec::new(),
        }
    }

    fn legacy_bounded(
        id: ObjectId,
        object: BoundedObject,
        counters: Arc<IndexedAdapterCounters>,
        budget: RecoveryCharge,
    ) -> Self {
        let retained = object.retained_bytes();
        Self {
            owner: ObjectOwner::LegacyBounded {
                object: Arc::new(object),
                id,
                _retained: Arc::new(IndexedRetainedCharge::new(
                    retained, retained, counters, budget,
                )),
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
            step.ok_or_else(|| {
                AccessError::typed(self.root_id(), AccessKind::Type, "object has no dictionary")
            })?,
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

    pub(crate) fn root_id(&self) -> ObjectId {
        match &self.owner {
            ObjectOwner::Eager { id, .. }
            | ObjectOwner::Owned { id, .. }
            | ObjectOwner::Bounded { id, .. }
            | ObjectOwner::LegacyBounded { id, .. } => *id,
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
            ObjectOwner::Eager { id, .. }
            | ObjectOwner::Owned { id, .. }
            | ObjectOwner::Bounded { id, .. }
            | ObjectOwner::LegacyBounded { id, .. } => Some(*id),
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
        Ok(PageContent::eager(self.document.get_page_content(page)))
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
// non-limit decode error and remains inside the page payload admission.
pub(crate) struct IndexedDocumentAdapter {
    reader: IndexedReader,
    recovery: Arc<SourceRecovery>,
    page_map: OnceLock<Result<Arc<PageMap>, AccessError>>,
    counters: Arc<IndexedAdapterCounters>,
    object_cells: ObjectCellArena,
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
        let reader = IndexedReader::open_shared(Arc::clone(&source), options)
            .map_err(|error| indexed_error((0, 0), error))?;
        let (map, stats) = reader
            .page_map_with_stats()
            .map_err(|error| indexed_error((0, 0), error).at(AccessPhase::Pages, None))?;
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
        let object_cells = ObjectCellDomain::production().open_arena()?;
        Ok(Self {
            reader,
            recovery: Arc::new(SourceRecovery::new(source)),
            page_map: OnceLock::from(Ok(map)),
            counters,
            object_cells,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))] // slice-2 diagnostics; slice 3 exposes route counters
    pub(crate) fn counters(&self) -> Arc<IndexedAdapterCounters> {
        Arc::clone(&self.counters)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn prepare_declared_object_stream(
        &self,
        container: ObjectId,
    ) -> Result<ResolvedObjectStreamPin, ContainerCellError> {
        self.object_cells
            .resolve_object_stream(container, |permit| {
                let initial = permit.stats();
                self.counters
                    .initial_container_permit_current_bytes
                    .store(initial.current_bytes, Ordering::Relaxed);
                self.counters
                    .initial_container_permit_peak_bytes
                    .store(initial.peak_bytes, Ordering::Relaxed);
                let attempt = IndexedContainerAttempt::start(&self.counters);
                let result = self
                    .reader
                    .prepare_object_stream_with_permit(container, permit);
                attempt.record_peak(permit.stats().peak_bytes);
                match result {
                    Ok(owner) => {
                        attempt.record_success();
                        Ok(owner)
                    }
                    Err(error) => {
                        let failure = crate::objstm_failures::classify(container, error);
                        attempt.record_failure(failure.class());
                        Err(CellLoadError::objstm(failure))
                    }
                }
            })
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
                    .map_err(|error| indexed_error((0, 0), error).at(AccessPhase::Pages, None))
            })
            .clone()
    }

    fn resolve_bounded_legacy(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
        let _exclusive = exclusive(indexed_object_exclusive());
        let budget = provisional_o_budget()
            .acquire_available(PROVISIONAL_O_BYTES)
            .map_err(|error| AccessError::source(error).at(AccessPhase::Object, None))?;
        let active = self
            .counters
            .active_resolutions
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.counters
            .peak_active_resolutions
            .fetch_max(active, Ordering::Relaxed);
        self.counters
            .object_resolutions
            .fetch_add(1, Ordering::Relaxed);
        let permit = ScalarResolutionPermit::new(budget.bytes);
        let result = self.reader.resolve_object_with_permit(id, &permit);
        self.counters
            .active_resolutions
            .fetch_sub(1, Ordering::Relaxed);
        match result {
            Ok(object) => {
                self.counters
                    .peak_resolution_bytes
                    .fetch_max(object.peak_bytes(), Ordering::Relaxed);
                Ok(ObjectHandle::legacy_bounded(
                    id,
                    object,
                    Arc::clone(&self.counters),
                    budget,
                ))
            }
            Err(error) => {
                self.counters
                    .object_failures
                    .fetch_add(1, Ordering::Relaxed);
                Err(indexed_error(id, error))
            }
        }
    }

    fn resolve_bounded(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
        if matches!(
            self.reader.object_location(id),
            Some(IndexedObjectLocation::Compressed { .. })
        ) {
            return self.resolve_bounded_legacy(id);
        }
        let pin = self.object_cells.resolve(id, |permit| {
            let active = self
                .counters
                .active_resolutions
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            self.counters
                .peak_active_resolutions
                .fetch_max(active, Ordering::Relaxed);
            self.counters
                .object_resolutions
                .fetch_add(1, Ordering::Relaxed);
            let result = self.reader.resolve_object_with_permit(id, permit);
            self.counters
                .active_resolutions
                .fetch_sub(1, Ordering::Relaxed);
            match result {
                Ok(object) => {
                    self.counters
                        .peak_resolution_bytes
                        .fetch_max(object.peak_bytes(), Ordering::Relaxed);
                    Ok(object)
                }
                Err(error) => {
                    self.counters
                        .object_failures
                        .fetch_add(1, Ordering::Relaxed);
                    Err(indexed_cell_error(id, error))
                }
            }
        })?;
        Ok(ObjectHandle::bounded(id, pin))
    }

    fn resolve_bounded_terminal(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
        let mut current = id;
        let mut seen = std::collections::HashSet::new();
        let mut hops = 0;
        loop {
            if !seen.insert(current) {
                return Err(AccessError::typed(
                    current,
                    AccessKind::Backend,
                    "indirect reference cycle",
                ));
            }
            let handle = self.resolve_bounded(current)?;
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
        self.resolve_bounded_terminal(id)
            .map_err(|error| error.at(AccessPhase::Object, None))
    }

    fn trailer_entry(&self, key: &[u8]) -> Result<ObjectHandle, AccessError> {
        let _exclusive = exclusive(indexed_object_exclusive());
        let budget = provisional_o_budget()
            // The raw trailer seam returns an owned clone rather than a permit-backed owner, so
            // its size is unknowable until after cloning. Hold all of O across that clone and
            // reconcile to the retained estimate afterward.
            .try_acquire(PROVISIONAL_O_BYTES)
            .map_err(|error| AccessError::source(error).at(AccessPhase::Trailer, None))?;
        // `trailer_entry_raw_owned` is the independently reviewed L3.0a/a477ba4 seam: it clones only
        // the immutable raw trailer value and never dereferences an indirect entry.
        let value = self.reader.trailer_entry_raw_owned(key).ok_or_else(|| {
            AccessError::typed((0, 0), AccessKind::Type, "missing trailer entry")
                .at(AccessPhase::Trailer, None)
        })?;
        if let Object::Reference(id) = value {
            drop(budget);
            drop(_exclusive);
            return self
                .object(id)
                .map_err(|error| error.at(AccessPhase::Trailer, None));
        }
        let retained = retained_object_bytes(&value);
        // Other live bounded owners consume the same O allowance. A direct trailer clone may
        // retain only the capacity actually admitted for this operation, not merely the nominal
        // 64 MiB ceiling.
        if retained > budget.bytes {
            return Err(AccessError::typed(
                (0, 0),
                AccessKind::ResourceLimit,
                format!(
                    "direct trailer value retains {retained} bytes, exceeding the admitted {} bytes",
                    budget.bytes
                ),
            )
            .at(AccessPhase::Trailer, None));
        }
        Ok(ObjectHandle::owned_charged(
            (0, 0),
            value,
            Arc::clone(&self.counters),
            budget,
        ))
    }

    fn page_content(&self, page: ObjectId) -> Result<PageContent, AccessError> {
        let _exclusive = exclusive(indexed_page_content_exclusive());
        self.counters
            .page_content_ops
            .fetch_add(1, Ordering::Relaxed);
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
                Err(_) => return Ok(PageContent::indexed(Vec::new(), None)),
            },
            ContentsShape::Array => page_handle
                .entry(self, b"Contents")
                .map_err(|error| error.at(AccessPhase::PageContent, None))?,
            _ => return Ok(PageContent::indexed(Vec::new(), None)),
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
            None => return Ok(PageContent::indexed(Vec::new(), None)),
        }

        let mut output = Vec::new();
        let mut output_charge = None;
        for stream in streams {
            if !stream
                .read(|object| object.as_stream().is_ok())
                .unwrap_or(false)
            {
                continue;
            }
            let logical_remaining = usize::try_from(PROVISIONAL_O_BYTES)
                .unwrap_or(usize::MAX)
                .saturating_sub(output.len())
                .saturating_sub(1);
            if logical_remaining == 0 {
                return Err(AccessError::typed(
                    page,
                    AccessKind::ResourceLimit,
                    format!("page content exceeds {PROVISIONAL_O_BYTES} bytes"),
                )
                .at(AccessPhase::PageContent, None));
            }
            let mut payload_charge = provisional_o_budget()
                .acquire_available(logical_remaining as u64)
                .map_err(|error| AccessError::source(error).at(AccessPhase::PageContent, None))?;
            let payload_limit = usize::try_from(payload_charge.bytes).unwrap_or(usize::MAX);
            let result = stream.read(|object| {
                let Ok(stream) = object.as_stream() else {
                    return Ok(None);
                };
                let payload = match stream.decompressed_content_with_limit(payload_limit) {
                    Ok(bytes) => bytes,
                    Err(lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded {
                        ..
                    })) => {
                        return Err(AccessError::typed(
                            page,
                            AccessKind::ResourceLimit,
                            format!(
                                "page content exceeds available {payload_limit}-byte allowance"
                            ),
                        ));
                    }
                    Err(_) if stream.content.len() <= payload_limit => stream.content.clone(),
                    Err(_) => {
                        return Err(AccessError::typed(
                            page,
                            AccessKind::ResourceLimit,
                            format!(
                                "page content exceeds available {payload_limit}-byte allowance"
                            ),
                        ));
                    }
                };
                Ok(Some(payload))
            });
            match result {
                Ok(Ok(Some(payload))) => {
                    if payload.capacity() as u64 > payload_charge.bytes {
                        return Err(AccessError::typed(
                            page,
                            AccessKind::ResourceLimit,
                            "decoded payload allocator exceeded admitted capacity",
                        )
                        .at(AccessPhase::PageContent, None));
                    }
                    payload_charge.shrink_to(payload.capacity() as u64);
                    append_page_payload(
                        page,
                        &mut output,
                        &mut output_charge,
                        payload,
                        payload_charge,
                    )
                    .map_err(|error| error.at(AccessPhase::PageContent, None))?;
                }
                Ok(Ok(None)) | Err(_) => {}
                Ok(Err(error)) => {
                    return Err(error.at(AccessPhase::PageContent, None));
                }
            }
        }
        Ok(PageContent::indexed(output, output_charge))
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
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

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

    #[derive(Default)]
    struct ObjStmBlockState {
        armed: bool,
        blocked: bool,
        released: bool,
    }

    struct ObjStmBlockControl {
        state: Arc<(Mutex<ObjStmBlockState>, Condvar)>,
        reads: Arc<AtomicU64>,
        failure: Arc<AtomicU8>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    #[repr(u8)]
    enum ObjStmSourceFailure {
        None = 0,
        Changed = 1,
        Io = 2,
    }

    impl ObjStmBlockControl {
        fn arm(&self, failure: ObjStmSourceFailure) {
            self.failure.store(failure as u8, Ordering::Release);
            let (state, _) = &*self.state;
            let mut state = state.lock().unwrap();
            state.armed = true;
            state.blocked = false;
            state.released = false;
        }

        fn wait_until_blocked(&self) {
            let (state, wake) = &*self.state;
            let mut state = state.lock().unwrap();
            while !state.blocked {
                state = wake.wait(state).unwrap();
            }
        }

        fn release(&self) {
            let (state, wake) = &*self.state;
            let mut state = state.lock().unwrap();
            state.released = true;
            wake.notify_all();
        }

        fn reads(&self) -> u64 {
            self.reads.load(Ordering::Relaxed)
        }

        fn clear_failure(&self) {
            self.failure
                .store(ObjStmSourceFailure::None as u8, Ordering::Release);
        }
    }

    struct ObjStmBlockingSource {
        bytes: Arc<[u8]>,
        control: Arc<ObjStmBlockControl>,
    }

    impl RandomAccessSource for ObjStmBlockingSource {
        fn len(&self) -> SourceResult<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
            self.control.reads.fetch_add(1, Ordering::Relaxed);
            let (state, wake) = &*self.control.state;
            let mut state = state.lock().unwrap();
            if state.armed && !state.blocked {
                state.blocked = true;
                wake.notify_all();
                while !state.released {
                    state = wake.wait(state).unwrap();
                }
            }
            drop(state);
            match self.control.failure.load(Ordering::Acquire) {
                value if value == ObjStmSourceFailure::Changed as u8 => {
                    return Err(SourceError::SourceChanged);
                }
                value if value == ObjStmSourceFailure::Io as u8 => {
                    return Err(SourceError::Io(std::io::Error::other(
                        "injected object-stream positioned I/O failure",
                    )));
                }
                _ => {}
            }
            let start =
                usize::try_from(offset).map_err(|_| SourceError::PlatformLimitExceeded {
                    requested: offset,
                    limit: usize::MAX as u64,
                })?;
            let take = out.len().min(self.bytes.len().saturating_sub(start));
            out[..take].copy_from_slice(&self.bytes[start..start + take]);
            Ok(take)
        }

        fn validate_unchanged(&self) -> SourceResult<()> {
            match self.control.failure.load(Ordering::Acquire) {
                value if value == ObjStmSourceFailure::Changed as u8 => {
                    Err(SourceError::SourceChanged)
                }
                value if value == ObjStmSourceFailure::Io as u8 => Err(SourceError::Io(
                    std::io::Error::other("injected object-stream validation I/O failure"),
                )),
                _ => Ok(()),
            }
        }
    }

    fn blocking_objstm_source(
        bytes: Arc<[u8]>,
    ) -> (Arc<dyn RandomAccessSource>, Arc<ObjStmBlockControl>) {
        let control = Arc::new(ObjStmBlockControl {
            state: Arc::new((Mutex::new(ObjStmBlockState::default()), Condvar::new())),
            reads: Arc::new(AtomicU64::new(0)),
            failure: Arc::new(AtomicU8::new(ObjStmSourceFailure::None as u8)),
        });
        let source: Arc<dyn RandomAccessSource> = Arc::new(ObjStmBlockingSource {
            bytes,
            control: Arc::clone(&control),
        });
        (source, control)
    }

    struct ObjStmWaitBarrier {
        id: ObjectId,
        target: usize,
        attached: Mutex<usize>,
        removed: AtomicU64,
        wake: Condvar,
    }

    impl ObjStmWaitBarrier {
        fn new(id: ObjectId, target: usize) -> Arc<Self> {
            Arc::new(Self {
                id,
                target,
                attached: Mutex::new(0),
                removed: AtomicU64::new(0),
                wake: Condvar::new(),
            })
        }

        fn wait_until_attached(&self) {
            let mut attached = self.attached.lock().unwrap();
            while *attached < self.target {
                attached = self.wake.wait(attached).unwrap();
            }
            assert_eq!(*attached, self.target);
        }

        fn assert_balanced(&self) {
            assert_eq!(self.removed.load(Ordering::Relaxed), self.target as u64);
        }
    }

    impl crate::object_cells::WaitEdgeHooks for ObjStmWaitBarrier {
        fn add(&self, _epoch: u64, id: ObjectId, _generation: u64, _ordinal: u64) {
            if id != self.id {
                return;
            }
            let mut attached = self.attached.lock().unwrap();
            *attached += 1;
            assert!(*attached <= self.target);
            self.wake.notify_all();
        }

        fn remove(&self, _epoch: u64, id: ObjectId, _generation: u64, _ordinal: u64) {
            if id == self.id {
                self.removed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    struct AccessGeneratedDir(PathBuf);

    impl AccessGeneratedDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "distillpdf-gate3-access-{}-{label}-{}",
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

    impl Drop for AccessGeneratedDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn generate_objstm_fixtures(output: &Path, arguments: &[&str]) {
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
            .expect("run deterministic ObjStm fixture generator");
        assert!(status.success(), "ObjStm fixture generation failed");
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
        let range = SourceRange::new(Arc::clone(&adapter.source_recovery().source), 2, 3);
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
        let (wait_tx, wait_rx) = std::sync::mpsc::sync_channel(0);
        budget.set_wait_entered(wait_tx);
        let child = {
            let child_budget = budget;
            std::thread::spawn(move || {
                let _second = child_budget.acquire(6).unwrap();
            })
        };
        wait_rx.recv().unwrap();
        assert_eq!(*budget.used.lock().unwrap(), 7);
        drop(first);
        child.join().unwrap();
        assert_eq!(*budget.used.lock().unwrap(), 0);
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

    fn indexed_object_stream_fixture() -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        let pages = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0,
        }));
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages),
        }));
        document.add_object(Object::Dictionary(dictionary! {
            "Producer" => "declared-container-adapter",
        }));
        document.trailer.set("Root", Object::Reference(catalog));
        let options = lopdf::SaveOptions::builder()
            .use_object_streams(true)
            .use_xref_streams(true)
            .build();
        let mut raw = Vec::new();
        document.save_with_options(&mut raw, options).unwrap();
        raw
    }

    fn first_declared_container(adapter: &IndexedDocumentAdapter) -> ObjectId {
        adapter
            .reader
            .object_ids()
            .into_iter()
            .find_map(|id| match adapter.reader.object_location(id) {
                Some(IndexedObjectLocation::Compressed { container, .. }) => Some(container),
                _ => None,
            })
            .expect("fixture must declare a compressed object")
    }

    fn assert_representation_counter_sums(snapshot: &crate::object_cells::ObjectCellSnapshot) {
        macro_rules! assert_sum {
            ($field:ident) => {
                assert_eq!(
                    snapshot.$field,
                    snapshot.raw.$field + snapshot.containers.$field + snapshot.members.$field,
                    concat!("aggregate ", stringify!($field))
                );
            };
        }
        assert_sum!(calls);
        assert_sum!(loads);
        assert_sum!(hits);
        assert_sum!(waits);
        assert_sum!(negative_hits);
        assert_sum!(transient_shares);
        assert_sum!(bypasses);
        assert_sum!(evictions);
        assert_sum!(cancellations);
    }

    fn assert_member_counters_zero(snapshot: &crate::object_cells::ObjectCellSnapshot) {
        assert_eq!(snapshot.members.calls, 0);
        assert_eq!(snapshot.members.loads, 0);
        assert_eq!(snapshot.members.hits, 0);
        assert_eq!(snapshot.members.waits, 0);
        assert_eq!(snapshot.members.negative_hits, 0);
        assert_eq!(snapshot.members.transient_shares, 0);
        assert_eq!(snapshot.members.bypasses, 0);
        assert_eq!(snapshot.members.evictions, 0);
        assert_eq!(snapshot.members.cancellations, 0);
    }

    fn assert_container_delta(
        baseline: &crate::object_cells::ObjectCellSnapshot,
        snapshot: &crate::object_cells::ObjectCellSnapshot,
        expected: crate::object_cells::RepresentationSnapshot,
    ) {
        let actual = crate::object_cells::RepresentationSnapshot {
            calls: snapshot.containers.calls - baseline.containers.calls,
            loads: snapshot.containers.loads - baseline.containers.loads,
            hits: snapshot.containers.hits - baseline.containers.hits,
            waits: snapshot.containers.waits - baseline.containers.waits,
            negative_hits: snapshot.containers.negative_hits - baseline.containers.negative_hits,
            transient_shares: snapshot.containers.transient_shares
                - baseline.containers.transient_shares,
            bypasses: snapshot.containers.bypasses - baseline.containers.bypasses,
            evictions: snapshot.containers.evictions - baseline.containers.evictions,
            cancellations: snapshot.containers.cancellations - baseline.containers.cancellations,
        };
        assert_eq!(actual, expected);
        assert_eq!(snapshot.raw, baseline.raw);
        assert_eq!(snapshot.members, baseline.members);
        assert_representation_counter_sums(snapshot);
    }

    fn assert_declared_container_route_neutral(adapter: &IndexedDocumentAdapter) {
        assert_eq!(adapter.reader.cache_stats(), Default::default());
        assert_eq!(adapter.reader.object_cache_stats(), Default::default());
        assert_eq!(
            adapter.reader.object_stream_cache_stats(),
            Default::default()
        );
        let counters = adapter.counters();
        assert_eq!(counters.object_resolutions.load(Ordering::Relaxed), 0);
        assert_eq!(counters.object_failures.load(Ordering::Relaxed), 0);
        assert_eq!(counters.active_resolutions.load(Ordering::Relaxed), 0);
        assert_eq!(counters.peak_active_resolutions.load(Ordering::Relaxed), 0);
        assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
    }

    fn assert_one_successful_container_preparation(
        adapter: &IndexedDocumentAdapter,
        expected_peak_permit_bytes: u64,
    ) {
        let counters = adapter.counters();
        assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 1);
        assert_eq!(
            counters
                .container_preparation_successes
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .container_preparation_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_persistent_native_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_persistent_above_cap_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_flight_only_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_exact_key_invariant_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .active_container_preparations
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .peak_active_container_preparations
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .initial_container_permit_current_bytes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .initial_container_permit_peak_bytes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters.peak_container_permit_bytes.load(Ordering::Relaxed),
            expected_peak_permit_bytes
        );
    }

    fn assert_no_o_or_completion_change(
        baseline: &crate::broker::BrokerSnapshot,
        snapshot: &crate::broker::BrokerSnapshot,
    ) {
        assert_eq!(snapshot.oversize_bytes, baseline.oversize_bytes);
        assert_eq!(snapshot.peak_oversize_bytes, baseline.peak_oversize_bytes);
        assert_eq!(
            snapshot.completion_reserve_bytes,
            baseline.completion_reserve_bytes
        );
        assert_eq!(
            snapshot.peak_completion_reserve_bytes,
            baseline.peak_completion_reserve_bytes
        );
        assert_eq!(snapshot.oversize_owners, baseline.oversize_owners);
    }

    fn assert_broker_current_equal(
        baseline: &crate::broker::BrokerSnapshot,
        snapshot: &crate::broker::BrokerSnapshot,
    ) {
        assert_eq!(snapshot.normal_limit_bytes, baseline.normal_limit_bytes);
        assert_eq!(snapshot.normal_payload_bytes, baseline.normal_payload_bytes);
        assert_eq!(
            snapshot.normal_in_flight_estimate_bytes,
            baseline.normal_in_flight_estimate_bytes
        );
        assert_eq!(snapshot.metadata_bytes, baseline.metadata_bytes);
        assert_eq!(
            snapshot.completion_reserve_bytes,
            baseline.completion_reserve_bytes
        );
        assert_eq!(snapshot.oversize_bytes, baseline.oversize_bytes);
        assert_eq!(snapshot.aggregate_bytes, baseline.aggregate_bytes);
        assert_eq!(snapshot.queued, baseline.queued);
        assert_eq!(snapshot.in_flight, baseline.in_flight);
        assert_eq!(snapshot.live_request_records, baseline.live_request_records);
        assert_eq!(snapshot.error_metadata_bytes, baseline.error_metadata_bytes);
        assert_eq!(
            snapshot.reservation_metadata_bytes,
            baseline.reservation_metadata_bytes
        );
        assert_eq!(snapshot.oversize_owners, baseline.oversize_owners);
        assert_eq!(snapshot.cache_bytes, baseline.cache_bytes);
        assert_eq!(snapshot.pin_bytes, baseline.pin_bytes);
        assert_eq!(snapshot.bypass_bytes, baseline.bypass_bytes);
        assert_eq!(snapshot.active_operations, baseline.active_operations);
        assert_eq!(snapshot.invariant_failed, baseline.invariant_failed);
        assert_eq!(snapshot.closed, baseline.closed);
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
        assert_eq!(adapter.pages().unwrap().len(), 1);
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
        let counters = adapter.counters();
        assert_eq!(
            counters
                .retained_object_admitted_bytes
                .load(Ordering::Relaxed),
            PROVISIONAL_O_BYTES
        );
        assert!(
            counters
                .retained_object_estimated_bytes
                .load(Ordering::Relaxed)
                < PROVISIONAL_O_BYTES
        );
        let concurrent = adapter.object((6, 0)).unwrap();
        assert_eq!(
            concurrent
                .read(|object| object.as_stream().unwrap().content.clone())
                .unwrap(),
            b"DATA"
        );
        drop(concurrent);
        let started = std::time::Instant::now();
        let error = adapter
            .recover_source_stream(6)
            .err()
            .expect("recovery must fail instead of blocking behind a live O owner");
        assert_eq!(error.kind, AccessKind::ResourceLimit);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(held);
        drop(flag);
        assert_eq!(
            adapter
                .counters()
                .retained_object_admitted_bytes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            adapter.recover_source_stream(6).unwrap().unwrap().as_ref(),
            b"DATA"
        );
        let stream = adapter.object((6, 0)).unwrap();
        assert_eq!(
            stream
                .read(|object| object.as_stream().unwrap().content.clone())
                .unwrap(),
            b"DATA"
        );
        drop(stream);
        assert!(adapter.object((5, 1)).is_err());
        let counters = adapter.counters();
        let cap = INDEX_FIXED_BYTES
            + INDEX_OBJECT_BYTES * counters.index_objects.load(Ordering::Relaxed)
            + INDEX_PAGE_BYTES * counters.index_pages.load(Ordering::Relaxed);
        assert!(counters.index_estimated_bytes.load(Ordering::Relaxed) <= cap);
        assert!(counters.peak_active_resolutions.load(Ordering::Relaxed) <= 1);
        assert!(counters.peak_resolution_bytes.load(Ordering::Relaxed) <= PROVISIONAL_O_BYTES);
    }

    #[test]
    fn indexed_normal_objects_single_flight_and_negative_cache_by_full_id() {
        let _test_lock = indexed_test_lock();
        let raw = indexed_metadata_fixture();
        let adapter = Arc::new(indexed(&raw, None));
        let baseline = adapter.object_cells.snapshot();
        let barrier = Arc::new(std::sync::Barrier::new(17));
        let mut joins = Vec::new();
        for _ in 0..16 {
            let adapter = Arc::clone(&adapter);
            let barrier = Arc::clone(&barrier);
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                adapter
                    .object((6, 0))
                    .unwrap()
                    .read(|object| object.as_stream().unwrap().content.clone())
                    .unwrap()
            }));
        }
        barrier.wait();
        for join in joins {
            assert_eq!(join.join().unwrap(), b"DATA");
        }
        assert_eq!(
            adapter
                .counters()
                .object_resolutions
                .load(Ordering::Relaxed),
            1
        );
        let first = adapter.object((5, 1)).err().expect("wrong generation");
        let second = adapter
            .object((5, 1))
            .err()
            .expect("cached wrong generation");
        assert_eq!(first, second);
        assert_eq!(first.object, (5, 1));
        assert_eq!(
            adapter.counters().object_failures.load(Ordering::Relaxed),
            1
        );
        let cells = adapter.object_cells.snapshot();
        assert_eq!(cells.loads - baseline.loads, 2);
        assert_eq!(cells.negative_hits - baseline.negative_hits, 1);
        assert_eq!(cells.live_interests, 0);
    }

    #[test]
    fn indexed_normal_cells_do_not_alias_across_document_epochs() {
        let _test_lock = indexed_test_lock();
        let raw = indexed_metadata_fixture();
        let first = indexed(&raw, None);
        let second = indexed(&raw, None);
        assert_ne!(first.object_cells.epoch(), second.object_cells.epoch());
        assert_eq!(
            first
                .object((6, 0))
                .unwrap()
                .read(|object| object.as_stream().unwrap().content.clone())
                .unwrap(),
            b"DATA"
        );
        assert_eq!(
            second
                .object((6, 0))
                .unwrap()
                .read(|object| object.as_stream().unwrap().content.clone())
                .unwrap(),
            b"DATA"
        );
        assert_eq!(
            first.counters().object_resolutions.load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            second.counters().object_resolutions.load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn indexed_declared_container_preparation_is_cache_neutral_and_raw_isolated() {
        let _test_lock = indexed_test_lock();
        let raw = indexed_object_stream_fixture();
        let adapter = indexed(&raw, None);
        let container = first_declared_container(&adapter);
        assert_eq!(adapter.reader.cache_stats(), Default::default());
        assert_eq!(adapter.reader.object_cache_stats(), Default::default());
        assert_eq!(
            adapter.reader.object_stream_cache_stats(),
            Default::default()
        );

        let first = match adapter.prepare_declared_object_stream(container) {
            Ok(pin) => pin,
            Err(_) => panic!("declared container preparation must succeed"),
        };
        assert_eq!(first.as_object_stream().container_id(), container);
        let second = match adapter.prepare_declared_object_stream(container) {
            Ok(pin) => pin,
            Err(_) => panic!("declared container hit must succeed"),
        };
        assert!(std::ptr::eq(
            first.as_object_stream(),
            second.as_object_stream()
        ));

        let counters = adapter.counters();
        assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 1);
        assert_eq!(
            counters
                .container_preparation_successes
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .container_preparation_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .active_container_preparations
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .peak_active_container_preparations
                .load(Ordering::Relaxed),
            1
        );
        let peak = counters.peak_container_permit_bytes.load(Ordering::Relaxed);
        assert!(peak > 0);
        assert!(peak <= INDEXED_STREAM_BYTES);
        assert_eq!(counters.object_resolutions.load(Ordering::Relaxed), 0);
        assert_eq!(counters.object_failures.load(Ordering::Relaxed), 0);
        assert_eq!(counters.active_resolutions.load(Ordering::Relaxed), 0);
        assert_eq!(counters.peak_active_resolutions.load(Ordering::Relaxed), 0);
        assert_eq!(
            counters
                .retained_object_admitted_bytes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(adapter.reader.cache_stats(), Default::default());
        assert_eq!(adapter.reader.object_cache_stats(), Default::default());
        assert_eq!(
            adapter.reader.object_stream_cache_stats(),
            Default::default()
        );
    }

    #[test]
    fn indexed_declared_container_single_flight_diagnostics_are_exact() {
        let _test_lock = indexed_test_lock();
        let generated = AccessGeneratedDir::new("positive");
        generate_objstm_fixtures(generated.path(), &["--profile", "objstm-container"]);
        let committed_r4 = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures_pdf/objstm/objstm-r4-rc4.pdf"
        );
        let cases = [
            ("plain", generated.path().join("objstm-plain.pdf")),
            ("Flate", generated.path().join("objstm-flate.pdf")),
            ("R4 RC4", PathBuf::from(committed_r4)),
        ];
        for (case, path) in cases {
            let raw: Arc<[u8]> = Arc::from(std::fs::read(path).unwrap());
            for callers in [2_usize, 4, 16] {
                let broker_baseline = crate::broker::BudgetBroker::production().snapshot();
                let (source, control) = blocking_objstm_source(Arc::clone(&raw));
                let adapter = Arc::new(IndexedDocumentAdapter::open(source, None).unwrap());
                let container = first_declared_container(&adapter);
                let waiters = ObjStmWaitBarrier::new(container, callers - 1);
                adapter.object_cells.set_wait_hooks(waiters.clone());
                let baseline = adapter.object_cells.snapshot();
                let broker_open = crate::broker::BudgetBroker::production().snapshot();
                assert_eq!(
                    broker_open.active_operations,
                    broker_baseline.active_operations + 1
                );
                assert_no_o_or_completion_change(&broker_baseline, &broker_open);
                assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
                control.arm(ObjStmSourceFailure::None);

                let leader_adapter = Arc::clone(&adapter);
                let leader = std::thread::spawn(move || {
                    leader_adapter.prepare_declared_object_stream(container)
                });
                control.wait_until_blocked();
                let broker_blocked = crate::broker::BudgetBroker::production().snapshot();
                let blocked_cells = adapter.object_cells.snapshot();
                let loading_metadata = blocked_cells.cache_bytes - baseline.cache_bytes;
                assert_eq!(
                    broker_blocked.normal_in_flight_estimate_bytes,
                    broker_open.normal_in_flight_estimate_bytes + INDEXED_STREAM_BYTES,
                    "{case}, {callers} callers"
                );
                assert_eq!(
                    broker_blocked.normal_payload_bytes,
                    broker_open.normal_payload_bytes + loading_metadata + INDEXED_STREAM_BYTES,
                    "{case}, {callers} callers"
                );
                assert_eq!(
                    broker_blocked.metadata_bytes,
                    broker_open.metadata_bytes + crate::broker::QUEUE_METADATA_WEIGHT
                );
                assert_eq!(
                    broker_blocked.completion_reserve_bytes,
                    broker_open.completion_reserve_bytes
                );
                assert_eq!(broker_blocked.oversize_bytes, broker_open.oversize_bytes);
                assert_eq!(
                    broker_blocked.aggregate_bytes,
                    broker_open.aggregate_bytes
                        + loading_metadata
                        + INDEXED_STREAM_BYTES
                        + crate::broker::QUEUE_METADATA_WEIGHT
                );
                assert_eq!(
                    broker_blocked.cache_bytes,
                    broker_open.cache_bytes + loading_metadata
                );
                assert_eq!(broker_blocked.pin_bytes, broker_open.pin_bytes);
                assert_eq!(broker_blocked.bypass_bytes, broker_open.bypass_bytes);
                assert_eq!(
                    broker_blocked.peak_normal_bytes,
                    broker_open
                        .peak_normal_bytes
                        .max(broker_blocked.normal_payload_bytes + broker_blocked.metadata_bytes)
                );
                assert_eq!(
                    broker_blocked.peak_aggregate_bytes,
                    broker_open
                        .peak_aggregate_bytes
                        .max(broker_blocked.aggregate_bytes)
                );
                assert_eq!(
                    broker_blocked.peak_cache_bytes,
                    broker_open.peak_cache_bytes.max(broker_blocked.cache_bytes)
                );
                assert_eq!(broker_blocked.peak_pin_bytes, broker_open.peak_pin_bytes);
                assert_eq!(
                    broker_blocked.peak_bypass_bytes,
                    broker_open
                        .peak_bypass_bytes
                        .max(broker_open.bypass_bytes + loading_metadata)
                );
                let fresh_permit = adapter
                    .object_cells
                    .active_container_permit_stats(container)
                    .expect("blocked leader owns a fresh container permit");
                assert_eq!(
                    adapter
                        .counters()
                        .initial_container_permit_current_bytes
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(
                    adapter
                        .counters()
                        .initial_container_permit_peak_bytes
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(fresh_permit.limit_bytes, INDEXED_STREAM_BYTES);
                assert_eq!(fresh_permit.current_bytes, 256);
                assert_eq!(fresh_permit.peak_bytes, 256);
                assert_eq!(fresh_permit.reservations, 1);
                assert!(!fresh_permit.cancelled);
                assert!(!fresh_permit.closed);
                assert_no_o_or_completion_change(&broker_open, &broker_blocked);
                assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);

                let mut followers = Vec::new();
                for _ in 1..callers {
                    let adapter = Arc::clone(&adapter);
                    followers.push(std::thread::spawn(move || {
                        adapter.prepare_declared_object_stream(container)
                    }));
                }
                waiters.wait_until_attached();
                control.release();

                let mut pins = Vec::new();
                pins.push(match leader.join().unwrap() {
                    Ok(pin) => pin,
                    Err(_) => panic!("elected preparation must succeed"),
                });
                for follower in followers {
                    pins.push(match follower.join().unwrap() {
                        Ok(pin) => pin,
                        Err(_) => panic!("attached container waiter must succeed"),
                    });
                }
                let owner = pins[0].as_object_stream() as *const _;
                assert!(pins
                    .iter()
                    .all(|pin| std::ptr::eq(owner, pin.as_object_stream())));

                let after_flight = adapter.object_cells.snapshot();
                assert_eq!(
                    after_flight.containers.calls - baseline.containers.calls,
                    callers as u64,
                    "{case}, {callers} callers"
                );
                assert_eq!(after_flight.containers.loads - baseline.containers.loads, 1);
                assert_eq!(after_flight.containers.hits - baseline.containers.hits, 0);
                assert_eq!(
                    after_flight.containers.waits - baseline.containers.waits,
                    (callers - 1) as u64
                );
                assert_eq!(
                    after_flight.containers.negative_hits - baseline.containers.negative_hits,
                    0
                );
                assert_eq!(
                    after_flight.containers.transient_shares - baseline.containers.transient_shares,
                    0
                );
                assert_eq!(
                    after_flight.containers.bypasses - baseline.containers.bypasses,
                    0
                );
                assert_eq!(
                    after_flight.containers.evictions - baseline.containers.evictions,
                    0
                );
                assert_eq!(
                    after_flight.containers.cancellations - baseline.containers.cancellations,
                    0
                );
                assert_member_counters_zero(&after_flight);
                assert_representation_counter_sums(&after_flight);
                let broker_published = crate::broker::BudgetBroker::production().snapshot();
                let (retained, retained_permit, broker_charge) = adapter
                    .object_cells
                    .container_retained_evidence(container)
                    .expect("published container retains its exact permit and charge");
                let retained_stats = retained_permit.stats();
                assert_eq!(retained_stats.current_bytes, retained);
                assert_eq!(broker_charge, retained);
                assert_eq!(
                    broker_published.normal_in_flight_estimate_bytes,
                    broker_open.normal_in_flight_estimate_bytes
                );
                let published_weight = after_flight.cache_bytes - baseline.cache_bytes;
                assert_eq!(
                    broker_published.normal_payload_bytes,
                    broker_open.normal_payload_bytes + published_weight
                );
                assert_eq!(broker_published.metadata_bytes, broker_open.metadata_bytes);
                assert_eq!(
                    broker_published.completion_reserve_bytes,
                    broker_open.completion_reserve_bytes
                );
                assert_eq!(broker_published.oversize_bytes, broker_open.oversize_bytes);
                assert_eq!(
                    broker_published.aggregate_bytes,
                    broker_open.aggregate_bytes + published_weight
                );
                assert_eq!(
                    broker_published.cache_bytes,
                    broker_open.cache_bytes + published_weight - broker_charge
                );
                assert_eq!(
                    broker_published.pin_bytes,
                    broker_open.pin_bytes + broker_charge
                );
                assert_eq!(broker_published.bypass_bytes, broker_open.bypass_bytes);
                assert_eq!(
                    broker_published.peak_normal_bytes,
                    broker_blocked.peak_normal_bytes
                );
                assert_eq!(
                    broker_published.peak_aggregate_bytes,
                    broker_blocked.peak_aggregate_bytes
                );
                assert_eq!(
                    broker_published.peak_cache_bytes,
                    broker_blocked
                        .peak_cache_bytes
                        .max(broker_open.cache_bytes + published_weight)
                );
                assert_eq!(
                    broker_published.peak_pin_bytes,
                    broker_open.peak_pin_bytes.max(broker_published.pin_bytes)
                );
                assert_eq!(
                    broker_published.peak_bypass_bytes,
                    broker_blocked.peak_bypass_bytes
                );
                assert_no_o_or_completion_change(&broker_open, &broker_published);
                let reads_after_flight = control.reads();
                let hit = match adapter.prepare_declared_object_stream(container) {
                    Ok(pin) => pin,
                    Err(_) => panic!("sequential container hit must succeed"),
                };
                assert!(std::ptr::eq(owner, hit.as_object_stream()));
                assert_eq!(control.reads(), reads_after_flight);

                let final_snapshot = adapter.object_cells.snapshot();
                assert_eq!(
                    final_snapshot.containers.calls - baseline.containers.calls,
                    callers as u64 + 1
                );
                assert_eq!(
                    final_snapshot.containers.loads - baseline.containers.loads,
                    1
                );
                assert_eq!(final_snapshot.containers.hits - baseline.containers.hits, 1);
                assert_eq!(
                    final_snapshot.containers.waits - baseline.containers.waits,
                    (callers - 1) as u64
                );
                assert_eq!(
                    final_snapshot.containers.negative_hits - baseline.containers.negative_hits,
                    0
                );
                assert_eq!(
                    final_snapshot.containers.transient_shares
                        - baseline.containers.transient_shares,
                    0
                );
                assert_eq!(
                    final_snapshot.containers.bypasses - baseline.containers.bypasses,
                    0
                );
                assert_eq!(
                    final_snapshot.containers.evictions - baseline.containers.evictions,
                    0
                );
                assert_eq!(
                    final_snapshot.containers.cancellations - baseline.containers.cancellations,
                    0
                );
                assert_member_counters_zero(&final_snapshot);
                assert_representation_counter_sums(&final_snapshot);
                let counters = adapter.counters();
                assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 1);
                assert_eq!(
                    counters
                        .container_preparation_successes
                        .load(Ordering::Relaxed),
                    1
                );
                assert_eq!(
                    counters
                        .container_preparation_failures
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(
                    counters
                        .active_container_preparations
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(
                    counters
                        .peak_active_container_preparations
                        .load(Ordering::Relaxed),
                    1
                );
                assert_eq!(counters.object_resolutions.load(Ordering::Relaxed), 0);
                assert_eq!(counters.object_failures.load(Ordering::Relaxed), 0);
                assert_eq!(adapter.reader.cache_stats(), Default::default());
                assert_eq!(adapter.reader.object_cache_stats(), Default::default());
                assert_eq!(
                    adapter.reader.object_stream_cache_stats(),
                    Default::default()
                );
                assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
                drop(hit);
                drop(pins);
                drop(adapter);
                assert_eq!(retained_permit.stats().current_bytes, 0);
                assert_eq!(
                    retained_permit.stats().peak_bytes,
                    retained_stats.peak_bytes
                );
                let broker_drained = crate::broker::BudgetBroker::production().snapshot();
                assert_broker_current_equal(&broker_baseline, &broker_drained);
                assert_eq!(
                    broker_drained.aggregate_bytes,
                    broker_baseline.aggregate_bytes
                );
                assert_eq!(
                    broker_drained.normal_payload_bytes,
                    broker_baseline.normal_payload_bytes
                );
                assert_eq!(
                    broker_drained.normal_in_flight_estimate_bytes,
                    broker_baseline.normal_in_flight_estimate_bytes
                );
                assert_eq!(
                    broker_drained.metadata_bytes,
                    broker_baseline.metadata_bytes
                );
                assert_eq!(broker_drained.cache_bytes, broker_baseline.cache_bytes);
                assert_eq!(broker_drained.pin_bytes, broker_baseline.pin_bytes);
                assert_eq!(broker_drained.bypass_bytes, broker_baseline.bypass_bytes);
                assert_eq!(
                    broker_drained.active_operations,
                    broker_baseline.active_operations
                );
                assert_eq!(broker_drained.queued, broker_baseline.queued);
                assert_eq!(broker_drained.in_flight, broker_baseline.in_flight);
                assert_no_o_or_completion_change(&broker_baseline, &broker_drained);
                assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
            }
        }
    }

    #[test]
    fn indexed_declared_container_prepares_committed_r4_rc4_fixture() {
        let _test_lock = indexed_test_lock();
        let raw = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures_pdf/objstm/objstm-r4-rc4.pdf"
        ))
        .unwrap();
        let adapter = indexed(&raw, None);
        assert_eq!(
            adapter.reader.object_location((3, 0)),
            Some(IndexedObjectLocation::Compressed {
                container: (2, 0),
                index: 0,
            })
        );
        let pin = match adapter.prepare_declared_object_stream((2, 0)) {
            Ok(pin) => pin,
            Err(_) => panic!("committed encrypted container must prepare"),
        };
        assert_eq!(pin.as_object_stream().container_id(), (2, 0));

        let counters = adapter.counters();
        assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 1);
        assert_eq!(
            counters
                .container_preparation_successes
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .container_preparation_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(counters.object_resolutions.load(Ordering::Relaxed), 0);
        assert_eq!(counters.object_failures.load(Ordering::Relaxed), 0);
        assert_eq!(adapter.reader.cache_stats(), Default::default());
        assert_eq!(adapter.reader.object_cache_stats(), Default::default());
        assert_eq!(
            adapter.reader.object_stream_cache_stats(),
            Default::default()
        );
    }

    #[test]
    fn real_container_cache_boundary_is_cache_below_and_bypass_above() {
        const CACHE_TARGET_BYTES: u64 = 32 * 1024 * 1024;
        const BELOW_BYTES: usize = 33_424_113;
        const BELOW_SHA256: &str =
            "c3f723fdae1022e20240309f01d1de77460debc3f2d8cd15aee576cfb2e82d6c";
        const BELOW_RETAINED: u64 = 33_423_376;
        const BELOW_PEAK: u64 = 33_424_552;
        const ABOVE_BYTES: usize = 33_686_257;
        const ABOVE_SHA256: &str =
            "aa4206ba1fba2505b81798098980921344dd407018f469639cda8d22132a99b7";
        const ABOVE_RETAINED: u64 = 33_685_520;
        const ABOVE_PEAK: u64 = 33_686_696;

        let _test_lock = indexed_test_lock();
        for (variant, expected_bytes, expected_sha, expected_retained, expected_peak) in [
            (
                "plain-cache-below",
                BELOW_BYTES,
                BELOW_SHA256,
                BELOW_RETAINED,
                BELOW_PEAK,
            ),
            (
                "plain-cache-above",
                ABOVE_BYTES,
                ABOVE_SHA256,
                ABOVE_RETAINED,
                ABOVE_PEAK,
            ),
        ] {
            let generated = AccessGeneratedDir::new(variant);
            generate_objstm_fixtures(
                generated.path(),
                &["--profile", "objstm-boundary", "--objstm-boundary", variant],
            );
            let raw =
                std::fs::read(generated.path().join(format!("objstm-{variant}.pdf"))).unwrap();
            assert_eq!(raw.len(), expected_bytes);
            assert_eq!(format!("{:x}", Sha256::digest(&raw)), expected_sha);

            let raw: Arc<[u8]> = Arc::from(raw);
            let broker_baseline = crate::broker::BudgetBroker::production().snapshot();
            let (source, control) = blocking_objstm_source(Arc::clone(&raw));
            let adapter = Arc::new(IndexedDocumentAdapter::open(source, None).unwrap());
            let container = (6, 0);
            assert_eq!(first_declared_container(&adapter), container);
            let epoch = adapter.object_cells.epoch();
            let cell_baseline = adapter.object_cells.snapshot();
            let broker_open = crate::broker::BudgetBroker::production().snapshot();
            assert_eq!(
                broker_open.active_operations,
                broker_baseline.active_operations + 1
            );
            assert!(broker_open.operations.contains_key(&epoch));
            assert_no_o_or_completion_change(&broker_baseline, &broker_open);

            if variant == "plain-cache-below" {
                let first = adapter.prepare_declared_object_stream(container).unwrap();
                let first_pointer = first.as_object_stream() as *const _;
                let (retained, retained_permit, charge) = first.retained_evidence();
                let retained_stats = retained_permit.stats();
                assert_eq!(retained, expected_retained);
                assert_eq!(charge, expected_retained);
                assert_eq!(retained_stats.current_bytes, expected_retained);
                assert_eq!(retained_stats.peak_bytes, expected_peak);
                assert_eq!(retained_stats.limit_bytes, INDEXED_STREAM_BYTES);
                assert!(expected_peak < INDEXED_STREAM_BYTES);

                let published = adapter.object_cells.snapshot();
                let complete_weight = published.cache_bytes - cell_baseline.cache_bytes;
                assert!(complete_weight > expected_retained);
                assert!(complete_weight <= CACHE_TARGET_BYTES);
                assert_eq!(published.cells, cell_baseline.cells + 1);
                assert_eq!(published.ready, cell_baseline.ready + 1);
                assert_container_delta(
                    &cell_baseline,
                    &published,
                    crate::object_cells::RepresentationSnapshot {
                        calls: 1,
                        loads: 1,
                        ..Default::default()
                    },
                );
                let published_broker = crate::broker::BudgetBroker::production().snapshot();
                let open_operation = &broker_open.operations[&epoch];
                let published_operation = &published_broker.operations[&epoch];
                assert_eq!(published_operation.pin_bytes, expected_retained);
                assert_eq!(published_operation.bypass_bytes, 0);
                assert_eq!(published_operation.self_pinned_bytes, expected_retained);
                assert_eq!(
                    published_operation.cache_bytes,
                    open_operation.cache_bytes + complete_weight - expected_retained
                );
                assert_eq!(
                    published_broker.normal_payload_bytes,
                    broker_open.normal_payload_bytes + complete_weight
                );
                assert_eq!(
                    published_broker.cache_bytes,
                    broker_open.cache_bytes + complete_weight - expected_retained
                );
                assert_eq!(
                    published_broker.pin_bytes,
                    broker_open.pin_bytes + expected_retained
                );
                assert_eq!(published_broker.bypass_bytes, broker_open.bypass_bytes);
                assert_no_o_or_completion_change(&broker_open, &published_broker);
                assert_declared_container_route_neutral(&adapter);

                drop(first);
                assert_eq!(retained_permit.stats().current_bytes, expected_retained);
                let reads_before_hit = control.reads();
                let hit = adapter.prepare_declared_object_stream(container).unwrap();
                assert!(std::ptr::eq(first_pointer, hit.as_object_stream()));
                assert_eq!(control.reads(), reads_before_hit);
                let hit_snapshot = adapter.object_cells.snapshot();
                assert_container_delta(
                    &cell_baseline,
                    &hit_snapshot,
                    crate::object_cells::RepresentationSnapshot {
                        calls: 2,
                        loads: 1,
                        hits: 1,
                        ..Default::default()
                    },
                );
                assert_eq!(
                    adapter
                        .counters()
                        .peak_container_permit_bytes
                        .load(Ordering::Relaxed),
                    expected_peak
                );
                assert_declared_container_route_neutral(&adapter);
                drop(hit);
                drop(adapter);
                assert_eq!(retained_permit.stats().current_bytes, 0);
            } else {
                assert!(expected_retained > CACHE_TARGET_BYTES);
                assert!(expected_retained < INDEXED_STREAM_BYTES);
                let waiters = ObjStmWaitBarrier::new(container, 1);
                adapter.object_cells.set_wait_hooks(waiters.clone());
                control.arm(ObjStmSourceFailure::None);
                let leader_adapter = Arc::clone(&adapter);
                let leader = std::thread::spawn(move || {
                    leader_adapter.prepare_declared_object_stream(container)
                });
                control.wait_until_blocked();
                let follower_adapter = Arc::clone(&adapter);
                let follower = std::thread::spawn(move || {
                    follower_adapter.prepare_declared_object_stream(container)
                });
                waiters.wait_until_attached();
                control.release();
                let leader_pin = leader.join().unwrap().unwrap();
                let follower_pin = follower.join().unwrap().unwrap();
                waiters.assert_balanced();
                assert!(std::ptr::eq(
                    leader_pin.as_object_stream(),
                    follower_pin.as_object_stream()
                ));
                let (retained, retained_permit, charge) = leader_pin.retained_evidence();
                assert_eq!(retained, expected_retained);
                assert_eq!(charge, expected_retained);
                assert_eq!(retained_permit.stats().current_bytes, expected_retained);
                assert_eq!(retained_permit.stats().peak_bytes, expected_peak);
                assert_eq!(retained_permit.stats().limit_bytes, INDEXED_STREAM_BYTES);

                let first_flight = adapter.object_cells.snapshot();
                assert!(!adapter.object_cells.has_container_cell(container));
                assert_eq!(first_flight.cells, cell_baseline.cells);
                assert_eq!(first_flight.cache_bytes, cell_baseline.cache_bytes);
                assert_container_delta(
                    &cell_baseline,
                    &first_flight,
                    crate::object_cells::RepresentationSnapshot {
                        calls: 2,
                        loads: 1,
                        waits: 1,
                        bypasses: 1,
                        ..Default::default()
                    },
                );
                let bypassed = crate::broker::BudgetBroker::production().snapshot();
                let bypass_operation = &bypassed.operations[&epoch];
                assert_eq!(
                    bypass_operation.cache_bytes,
                    broker_open.operations[&epoch].cache_bytes
                );
                assert_eq!(bypass_operation.pin_bytes, 0);
                assert_eq!(bypass_operation.bypass_bytes, expected_retained);
                assert_eq!(bypass_operation.self_pinned_bytes, expected_retained);
                assert_eq!(bypassed.cache_bytes, broker_open.cache_bytes);
                assert_eq!(bypassed.pin_bytes, broker_open.pin_bytes);
                assert_eq!(
                    bypassed.bypass_bytes,
                    broker_open.bypass_bytes + expected_retained
                );
                assert_eq!(
                    bypassed.normal_payload_bytes,
                    broker_open.normal_payload_bytes + expected_retained
                );
                assert_no_o_or_completion_change(&broker_open, &bypassed);
                assert_declared_container_route_neutral(&adapter);

                drop(follower_pin);
                drop(leader_pin);
                assert_eq!(retained_permit.stats().current_bytes, 0);
                let released = crate::broker::BudgetBroker::production().snapshot();
                assert_broker_current_equal(&broker_open, &released);
                assert!(!adapter.object_cells.has_container_cell(container));

                let reload = adapter.prepare_declared_object_stream(container).unwrap();
                let (_, reload_permit, reload_charge) = reload.retained_evidence();
                assert_eq!(retained_permit.stats().current_bytes, 0);
                assert_eq!(reload_charge, expected_retained);
                assert_eq!(reload_permit.stats().current_bytes, expected_retained);
                assert_eq!(reload_permit.stats().peak_bytes, expected_peak);
                let reloaded = adapter.object_cells.snapshot();
                assert!(!adapter.object_cells.has_container_cell(container));
                assert_container_delta(
                    &cell_baseline,
                    &reloaded,
                    crate::object_cells::RepresentationSnapshot {
                        calls: 3,
                        loads: 2,
                        waits: 1,
                        bypasses: 2,
                        ..Default::default()
                    },
                );
                assert_eq!(
                    adapter
                        .counters()
                        .peak_container_permit_bytes
                        .load(Ordering::Relaxed),
                    expected_peak
                );
                assert_eq!(
                    adapter
                        .counters()
                        .container_preparations
                        .load(Ordering::Relaxed),
                    2
                );
                assert_eq!(
                    adapter
                        .counters()
                        .container_preparation_successes
                        .load(Ordering::Relaxed),
                    2
                );
                assert_declared_container_route_neutral(&adapter);
                drop(reload);
                assert_eq!(reload_permit.stats().current_bytes, 0);
                assert_broker_current_equal(
                    &broker_open,
                    &crate::broker::BudgetBroker::production().snapshot(),
                );
                drop(adapter);
            }

            assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
            let drained = crate::broker::BudgetBroker::production().snapshot();
            assert_broker_current_equal(&broker_baseline, &drained);
            assert_no_o_or_completion_change(&broker_baseline, &drained);
        }
    }

    #[test]
    fn committed_r4_container_owners_are_epoch_and_close_isolated() {
        const R4_RETAINED: u64 = 428;
        const R4_PEAK: u64 = 64 * 1024 * 1024;

        let _test_lock = indexed_test_lock();
        let raw: Arc<[u8]> = Arc::from(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../tests/fixtures_pdf/objstm/objstm-r4-rc4.pdf"
            ))
            .unwrap(),
        );
        let broker_baseline = crate::broker::BudgetBroker::production().snapshot();
        let first_source: Arc<dyn RandomAccessSource> =
            Arc::new(BytesSource::new(Arc::clone(&raw)));
        let second_source: Arc<dyn RandomAccessSource> =
            Arc::new(BytesSource::new(Arc::clone(&raw)));
        let first = IndexedDocumentAdapter::open(first_source, None).unwrap();
        let second = IndexedDocumentAdapter::open(second_source, None).unwrap();
        let first_epoch = first.object_cells.epoch();
        let second_epoch = second.object_cells.epoch();
        assert_ne!(first_epoch, second_epoch);
        let cell_baseline = first.object_cells.snapshot();

        let first_pin = first.prepare_declared_object_stream((2, 0)).unwrap();
        let second_pin = second.prepare_declared_object_stream((2, 0)).unwrap();
        assert_eq!(first_pin.as_object_stream().container_id(), (2, 0));
        assert_eq!(second_pin.as_object_stream().container_id(), (2, 0));
        assert!(!std::ptr::eq(
            first_pin.as_object_stream(),
            second_pin.as_object_stream()
        ));
        let first_member = first_pin
            .as_object_stream()
            .resolve_member((3, 0), 0)
            .unwrap();
        let second_member = second_pin
            .as_object_stream()
            .resolve_member((3, 0), 0)
            .unwrap();
        let expected_member = Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference((4, 0))],
            "Count" => 1,
        });
        assert_eq!(first_member.as_object(), &expected_member);
        assert_eq!(second_member.as_object(), &expected_member);
        drop(first_member);
        drop(second_member);

        let (first_retained, first_permit, first_charge) = first_pin.retained_evidence();
        let (second_retained, second_permit, second_charge) = second_pin.retained_evidence();
        assert_eq!((first_retained, first_charge), (R4_RETAINED, R4_RETAINED));
        assert_eq!((second_retained, second_charge), (R4_RETAINED, R4_RETAINED));
        assert_eq!(first_permit.stats().current_bytes, R4_RETAINED);
        assert_eq!(second_permit.stats().current_bytes, R4_RETAINED);
        assert_eq!(first_permit.stats().peak_bytes, R4_PEAK);
        assert_eq!(second_permit.stats().peak_bytes, R4_PEAK);

        let published = crate::broker::BudgetBroker::production().snapshot();
        assert_eq!(
            published.active_operations,
            broker_baseline.active_operations + 2
        );
        let first_operation = published.operations[&first_epoch].clone();
        let second_operation = published.operations[&second_epoch].clone();
        assert_eq!(first_operation.pin_bytes, R4_RETAINED);
        assert_eq!(second_operation.pin_bytes, R4_RETAINED);
        assert_eq!(first_operation.bypass_bytes, 0);
        assert_eq!(second_operation.bypass_bytes, 0);
        assert_eq!(first_operation.self_pinned_bytes, R4_RETAINED);
        assert_eq!(second_operation.self_pinned_bytes, R4_RETAINED);
        assert_eq!(first_operation, second_operation);
        assert_no_o_or_completion_change(&broker_baseline, &published);
        assert_one_successful_container_preparation(&first, R4_PEAK);
        assert_one_successful_container_preparation(&second, R4_PEAK);
        assert_declared_container_route_neutral(&first);
        assert_declared_container_route_neutral(&second);

        drop(first);
        assert_eq!(first_pin.as_object_stream().container_id(), (2, 0));
        assert_eq!(second_pin.as_object_stream().container_id(), (2, 0));
        let after_first_close = crate::broker::BudgetBroker::production().snapshot();
        assert_eq!(
            after_first_close.operations[&first_epoch].pin_bytes,
            R4_RETAINED
        );
        assert_eq!(after_first_close.operations[&first_epoch].cache_bytes, 0);
        assert_eq!(
            after_first_close.operations[&second_epoch],
            second_operation
        );
        assert_eq!(second_permit.stats().current_bytes, R4_RETAINED);

        drop(first_pin);
        assert_eq!(first_permit.stats().current_bytes, 0);
        let after_first_owner = crate::broker::BudgetBroker::production().snapshot();
        assert!(!after_first_owner.operations.contains_key(&first_epoch));
        assert_eq!(
            after_first_owner.operations[&second_epoch],
            second_operation
        );
        assert_eq!(
            after_first_owner.active_operations,
            broker_baseline.active_operations + 1
        );

        let second_pointer = second_pin.as_object_stream() as *const _;
        let second_hit = second.prepare_declared_object_stream((2, 0)).unwrap();
        assert!(std::ptr::eq(second_pointer, second_hit.as_object_stream()));
        let final_cells = second.object_cells.snapshot();
        assert_container_delta(
            &cell_baseline,
            &final_cells,
            crate::object_cells::RepresentationSnapshot {
                calls: 3,
                loads: 2,
                hits: 1,
                ..Default::default()
            },
        );
        assert_one_successful_container_preparation(&second, R4_PEAK);
        assert_declared_container_route_neutral(&second);

        drop(second_hit);
        drop(second_pin);
        assert_eq!(second_permit.stats().current_bytes, R4_RETAINED);
        drop(second);
        assert_eq!(second_permit.stats().current_bytes, 0);
        let drained = crate::broker::BudgetBroker::production().snapshot();
        assert_broker_current_equal(&broker_baseline, &drained);
        assert_no_o_or_completion_change(&broker_baseline, &drained);
        assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
    }

    #[test]
    fn declared_container_source_read_close_rejects_stale_publication_exactly() {
        let _test_lock = indexed_test_lock();
        let raw: Arc<[u8]> = Arc::from(indexed_object_stream_fixture());
        let broker_baseline = crate::broker::BudgetBroker::production().snapshot();
        let (source, control) = blocking_objstm_source(raw);
        let adapter = Arc::new(IndexedDocumentAdapter::open(source, None).unwrap());
        let container = first_declared_container(&adapter);
        let baseline = adapter.object_cells.snapshot();
        let broker_open = crate::broker::BudgetBroker::production().snapshot();
        control.arm(ObjStmSourceFailure::None);

        let leader_adapter = Arc::clone(&adapter);
        let leader =
            std::thread::spawn(move || leader_adapter.prepare_declared_object_stream(container));
        control.wait_until_blocked();
        let blocked_cells = adapter.object_cells.snapshot();
        let blocked_broker = crate::broker::BudgetBroker::production().snapshot();
        let blocked_permit = adapter
            .object_cells
            .active_container_permit_stats(container)
            .expect("source-read leader owns its container permit");
        let blocked_counters = adapter.counters();
        let blocked_counter_values = (
            blocked_counters
                .container_preparations
                .load(Ordering::Relaxed),
            blocked_counters
                .container_preparation_successes
                .load(Ordering::Relaxed),
            blocked_counters
                .container_preparation_failures
                .load(Ordering::Relaxed),
            blocked_counters
                .active_container_preparations
                .load(Ordering::Relaxed),
            blocked_counters
                .peak_active_container_preparations
                .load(Ordering::Relaxed),
        );

        adapter.object_cells.close();
        let closed_cells = adapter.object_cells.snapshot();
        let broker_closed = crate::broker::BudgetBroker::production().snapshot();
        control.release();
        let result = leader.join().unwrap();
        let error = result.unwrap_err().into_access_for_test();
        let terminal = adapter.object_cells.snapshot();

        assert_container_delta(
            &baseline,
            &blocked_cells,
            crate::object_cells::RepresentationSnapshot {
                calls: 1,
                loads: 1,
                hits: 0,
                waits: 0,
                negative_hits: 0,
                transient_shares: 0,
                bypasses: 0,
                evictions: 0,
                cancellations: 0,
            },
        );
        assert_eq!(blocked_cells.loading, baseline.loading + 1);
        assert_eq!(blocked_cells.cells, baseline.cells + 1);
        assert_eq!(blocked_cells.live_interests, baseline.live_interests + 1);
        assert_eq!(blocked_counter_values, (1, 0, 0, 1, 1));
        assert_eq!(blocked_permit.limit_bytes, INDEXED_STREAM_BYTES);
        assert_eq!(blocked_permit.current_bytes, 256);
        assert_eq!(blocked_permit.peak_bytes, 256);
        assert_eq!(blocked_permit.reservations, 1);
        assert!(!blocked_permit.cancelled);
        assert!(!blocked_permit.closed);
        assert_eq!(
            blocked_broker.normal_in_flight_estimate_bytes,
            broker_open.normal_in_flight_estimate_bytes + INDEXED_STREAM_BYTES
        );

        assert_eq!(closed_cells.cells, baseline.cells);
        assert_eq!(closed_cells.loading, baseline.loading);
        assert_eq!(closed_cells.ready, baseline.ready);
        assert_eq!(closed_cells.negative, baseline.negative);
        assert_eq!(closed_cells.live_interests, baseline.live_interests + 1);
        assert_eq!(closed_cells.cache_bytes, baseline.cache_bytes);
        assert_eq!(closed_cells.raw, baseline.raw);
        assert_eq!(closed_cells.containers, blocked_cells.containers);
        assert_eq!(closed_cells.members, baseline.members);
        assert_eq!(closed_cells.closes, baseline.closes + 1);
        assert_eq!(closed_cells.arenas + 1, baseline.arenas);
        assert_representation_counter_sums(&closed_cells);
        assert_eq!(
            broker_closed.normal_payload_bytes,
            broker_baseline.normal_payload_bytes + INDEXED_STREAM_BYTES
        );
        assert_eq!(
            broker_closed.normal_in_flight_estimate_bytes,
            broker_baseline.normal_in_flight_estimate_bytes + INDEXED_STREAM_BYTES
        );
        assert_eq!(broker_closed.cache_bytes, broker_baseline.cache_bytes);
        assert_no_o_or_completion_change(&broker_baseline, &broker_closed);

        assert_eq!(error.kind, AccessKind::Backend);
        assert_eq!(error.detail, "object cell arena is closed");
        assert_eq!(terminal.cells, baseline.cells);
        assert_eq!(terminal.loading, baseline.loading);
        assert_eq!(terminal.ready, baseline.ready);
        assert_eq!(terminal.negative, baseline.negative);
        assert_eq!(terminal.live_interests, baseline.live_interests);
        assert_eq!(terminal.cache_bytes, baseline.cache_bytes);
        assert_eq!(terminal.raw, baseline.raw);
        assert_eq!(terminal.containers, blocked_cells.containers);
        assert_eq!(terminal.members, baseline.members);
        assert_eq!(terminal.closes, baseline.closes + 1);
        assert_representation_counter_sums(&terminal);

        let counters = adapter.counters();
        assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 1);
        assert_eq!(
            counters
                .container_preparation_successes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_preparation_failures
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .container_persistent_native_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_persistent_above_cap_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_flight_only_failures
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .container_exact_key_invariant_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .active_container_preparations
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .peak_active_container_preparations
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .initial_container_permit_current_bytes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .initial_container_permit_peak_bytes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters.peak_container_permit_bytes.load(Ordering::Relaxed),
            256
        );
        assert_declared_container_route_neutral(&adapter);
        assert!(adapter
            .object_cells
            .active_container_permit_stats(container)
            .is_none());
        let drained = crate::broker::BudgetBroker::production().snapshot();
        assert_broker_current_equal(&broker_baseline, &drained);
        assert_no_o_or_completion_change(&broker_baseline, &drained);
        drop(adapter);
        assert_broker_current_equal(
            &broker_baseline,
            &crate::broker::BudgetBroker::production().snapshot(),
        );
    }

    #[test]
    fn indexed_declared_containers_keep_two_keys_and_xref_authority_distinct() {
        let _test_lock = indexed_test_lock();
        let generated = AccessGeneratedDir::new("authority");
        generate_objstm_fixtures(generated.path(), &["--profile", "objstm-container"]);

        let two_raw = std::fs::read(generated.path().join("objstm-two-containers.pdf")).unwrap();
        let two = indexed(&two_raw, None);
        let first = two.prepare_declared_object_stream((6, 0)).unwrap();
        let second = two.prepare_declared_object_stream((9, 0)).unwrap();
        assert_eq!(first.as_object_stream().container_id(), (6, 0));
        assert_eq!(second.as_object_stream().container_id(), (9, 0));
        assert!(!std::ptr::eq(
            first.as_object_stream(),
            second.as_object_stream()
        ));
        assert_eq!(
            two.counters()
                .container_preparations
                .load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            two.counters()
                .container_preparation_successes
                .load(Ordering::Relaxed),
            2
        );
        let two_snapshot = two.object_cells.snapshot();
        assert_member_counters_zero(&two_snapshot);
        assert_representation_counter_sums(&two_snapshot);
        assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);

        let authority_raw =
            std::fs::read(generated.path().join("objstm-xref-authority.pdf")).unwrap();
        let authority = indexed(&authority_raw, None);
        assert_eq!(
            authority.reader.object_location((7, 0)),
            Some(IndexedObjectLocation::Compressed {
                container: (10, 0),
                index: 0,
            })
        );
        let selected = authority.prepare_declared_object_stream((10, 0)).unwrap();
        assert_eq!(selected.as_object_stream().container_id(), (10, 0));
        assert_eq!(
            authority
                .counters()
                .container_preparations
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            authority
                .counters()
                .object_resolutions
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            authority.counters().object_failures.load(Ordering::Relaxed),
            0
        );
        let authority_snapshot = authority.object_cells.snapshot();
        assert_member_counters_zero(&authority_snapshot);
        assert_representation_counter_sums(&authority_snapshot);
        assert_eq!(authority.reader.cache_stats(), Default::default());
        assert_eq!(authority.reader.object_cache_stats(), Default::default());
        assert_eq!(
            authority.reader.object_stream_cache_stats(),
            Default::default()
        );
        assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
    }

    #[test]
    fn indexed_declared_container_persistent_failure_is_arc_shared_and_classed_once() {
        let _test_lock = indexed_test_lock();
        let raw = indexed_metadata_fixture();
        let adapter = indexed(&raw, None);
        let first = match adapter.prepare_declared_object_stream((5, 2)) {
            Err(ContainerCellError::Shared(owner)) => owner,
            Err(ContainerCellError::Control(_)) => {
                panic!("normal non-stream container must be a shared failure")
            }
            Ok(_) => panic!("normal non-stream object must not prepare as an object stream"),
        };
        let second = match adapter.prepare_declared_object_stream((5, 2)) {
            Err(ContainerCellError::Shared(owner)) => owner,
            Err(ContainerCellError::Control(_)) => {
                panic!("persistent hit must remain a shared failure")
            }
            Ok(_) => panic!("persistent non-stream hit must not succeed"),
        };
        assert!(Arc::ptr_eq(&first, &second));

        let counters = adapter.counters();
        assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 1);
        assert_eq!(
            counters
                .container_preparation_successes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_preparation_failures
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .container_persistent_native_failures
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .container_persistent_above_cap_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_flight_only_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_exact_key_invariant_failures
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(counters.object_resolutions.load(Ordering::Relaxed), 0);
        assert_eq!(counters.object_failures.load(Ordering::Relaxed), 0);
        assert_eq!(adapter.reader.cache_stats(), Default::default());
        assert_eq!(adapter.reader.object_cache_stats(), Default::default());
        assert_eq!(
            adapter.reader.object_stream_cache_stats(),
            Default::default()
        );
    }

    #[test]
    fn indexed_declared_container_persistent_fixture_matrix_is_exact() {
        use crate::objstm_failures::{FailurePayload, ObjStmFailureClass};

        #[derive(Clone, Copy)]
        enum DynamicExpectation {
            Any,
            None,
            Present,
        }

        let _test_lock = indexed_test_lock();
        let generated = AccessGeneratedDir::new("persistent");
        generate_objstm_fixtures(generated.path(), &["--profile", "objstm-container"]);
        let mut runs = Vec::new();
        for name in [
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
            "objstm-endstream-missing.pdf",
            "objstm-endstream-truncated.pdf",
        ] {
            let dynamic = match name {
                "objstm-filter-chain.pdf" => DynamicExpectation::None,
                "objstm-flate-corrupt.pdf" => DynamicExpectation::Present,
                _ => DynamicExpectation::Any,
            };
            runs.push((
                generated.path().join(name),
                (6, 0),
                2_usize,
                ObjStmFailureClass::PersistentNative,
                dynamic,
                true,
            ));
        }
        runs.push((
            generated.path().join("objstm-container-generation.pdf"),
            (6, 1),
            2,
            ObjStmFailureClass::PersistentNative,
            DynamicExpectation::Any,
            false,
        ));
        for callers in [4_usize, 16] {
            runs.push((
                generated.path().join("objstm-filter-chain.pdf"),
                (6, 0),
                callers,
                ObjStmFailureClass::PersistentNative,
                DynamicExpectation::None,
                true,
            ));
            runs.push((
                generated.path().join("objstm-flate-corrupt.pdf"),
                (6, 0),
                callers,
                ObjStmFailureClass::PersistentNative,
                DynamicExpectation::Present,
                true,
            ));
        }

        let above_cap = AccessGeneratedDir::new("above-cap");
        generate_objstm_fixtures(
            above_cap.path(),
            &[
                "--profile",
                "objstm-boundary",
                "--objstm-boundary",
                "encoded-over-64m",
            ],
        );
        runs.push((
            above_cap.path().join("objstm-encoded-over-64m.pdf"),
            (6, 0),
            1,
            ObjStmFailureClass::PersistentAboveCap,
            DynamicExpectation::None,
            false,
        ));

        for (path, container, callers, expected_class, dynamic, block_in_source) in runs {
            let raw: Arc<[u8]> = Arc::from(std::fs::read(&path).unwrap());
            let broker_baseline = crate::broker::BudgetBroker::production().snapshot();
            let (source, control) = blocking_objstm_source(raw);
            let adapter = Arc::new(IndexedDocumentAdapter::open(source, None).unwrap());
            let baseline = adapter.object_cells.snapshot();
            let broker_open = crate::broker::BudgetBroker::production().snapshot();
            let waiters = ObjStmWaitBarrier::new(container, callers.saturating_sub(1));
            adapter.object_cells.set_wait_hooks(waiters.clone());
            if block_in_source {
                control.arm(ObjStmSourceFailure::None);
            }

            let leader_adapter = Arc::clone(&adapter);
            let leader = std::thread::spawn(move || {
                leader_adapter.prepare_declared_object_stream(container)
            });
            let mut followers = Vec::new();
            if block_in_source {
                control.wait_until_blocked();
                let broker_blocked = crate::broker::BudgetBroker::production().snapshot();
                let blocked_cells = adapter.object_cells.snapshot();
                let loading_metadata = blocked_cells.cache_bytes - baseline.cache_bytes;
                assert_eq!(
                    broker_blocked.normal_payload_bytes,
                    broker_open.normal_payload_bytes + loading_metadata + INDEXED_STREAM_BYTES,
                    "{}",
                    path.display()
                );
                assert_eq!(
                    broker_blocked.normal_in_flight_estimate_bytes,
                    broker_open.normal_in_flight_estimate_bytes + INDEXED_STREAM_BYTES,
                    "{}",
                    path.display()
                );
                assert_eq!(
                    broker_blocked.metadata_bytes,
                    broker_open.metadata_bytes + crate::broker::QUEUE_METADATA_WEIGHT
                );
                assert_eq!(
                    broker_blocked.completion_reserve_bytes,
                    broker_open.completion_reserve_bytes
                );
                assert_eq!(broker_blocked.oversize_bytes, broker_open.oversize_bytes);
                assert_eq!(
                    broker_blocked.aggregate_bytes,
                    broker_open.aggregate_bytes
                        + loading_metadata
                        + INDEXED_STREAM_BYTES
                        + crate::broker::QUEUE_METADATA_WEIGHT
                );
                assert_eq!(
                    broker_blocked.cache_bytes,
                    broker_open.cache_bytes + loading_metadata
                );
                assert_eq!(broker_blocked.pin_bytes, broker_open.pin_bytes);
                assert_eq!(broker_blocked.bypass_bytes, broker_open.bypass_bytes);
                assert_eq!(
                    broker_blocked.peak_normal_bytes,
                    broker_open
                        .peak_normal_bytes
                        .max(broker_blocked.normal_payload_bytes + broker_blocked.metadata_bytes)
                );
                assert_eq!(
                    broker_blocked.peak_aggregate_bytes,
                    broker_open
                        .peak_aggregate_bytes
                        .max(broker_blocked.aggregate_bytes)
                );
                assert_eq!(
                    broker_blocked.peak_cache_bytes,
                    broker_open.peak_cache_bytes.max(broker_blocked.cache_bytes)
                );
                assert_eq!(broker_blocked.peak_pin_bytes, broker_open.peak_pin_bytes);
                assert_eq!(
                    broker_blocked.peak_bypass_bytes,
                    broker_open
                        .peak_bypass_bytes
                        .max(broker_open.bypass_bytes + loading_metadata)
                );
                let fresh_permit = adapter
                    .object_cells
                    .active_container_permit_stats(container)
                    .expect("blocked persistent leader owns a fresh permit");
                assert_eq!(
                    adapter
                        .counters()
                        .initial_container_permit_current_bytes
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(
                    adapter
                        .counters()
                        .initial_container_permit_peak_bytes
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(fresh_permit.limit_bytes, INDEXED_STREAM_BYTES);
                assert_eq!(fresh_permit.current_bytes, 256);
                assert_eq!(fresh_permit.peak_bytes, 256);
                assert_eq!(fresh_permit.reservations, 1);
                assert!(!fresh_permit.cancelled);
                assert!(!fresh_permit.closed);
                for _ in 1..callers {
                    let adapter = Arc::clone(&adapter);
                    followers.push(std::thread::spawn(move || {
                        adapter.prepare_declared_object_stream(container)
                    }));
                }
                waiters.wait_until_attached();
                control.release();
            }

            let mut owners = Vec::new();
            let result = leader.join().unwrap();
            owners.push(match result {
                Err(ContainerCellError::Shared(owner)) => owner,
                Err(ContainerCellError::Control(_)) => {
                    panic!("{}: persistent failure became control", path.display())
                }
                Ok(_) => panic!("{}: persistent fixture succeeded", path.display()),
            });
            for follower in followers {
                owners.push(match follower.join().unwrap() {
                    Err(ContainerCellError::Shared(owner)) => owner,
                    Err(ContainerCellError::Control(_)) => {
                        panic!("{}: persistent waiter became control", path.display())
                    }
                    Ok(_) => panic!("{}: persistent waiter succeeded", path.display()),
                });
            }
            if !block_in_source {
                for _ in 1..callers {
                    owners.push(match adapter.prepare_declared_object_stream(container) {
                        Err(ContainerCellError::Shared(owner)) => owner,
                        Err(ContainerCellError::Control(_)) => {
                            panic!("{}: persistent hit became control", path.display())
                        }
                        Ok(_) => panic!("{}: persistent fixture succeeded", path.display()),
                    });
                }
            }
            assert_eq!(
                adapter
                    .counters()
                    .initial_container_permit_current_bytes
                    .load(Ordering::Relaxed),
                0,
                "{}",
                path.display()
            );
            assert_eq!(
                adapter
                    .counters()
                    .initial_container_permit_peak_bytes
                    .load(Ordering::Relaxed),
                0,
                "{}",
                path.display()
            );
            assert!(owners.iter().all(|owner| Arc::ptr_eq(owner, &owners[0])));
            let FailurePayload::ObjStm(template) = owners[0].payload() else {
                panic!("{}: persistent owner lost ObjStm payload", path.display())
            };
            let retained_weight = owners[0].retained_weight();
            assert_eq!(
                retained_weight,
                owners[0].payload().retained_weight().unwrap(),
                "{}",
                path.display()
            );
            let dynamic_allocation = owners[0].objstm_dynamic_allocation();
            assert_eq!(template.class(), expected_class, "{}", path.display());
            match dynamic {
                DynamicExpectation::Any => {}
                DynamicExpectation::None => {
                    assert_eq!(template.dynamic_capacity(), Some(0), "{}", path.display());
                    assert_eq!(dynamic_allocation, None, "{}", path.display());
                }
                DynamicExpectation::Present => {
                    assert!(
                        template.dynamic_capacity().is_some_and(|bytes| bytes > 0),
                        "{}",
                        path.display()
                    );
                    assert!(
                        dynamic_allocation.is_some_and(|(pointer, capacity)| {
                            pointer != 0 && capacity == template.dynamic_capacity().unwrap()
                        }),
                        "{}",
                        path.display()
                    );
                }
            }

            let reads_after_error = control.reads();
            let peak_after_error = adapter
                .counters()
                .peak_container_permit_bytes
                .load(Ordering::Relaxed);
            assert!(peak_after_error <= INDEXED_STREAM_BYTES);
            if block_in_source {
                assert!(peak_after_error > 0, "{}", path.display());
            }
            let hit = match adapter.prepare_declared_object_stream(container) {
                Err(ContainerCellError::Shared(owner)) => owner,
                Err(ContainerCellError::Control(_)) => {
                    panic!("{}: persistent hit became control", path.display())
                }
                Ok(_) => panic!("{}: persistent hit succeeded", path.display()),
            };
            assert!(Arc::ptr_eq(&hit, &owners[0]));
            let FailurePayload::ObjStm(_) = hit.payload() else {
                panic!("{}: persistent hit lost ObjStm payload", path.display())
            };
            assert_eq!(hit.retained_weight(), retained_weight, "{}", path.display());
            assert_eq!(
                hit.objstm_dynamic_allocation(),
                dynamic_allocation,
                "{}",
                path.display()
            );
            assert_eq!(control.reads(), reads_after_error);
            assert_eq!(
                adapter
                    .counters()
                    .peak_container_permit_bytes
                    .load(Ordering::Relaxed),
                peak_after_error
            );

            let snapshot = adapter.object_cells.snapshot();
            assert_container_delta(
                &baseline,
                &snapshot,
                crate::object_cells::RepresentationSnapshot {
                    calls: callers as u64 + 1,
                    loads: 1,
                    waits: if block_in_source {
                        (callers - 1) as u64
                    } else {
                        0
                    },
                    negative_hits: if block_in_source { 1 } else { callers as u64 },
                    hits: 0,
                    transient_shares: 0,
                    bypasses: 0,
                    evictions: 0,
                    cancellations: 0,
                },
            );
            if container == (6, 1) {
                assert_eq!(snapshot.raw, baseline.raw);
                assert_eq!(snapshot.members, baseline.members);
                assert_declared_container_route_neutral(&adapter);
            }
            let published_weight = snapshot.cache_bytes - baseline.cache_bytes;
            let loading_metadata = published_weight
                .checked_sub(retained_weight)
                .expect("persistent retained weight fits the exact cell charge");
            let broker_published = crate::broker::BudgetBroker::production().snapshot();
            assert_eq!(
                broker_published.normal_payload_bytes,
                broker_open.normal_payload_bytes + published_weight,
                "{}",
                path.display()
            );
            assert_eq!(
                broker_published.normal_in_flight_estimate_bytes,
                broker_open.normal_in_flight_estimate_bytes
            );
            assert_eq!(broker_published.metadata_bytes, broker_open.metadata_bytes);
            assert_eq!(
                broker_published.completion_reserve_bytes,
                broker_open.completion_reserve_bytes
            );
            assert_eq!(broker_published.oversize_bytes, broker_open.oversize_bytes);
            assert_eq!(
                broker_published.aggregate_bytes,
                broker_open.aggregate_bytes + published_weight
            );
            assert_eq!(
                broker_published.cache_bytes,
                broker_open.cache_bytes + published_weight
            );
            assert_eq!(broker_published.pin_bytes, broker_open.pin_bytes);
            assert_eq!(broker_published.bypass_bytes, broker_open.bypass_bytes);
            let loader_peak = broker_open
                .normal_payload_bytes
                .checked_add(loading_metadata + INDEXED_STREAM_BYTES)
                .and_then(|bytes| {
                    bytes.checked_add(
                        broker_open.metadata_bytes + crate::broker::QUEUE_METADATA_WEIGHT,
                    )
                })
                .unwrap();
            assert_eq!(
                broker_published.peak_normal_bytes,
                broker_open.peak_normal_bytes.max(loader_peak)
            );
            assert_eq!(
                broker_published.peak_aggregate_bytes,
                broker_open.peak_aggregate_bytes.max(
                    loader_peak + broker_open.completion_reserve_bytes + broker_open.oversize_bytes
                )
            );
            assert_eq!(
                broker_published.peak_cache_bytes,
                broker_open
                    .peak_cache_bytes
                    .max(broker_open.cache_bytes + published_weight)
            );
            assert_eq!(broker_published.peak_pin_bytes, broker_open.peak_pin_bytes);
            assert_eq!(
                broker_published.peak_bypass_bytes,
                broker_open
                    .peak_bypass_bytes
                    .max(broker_open.bypass_bytes + loading_metadata)
            );
            let counters = adapter.counters();
            assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 1);
            assert_eq!(
                counters
                    .container_preparation_failures
                    .load(Ordering::Relaxed),
                1
            );
            assert_eq!(
                counters
                    .container_preparation_successes
                    .load(Ordering::Relaxed),
                0
            );
            match expected_class {
                ObjStmFailureClass::PersistentNative => assert_eq!(
                    counters
                        .container_persistent_native_failures
                        .load(Ordering::Relaxed),
                    1
                ),
                ObjStmFailureClass::PersistentAboveCap => assert_eq!(
                    counters
                        .container_persistent_above_cap_failures
                        .load(Ordering::Relaxed),
                    1
                ),
                _ => unreachable!(),
            }
            assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
            assert_no_o_or_completion_change(
                &broker_baseline,
                &crate::broker::BudgetBroker::production().snapshot(),
            );
            drop(hit);
            drop(owners);
            drop(adapter);
            let drained = crate::broker::BudgetBroker::production().snapshot();
            assert_broker_current_equal(&broker_baseline, &drained);
            assert_eq!(drained.aggregate_bytes, broker_baseline.aggregate_bytes);
            assert_eq!(
                drained.normal_payload_bytes,
                broker_baseline.normal_payload_bytes
            );
            assert_eq!(drained.cache_bytes, broker_baseline.cache_bytes);
            assert_eq!(drained.pin_bytes, broker_baseline.pin_bytes);
            assert_eq!(drained.bypass_bytes, broker_baseline.bypass_bytes);
            assert_eq!(drained.active_operations, broker_baseline.active_operations);
            assert_no_o_or_completion_change(&broker_baseline, &drained);
        }
    }

    #[test]
    fn indexed_declared_container_flight_failure_retries_without_raw_fallback() {
        let _test_lock = indexed_test_lock();
        let raw: Arc<[u8]> = Arc::from(indexed_object_stream_fixture());
        for (label, failure, expected_kind) in [
            (
                "source-changed",
                ObjStmSourceFailure::Changed,
                AccessKind::SourceChanged,
            ),
            ("source-io", ObjStmSourceFailure::Io, AccessKind::SourceIo),
        ] {
            for callers in [2_usize, 4, 16] {
                let broker_baseline = crate::broker::BudgetBroker::production().snapshot();
                let (source, control) = blocking_objstm_source(Arc::clone(&raw));
                let adapter = Arc::new(IndexedDocumentAdapter::open(source, None).unwrap());
                let container = first_declared_container(&adapter);
                let waiters = ObjStmWaitBarrier::new(container, callers - 1);
                adapter.object_cells.set_wait_hooks(waiters.clone());
                let baseline = adapter.object_cells.snapshot();
                let broker_open = crate::broker::BudgetBroker::production().snapshot();
                control.arm(failure);

                let leader_adapter = Arc::clone(&adapter);
                let leader = std::thread::spawn(move || {
                    leader_adapter.prepare_declared_object_stream(container)
                });
                control.wait_until_blocked();
                let broker_blocked = crate::broker::BudgetBroker::production().snapshot();
                let blocked_cells = adapter.object_cells.snapshot();
                let loading_metadata = blocked_cells.cache_bytes - baseline.cache_bytes;
                assert_eq!(
                    broker_blocked.normal_in_flight_estimate_bytes,
                    broker_open.normal_in_flight_estimate_bytes + INDEXED_STREAM_BYTES,
                    "{label}, {callers} callers"
                );
                assert_eq!(
                    broker_blocked.normal_payload_bytes,
                    broker_open.normal_payload_bytes + loading_metadata + INDEXED_STREAM_BYTES,
                    "{label}, {callers} callers"
                );
                assert_eq!(
                    broker_blocked.metadata_bytes,
                    broker_open.metadata_bytes + crate::broker::QUEUE_METADATA_WEIGHT
                );
                assert_eq!(
                    broker_blocked.completion_reserve_bytes,
                    broker_open.completion_reserve_bytes
                );
                assert_eq!(broker_blocked.oversize_bytes, broker_open.oversize_bytes);
                assert_eq!(
                    broker_blocked.aggregate_bytes,
                    broker_open.aggregate_bytes
                        + loading_metadata
                        + INDEXED_STREAM_BYTES
                        + crate::broker::QUEUE_METADATA_WEIGHT
                );
                assert_eq!(
                    broker_blocked.cache_bytes,
                    broker_open.cache_bytes + loading_metadata
                );
                assert_eq!(broker_blocked.pin_bytes, broker_open.pin_bytes);
                assert_eq!(broker_blocked.bypass_bytes, broker_open.bypass_bytes);
                assert_eq!(
                    broker_blocked.peak_normal_bytes,
                    broker_open
                        .peak_normal_bytes
                        .max(broker_blocked.normal_payload_bytes + broker_blocked.metadata_bytes)
                );
                assert_eq!(
                    broker_blocked.peak_aggregate_bytes,
                    broker_open
                        .peak_aggregate_bytes
                        .max(broker_blocked.aggregate_bytes)
                );
                assert_eq!(
                    broker_blocked.peak_cache_bytes,
                    broker_open.peak_cache_bytes.max(broker_blocked.cache_bytes)
                );
                assert_eq!(broker_blocked.peak_pin_bytes, broker_open.peak_pin_bytes);
                assert_eq!(
                    broker_blocked.peak_bypass_bytes,
                    broker_open
                        .peak_bypass_bytes
                        .max(broker_open.bypass_bytes + loading_metadata)
                );
                let fresh_permit = adapter
                    .object_cells
                    .active_container_permit_stats(container)
                    .expect("blocked flight leader owns a fresh permit");
                assert_eq!(
                    adapter
                        .counters()
                        .initial_container_permit_current_bytes
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(
                    adapter
                        .counters()
                        .initial_container_permit_peak_bytes
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(fresh_permit.limit_bytes, INDEXED_STREAM_BYTES);
                assert_eq!(fresh_permit.current_bytes, 256);
                assert_eq!(fresh_permit.peak_bytes, 256);
                assert_eq!(fresh_permit.reservations, 1);
                assert!(!fresh_permit.cancelled);
                assert!(!fresh_permit.closed);
                assert_no_o_or_completion_change(&broker_open, &broker_blocked);
                assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);

                let mut followers = Vec::new();
                for _ in 1..callers {
                    let adapter = Arc::clone(&adapter);
                    followers.push(std::thread::spawn(move || {
                        adapter.prepare_declared_object_stream(container)
                    }));
                }
                waiters.wait_until_attached();
                control.release();

                let mut owners = Vec::new();
                owners.push(match leader.join().unwrap() {
                    Err(ContainerCellError::Shared(owner)) => owner,
                    Err(ContainerCellError::Control(_)) => {
                        panic!("{label} must be a shared flight failure")
                    }
                    Ok(_) => panic!("{label} preparation must not succeed"),
                });
                for follower in followers {
                    owners.push(match follower.join().unwrap() {
                        Err(ContainerCellError::Shared(owner)) => owner,
                        Err(ContainerCellError::Control(_)) => {
                            panic!("attached {label} waiter became control")
                        }
                        Ok(_) => panic!("attached {label} waiter succeeded"),
                    });
                }
                assert!(owners.iter().all(|owner| Arc::ptr_eq(owner, &owners[0])));
                let crate::objstm_failures::FailurePayload::ObjStm(template) = owners[0].payload()
                else {
                    panic!("{label}: flight owner lost ObjStm payload")
                };
                let crate::objstm_failures::ObjStmFact::FlightAccess { kind, .. } = template.fact()
                else {
                    panic!("{label}: flight owner lost typed source fact")
                };
                assert_eq!(*kind, expected_kind, "{label}");
                let flight_snapshot = adapter.object_cells.snapshot();
                assert_eq!(
                    flight_snapshot.containers.calls - baseline.containers.calls,
                    callers as u64
                );
                assert_eq!(
                    flight_snapshot.containers.loads - baseline.containers.loads,
                    1
                );
                assert_eq!(
                    flight_snapshot.containers.hits - baseline.containers.hits,
                    0
                );
                assert_eq!(
                    flight_snapshot.containers.waits - baseline.containers.waits,
                    (callers - 1) as u64
                );
                assert_eq!(
                    flight_snapshot.containers.negative_hits - baseline.containers.negative_hits,
                    0
                );
                assert_eq!(
                    flight_snapshot.containers.transient_shares
                        - baseline.containers.transient_shares,
                    (callers - 1) as u64
                );
                assert_eq!(
                    flight_snapshot.containers.bypasses - baseline.containers.bypasses,
                    0
                );
                assert_eq!(
                    flight_snapshot.containers.evictions - baseline.containers.evictions,
                    0
                );
                assert_eq!(
                    flight_snapshot.containers.cancellations - baseline.containers.cancellations,
                    0
                );
                assert_member_counters_zero(&flight_snapshot);
                assert_representation_counter_sums(&flight_snapshot);
                let error_peak = adapter
                    .counters()
                    .peak_container_permit_bytes
                    .load(Ordering::Relaxed);
                assert_eq!(error_peak, 256, "{label}");
                assert_eq!(
                    adapter
                        .counters()
                        .container_preparation_failures
                        .load(Ordering::Relaxed),
                    1
                );
                assert_eq!(
                    adapter
                        .counters()
                        .container_flight_only_failures
                        .load(Ordering::Relaxed),
                    1
                );

                drop(owners);
                let handed_off = adapter.object_cells.snapshot();
                assert_eq!(handed_off.cells, 0, "{label}");
                assert_eq!(handed_off.loading, 0, "{label}");
                assert_eq!(handed_off.ready, 0, "{label}");
                assert_eq!(handed_off.negative, 0, "{label}");
                assert_eq!(handed_off.live_interests, 0, "{label}");
                assert_eq!(handed_off.external_pins, 0, "{label}");
                assert_eq!(handed_off.cache_bytes, 0, "{label}");
                assert!(!adapter.object_cells.has_container_cell(container));
                assert_eq!(
                    adapter
                        .object_cells
                        .active_container_permit_stats(container),
                    None
                );
                let broker_handed_off = crate::broker::BudgetBroker::production().snapshot();
                assert_broker_current_equal(&broker_open, &broker_handed_off);
                assert_eq!(
                    broker_handed_off.normal_payload_bytes, broker_open.normal_payload_bytes,
                    "{label}"
                );
                assert_eq!(
                    broker_handed_off.normal_in_flight_estimate_bytes,
                    broker_open.normal_in_flight_estimate_bytes,
                    "{label}"
                );
                assert_eq!(
                    broker_handed_off.metadata_bytes, broker_open.metadata_bytes,
                    "{label}"
                );
                assert_eq!(
                    broker_handed_off.completion_reserve_bytes,
                    broker_open.completion_reserve_bytes,
                    "{label}"
                );
                assert_eq!(
                    broker_handed_off.oversize_bytes, broker_open.oversize_bytes,
                    "{label}"
                );
                assert_eq!(
                    broker_handed_off.aggregate_bytes, broker_open.aggregate_bytes,
                    "{label}"
                );
                assert_eq!(
                    broker_handed_off.cache_bytes, broker_open.cache_bytes,
                    "{label}"
                );
                assert_eq!(
                    broker_handed_off.pin_bytes, broker_open.pin_bytes,
                    "{label}"
                );
                assert_eq!(
                    broker_handed_off.bypass_bytes, broker_open.bypass_bytes,
                    "{label}"
                );
                control.clear_failure();
                let pin = match adapter.prepare_declared_object_stream(container) {
                    Ok(pin) => pin,
                    Err(_) => panic!("successor generation must retry after flight failure"),
                };
                assert_eq!(pin.as_object_stream().container_id(), container);
                let (success_retained, success_permit, success_charge) = adapter
                    .object_cells
                    .container_retained_evidence(container)
                    .expect("successor retains its exact permit and broker charge");
                assert_eq!(success_permit.stats().current_bytes, success_retained);
                assert_eq!(success_charge, success_retained);
                let counters = adapter.counters();
                assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 2);
                assert_eq!(
                    counters
                        .container_preparation_successes
                        .load(Ordering::Relaxed),
                    1
                );
                assert_eq!(
                    counters
                        .container_preparation_failures
                        .load(Ordering::Relaxed),
                    1
                );
                assert_eq!(
                    counters.peak_container_permit_bytes.load(Ordering::Relaxed),
                    success_permit.stats().peak_bytes
                );
                assert_eq!(
                    counters
                        .active_container_preparations
                        .load(Ordering::Relaxed),
                    0
                );
                assert_eq!(
                    counters
                        .peak_active_container_preparations
                        .load(Ordering::Relaxed),
                    1
                );
                assert_eq!(counters.object_resolutions.load(Ordering::Relaxed), 0);
                assert_eq!(counters.object_failures.load(Ordering::Relaxed), 0);
                let final_snapshot = adapter.object_cells.snapshot();
                assert_eq!(
                    final_snapshot.containers.calls - baseline.containers.calls,
                    callers as u64 + 1
                );
                assert_eq!(
                    final_snapshot.containers.loads - baseline.containers.loads,
                    2
                );
                assert_eq!(final_snapshot.containers.hits - baseline.containers.hits, 0);
                assert_eq!(
                    final_snapshot.containers.waits - baseline.containers.waits,
                    (callers - 1) as u64
                );
                assert_eq!(
                    final_snapshot.containers.negative_hits - baseline.containers.negative_hits,
                    0
                );
                assert_eq!(
                    final_snapshot.containers.transient_shares
                        - baseline.containers.transient_shares,
                    (callers - 1) as u64
                );
                assert_eq!(
                    final_snapshot.containers.bypasses - baseline.containers.bypasses,
                    0
                );
                assert_eq!(
                    final_snapshot.containers.evictions - baseline.containers.evictions,
                    0
                );
                assert_eq!(
                    final_snapshot.containers.cancellations - baseline.containers.cancellations,
                    0
                );
                assert_member_counters_zero(&final_snapshot);
                assert_representation_counter_sums(&final_snapshot);
                assert_eq!(adapter.reader.cache_stats(), Default::default());
                assert_eq!(adapter.reader.object_cache_stats(), Default::default());
                assert_eq!(
                    adapter.reader.object_stream_cache_stats(),
                    Default::default()
                );
                assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
                assert_no_o_or_completion_change(
                    &broker_baseline,
                    &crate::broker::BudgetBroker::production().snapshot(),
                );
                drop(pin);
                drop(adapter);
                assert_eq!(success_permit.stats().current_bytes, 0);
                let drained = crate::broker::BudgetBroker::production().snapshot();
                assert_broker_current_equal(&broker_baseline, &drained);
                assert_eq!(drained.aggregate_bytes, broker_baseline.aggregate_bytes);
                assert_eq!(
                    drained.normal_payload_bytes,
                    broker_baseline.normal_payload_bytes
                );
                assert_eq!(
                    drained.normal_in_flight_estimate_bytes,
                    broker_baseline.normal_in_flight_estimate_bytes
                );
                assert_eq!(drained.cache_bytes, broker_baseline.cache_bytes);
                assert_eq!(drained.pin_bytes, broker_baseline.pin_bytes);
                assert_eq!(drained.bypass_bytes, broker_baseline.bypass_bytes);
                assert_eq!(drained.active_operations, broker_baseline.active_operations);
                assert_no_o_or_completion_change(&broker_baseline, &drained);
            }
        }
    }

    #[test]
    fn indexed_declared_container_unsupported_length_is_flight_only_and_retryable() {
        let _test_lock = indexed_test_lock();
        let generated = AccessGeneratedDir::new("unsupported-length");
        generate_objstm_fixtures(generated.path(), &["--profile", "objstm-container"]);
        for name in [
            "objstm-length-missing.pdf",
            "objstm-length-negative.pdf",
            "objstm-length-indirect-missing.pdf",
        ] {
            let raw = std::fs::read(generated.path().join(name)).unwrap();
            let adapter = indexed(&raw, None);
            let baseline = adapter.object_cells.snapshot();
            let first = match adapter.prepare_declared_object_stream((6, 0)) {
                Err(ContainerCellError::Shared(owner)) => owner,
                Err(ContainerCellError::Control(_)) => {
                    panic!("{name}: unsupported length became control")
                }
                Ok(_) => panic!("{name}: unsupported length succeeded"),
            };
            let peak_after_first_error = adapter
                .counters()
                .peak_container_permit_bytes
                .load(Ordering::Relaxed);
            drop(first);
            let second = match adapter.prepare_declared_object_stream((6, 0)) {
                Err(ContainerCellError::Shared(owner)) => owner,
                Err(ContainerCellError::Control(_)) => {
                    panic!("{name}: unsupported-length retry became control")
                }
                Ok(_) => panic!("{name}: unsupported-length retry succeeded"),
            };
            drop(second);
            let counters = adapter.counters();
            assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 2);
            assert_eq!(
                counters
                    .container_preparation_failures
                    .load(Ordering::Relaxed),
                2
            );
            assert_eq!(
                counters
                    .container_flight_only_failures
                    .load(Ordering::Relaxed),
                2
            );
            assert_eq!(
                counters.peak_container_permit_bytes.load(Ordering::Relaxed),
                peak_after_first_error,
                "{name}"
            );
            let snapshot = adapter.object_cells.snapshot();
            assert_eq!(snapshot.containers.calls - baseline.containers.calls, 2);
            assert_eq!(snapshot.containers.loads - baseline.containers.loads, 2);
            assert_eq!(
                snapshot.containers.negative_hits - baseline.containers.negative_hits,
                0
            );
            assert_eq!(
                snapshot.containers.transient_shares - baseline.containers.transient_shares,
                0
            );
            assert_member_counters_zero(&snapshot);
            assert_representation_counter_sums(&snapshot);
            assert_eq!(adapter.reader.cache_stats(), Default::default());
            assert_eq!(adapter.reader.object_cache_stats(), Default::default());
            assert_eq!(
                adapter.reader.object_stream_cache_stats(),
                Default::default()
            );
            assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
        }
    }

    #[test]
    fn indexed_container_attempt_guard_clears_active_on_unwind() {
        let counters = IndexedAdapterCounters::default();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _attempt = IndexedContainerAttempt::start(&counters);
            panic!("injected preparation panic");
        }));
        assert!(unwind.is_err());
        assert_eq!(counters.container_preparations.load(Ordering::Relaxed), 1);
        assert_eq!(
            counters
                .active_container_preparations
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .peak_active_container_preparations
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters
                .container_preparation_successes
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters
                .container_preparation_failures
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn indexed_negative_classifier_covers_every_current_fork_variant() {
        use lopdf::{MissingNormalObjectReason, ObjectLimitProvenance};

        let id = (7, 3);
        let persistent = vec![
            IndexedReaderError::MissingNormalObject { id },
            IndexedReaderError::MissingNormalObjectAtXref {
                id,
                reason: MissingNormalObjectReason::HeaderProbeLimit {
                    offset: 1,
                    limit: 2,
                },
            },
            IndexedReaderError::GenerationMismatch { id, indexed: 2 },
            IndexedReaderError::IndirectObjectMismatch {
                expected: id,
                actual: (8, 0),
            },
            IndexedReaderError::InvalidIndirectObject { id, offset: 4 },
            IndexedReaderError::IncompleteObject { id, offset: 4 },
            IndexedReaderError::ObjectLimitExceeded {
                id,
                limit: 8,
                provenance: ObjectLimitProvenance::FrameNeedMoreAtMaximum,
            },
            IndexedReaderError::NotScalarObject { id },
            IndexedReaderError::NotStreamObject { id },
            IndexedReaderError::UnsupportedBoundedScalar { id, reason: "test" },
            IndexedReaderError::StreamLimitExceeded {
                id,
                length: 9,
                limit: 8,
            },
            IndexedReaderError::NegativeStreamLength { id, length: -1 },
            IndexedReaderError::MissingEndstream { id },
            IndexedReaderError::ResolutionCycle { id },
            IndexedReaderError::ResolutionDepthExceeded { limit: 128 },
            IndexedReaderError::ObjectDecryption {
                id,
                source: lopdf::encryption::DecryptionError::NotDecryptable,
            },
        ];
        for error in persistent {
            assert_eq!(
                indexed_negative_disposition(&error),
                NegativeDisposition::Persistent,
                "{error:?}"
            );
        }

        let transient = vec![
            IndexedReaderError::Source(SourceError::SourceChanged),
            IndexedReaderError::InvalidHeader { limit: 8 },
            IndexedReaderError::InvalidStartXref { limit: 8 },
            IndexedReaderError::StartXrefOutOfBounds {
                offset: 9,
                logical_len: 8,
            },
            IndexedReaderError::InvalidXref { offset: 1 },
            IndexedReaderError::IncompleteXref { offset: 1 },
            IndexedReaderError::InvalidTrailer { offset: 1 },
            IndexedReaderError::StructureLimitExceeded {
                structure: "test",
                limit: 8,
            },
            IndexedReaderError::EntryLimitExceeded { count: 9, limit: 8 },
            IndexedReaderError::RevisionLimitExceeded { limit: 8 },
            IndexedReaderError::InvalidTrailerOffset { key: "Root" },
            IndexedReaderError::XrefDecompression(lopdf::Error::InvalidStream("test".to_string())),
            IndexedReaderError::IndirectHeaderLimitExceeded {
                offset: 1,
                limit: 8,
            },
            IndexedReaderError::ScalarResourceLimit {
                id,
                requested: 9,
                limit: 8,
                phase: "test",
            },
            IndexedReaderError::ScalarResolutionCancelled { id, phase: "test" },
            IndexedReaderError::ScalarResolutionClosed { id, phase: "test" },
            IndexedReaderError::ObjectStreamContainerNotStream {
                id,
                container: (10, 0),
            },
            IndexedReaderError::ObjectStreamMember {
                id,
                container: (10, 0),
                index: 1,
                source: lopdf::Error::InvalidObjectStream("test".to_string()),
            },
            IndexedReaderError::ObjectStreamBatchSetup {
                container: (10, 0),
                source: lopdf::Error::InvalidObjectStream("test".to_string()),
            },
            IndexedReaderError::ObjectStreamCacheBypass { container: (10, 0) },
            IndexedReaderError::PasswordRequired,
            IndexedReaderError::InvalidPassword,
            IndexedReaderError::Encryption(lopdf::Error::InvalidStream("test".to_string())),
            IndexedReaderError::InvalidEncryptDictionary,
            IndexedReaderError::PageCountLimitExceeded { limit: 8 },
        ];
        for error in transient {
            assert_eq!(
                indexed_negative_disposition(&error),
                NegativeDisposition::FlightOnly,
                "{error:?}"
            );
        }
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
    fn indexed_tounicode_payload_exact_over_release_and_concurrent_admission() {
        let _test_lock = indexed_test_lock();
        const MIB: usize = 1024 * 1024;
        let make = |target: usize, compressed: bool| {
            let mut document = Document::with_version("1.7");
            let mut stream_dict = Dictionary::new();
            let content = if compressed {
                stream_dict.set("Filter", "FlateDecode");
                flate_zeros(target)
            } else {
                vec![0; target]
            };
            let stream = document.add_object(lopdf::Stream::new(stream_dict, content));
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

        let (_, exact_raw, exact_stream) = make(60 * MIB, true);
        let exact = indexed(&exact_raw, None);
        let stream = exact.object(exact_stream).unwrap();
        let bytes = stream.decoded_stream_bytes(64 * MIB).unwrap();
        let (payload, charge) = bytes.into_parts();
        assert_eq!(payload.len(), 60 * MIB);
        let aggregate = *provisional_o_budget().used.lock().unwrap();
        assert!(aggregate >= payload.capacity() as u64);
        assert!(aggregate <= PROVISIONAL_O_BYTES);
        drop(payload);
        drop(charge);
        drop(stream);
        assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);

        let (_, over_raw, over_stream) = make(64 * MIB + 1, true);
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
        assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);

        let (_, concurrent_raw, concurrent_stream) = make(40 * MIB, true);
        let concurrent = indexed(&concurrent_raw, None);
        let stream = concurrent.object(concurrent_stream).unwrap();
        let first = stream.decoded_stream_bytes(64 * MIB).unwrap();
        let second = stream.decoded_stream_bytes(64 * MIB).err().unwrap();
        assert_eq!(second.kind, AccessKind::ResourceLimit);
        drop(first);
        assert!(stream.decoded_stream_bytes(64 * MIB).is_ok());
        drop(stream);
        assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
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
            .expect_err("decoded content bomb must exceed aggregate O");
        assert_eq!(error.phase, AccessPhase::PageContent);
        assert_eq!(error.kind, AccessKind::ResourceLimit);
        assert_eq!(*provisional_o_budget().used.lock().unwrap(), 0);
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
        assert!(
            adapter
                .counters()
                .peak_resolution_bytes
                .load(Ordering::Relaxed)
                <= PROVISIONAL_O_BYTES
        );
        assert_eq!(adapter.object_cells.snapshot().cells, 0);
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
    fn page_content_budget_refuses_multi_stream_combine_overlap() {
        let _test_lock = indexed_test_lock();
        let page = (999, 0);
        let mut output = Vec::new();
        let mut output_charge = None;
        let payload = vec![b'a'; 20 * 1024 * 1024];
        let mut charge = provisional_o_budget()
            .try_acquire(payload.capacity() as u64)
            .unwrap();
        charge.shrink_to(payload.capacity() as u64);
        append_page_payload(page, &mut output, &mut output_charge, payload, charge).unwrap();

        let payload = vec![b'b'; 20 * 1024 * 1024];
        let mut charge = provisional_o_budget()
            .try_acquire(payload.capacity() as u64)
            .unwrap();
        charge.shrink_to(payload.capacity() as u64);
        let error = append_page_payload(page, &mut output, &mut output_charge, payload, charge)
            .expect_err("old output + payload + combined output must not exceed O");
        assert_eq!(error.kind, AccessKind::ResourceLimit);
        drop(output_charge);
    }

    #[test]
    fn indexed_route_has_no_eager_document_or_unbounded_resolver_calls() {
        let source = include_str!("access.rs");
        let start = source
            .find("pub(crate) struct IndexedDocumentAdapter")
            .unwrap();
        let end = source[start..]
            .find("#[cfg(test)]\npub(crate) mod tests")
            .map(|offset| start + offset)
            .unwrap();
        let indexed = &source[start..end];
        assert!(!indexed.contains("resolve_object_shared"));
        assert!(!indexed.contains("resolve_object("));
        assert!(!indexed.contains("Document::"));
        assert!(!indexed.contains("load_mem"));
        assert!(indexed.contains("resolve_object_with_permit"));
        assert!(indexed.contains("trailer_entry_raw_owned"));
        assert!(indexed.contains(".try_acquire(PROVISIONAL_O_BYTES)"));
        assert!(indexed.contains("retained > budget.bytes"));
    }
}
