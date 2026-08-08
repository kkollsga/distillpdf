//! Broker-owned, per-document single-flight cells for raw normal PDF objects.
//!
//! This module is deliberately isolated from resolver routing.  It owns the
//! process-wide cell index, document epochs, bounded interests, publication,
//! cache ownership and close semantics; `access` supplies the actual bounded
//! object loader in the integration slice.

#![cfg_attr(not(test), allow(dead_code))]

use crate::access::{AccessError, AccessKind};
#[cfg(test)]
use crate::broker::PendingReservationDiagnostic;
use crate::broker::{
    BrokerError, BrokerOperation, BudgetBroker, Lane, OwnershipClass, ReservationCancellation,
    RetainedCharge, SelfPinCharge,
};
use crate::objstm_failures::{
    FailurePayload, InvariantStage, ObjStmFailureClass, ObjStmFailureTemplate, RetainedWeightError,
};
#[cfg(test)]
use lopdf::ScalarResolutionStats;
use lopdf::{
    BoundedObject, BoundedObjectStream, Object, ObjectId, ScalarResolutionPermit,
    BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES,
};
use std::any::Any;
#[cfg(test)]
use std::cell::Cell as ThreadCell;
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
#[cfg(test)]
use std::ops::{Deref, DerefMut};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};

const ARENA_METADATA_BYTES: u64 = 4 * 1024;
// The fixed 5 KiB precharge explicitly partitions an emergency flight-error
// owner. Broker admission/close failures cannot acquire another reservation,
// so their bounded owner and detail remain charged by this cell-local slice.
const CELL_BASE_METADATA_BYTES: u64 = 4_608;
const CELL_ERROR_ENVELOPE_BYTES: u64 = 512;
const CELL_METADATA_BYTES: u64 = CELL_BASE_METADATA_BYTES + CELL_ERROR_ENVELOPE_BYTES;
const ERROR_OWNER_BYTES: u64 = crate::objstm_failures::FAILURE_OWNER_BASE_BYTES;
const LOADER_ESTIMATE_BYTES: u64 = 64 * 1024 * 1024;
const PRODUCTION_CACHE_TARGET_BYTES: u64 = 32 * 1024 * 1024;
// Loading cells may temporarily sit beyond the completed-cache target. This
// cap leaves at least 16 MiB of B after a 32 MiB cache and 64 MiB loader,
// including broker/operation/request metadata.
const MAX_LOADING_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INTERESTS_PER_CELL: usize = 64;
const MAX_GLOBAL_INTERESTS: usize = 65_536;
const MAX_GLOBAL_CELLS: usize = 16_384;
const BTREE_NODE_ENVELOPE_BYTES: usize = 2 * 1024;
const PREPARED_STREAM_STRUCTURAL_BYTES: usize =
    BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES as usize;
const ARC_ALLOCATION_HEADERS_BYTES: usize = 4 * std::mem::size_of::<usize>();

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Representation {
    RawNormalObject,
    DeclaredObjStmContainer,
    DeclaredObjStmMember,
}

impl Representation {
    const COUNT: usize = 3;

    const fn index(self) -> usize {
        match self {
            Self::RawNormalObject => 0,
            Self::DeclaredObjStmContainer => 1,
            Self::DeclaredObjStmMember => 2,
        }
    }

    const fn loader_estimate(self) -> u64 {
        match self {
            Self::RawNormalObject | Self::DeclaredObjStmContainer => LOADER_ESTIMATE_BYTES,
            Self::DeclaredObjStmMember => 4 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct CellKey {
    epoch: u64,
    id: ObjectId,
    representation: Representation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NegativeDisposition {
    Persistent,
    FlightOnly,
}

#[derive(Debug)]
pub(crate) struct CellLoadError {
    payload: FailurePayload,
    disposition: Option<NegativeDisposition>,
}

impl CellLoadError {
    pub(crate) fn new(error: AccessError, disposition: NegativeDisposition) -> Self {
        Self {
            payload: FailurePayload::Access(error),
            disposition: Some(disposition),
        }
    }

    pub(crate) fn objstm(template: ObjStmFailureTemplate) -> Self {
        let disposition = match template.class() {
            ObjStmFailureClass::PersistentNative | ObjStmFailureClass::PersistentAboveCap => {
                Some(NegativeDisposition::Persistent)
            }
            ObjStmFailureClass::FlightOnly => Some(NegativeDisposition::FlightOnly),
            ObjStmFailureClass::ExactKeyInvariant => None,
        };
        Self {
            payload: FailurePayload::ObjStm(template),
            disposition,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectCellConfig {
    cache_target_bytes: u64,
    max_cells: usize,
    max_global_interests: usize,
}

impl ObjectCellConfig {
    const fn production() -> Self {
        Self {
            cache_target_bytes: PRODUCTION_CACHE_TARGET_BYTES,
            max_cells: MAX_GLOBAL_CELLS,
            max_global_interests: MAX_GLOBAL_INTERESTS,
        }
    }

    #[cfg(test)]
    pub(crate) const fn scaled(cache_target_bytes: u64) -> Self {
        Self {
            cache_target_bytes,
            max_cells: MAX_GLOBAL_CELLS,
            max_global_interests: MAX_GLOBAL_INTERESTS,
        }
    }

    #[cfg(test)]
    const fn with_caps(mut self, max_cells: usize, max_global_interests: usize) -> Self {
        self.max_cells = max_cells;
        self.max_global_interests = max_global_interests;
        self
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ObjectCellSnapshot {
    pub(crate) arenas: usize,
    pub(crate) cells: usize,
    pub(crate) loading: usize,
    pub(crate) ready: usize,
    pub(crate) negative: usize,
    pub(crate) live_interests: usize,
    pub(crate) external_pins: usize,
    pub(crate) cache_bytes: u64,
    pub(crate) calls: u64,
    pub(crate) loads: u64,
    pub(crate) hits: u64,
    pub(crate) waits: u64,
    pub(crate) negative_hits: u64,
    pub(crate) transient_shares: u64,
    pub(crate) bypasses: u64,
    pub(crate) evictions: u64,
    pub(crate) cancellations: u64,
    pub(crate) closes: u64,
    pub(crate) dependency_cancellations: usize,
    pub(crate) dependency_pins: usize,
    pub(crate) dependency_edges: usize,
    pub(crate) dependency_leaders: usize,
    pub(crate) dependency_edge_adds: u64,
    pub(crate) dependency_edge_removes: u64,
    pub(crate) dependency_cleanup_acks: u64,
    #[cfg(test)]
    pub(crate) active_generations: usize,
    #[cfg(test)]
    pub(crate) parse_active: usize,
    #[cfg(test)]
    pub(crate) permit_current: usize,
    #[cfg(test)]
    pub(crate) broker_cancellation_current: usize,
    #[cfg(test)]
    pub(crate) close_acknowledgements: u64,
    pub(crate) raw: RepresentationSnapshot,
    pub(crate) containers: RepresentationSnapshot,
    pub(crate) members: RepresentationSnapshot,
    pub(crate) invariant_failed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RepresentationSnapshot {
    pub(crate) calls: u64,
    pub(crate) loads: u64,
    pub(crate) hits: u64,
    pub(crate) waits: u64,
    pub(crate) negative_hits: u64,
    pub(crate) transient_shares: u64,
    pub(crate) bypasses: u64,
    pub(crate) evictions: u64,
    pub(crate) cancellations: u64,
}

#[derive(Clone)]
pub(crate) struct ObjectCellDomain {
    inner: Arc<DomainInner>,
}

struct DomainInner {
    broker: BudgetBroker,
    config: ObjectCellConfig,
    admission: Mutex<()>,
    headroom: Mutex<()>,
    wait_hooks: Mutex<Option<Arc<dyn WaitEdgeHooks>>>,
    dependency_hooks: Mutex<Option<Arc<dyn DependencyHooks>>>,
    #[cfg(test)]
    dependency_wait_hooks: Mutex<Option<Arc<dyn DependencyWaitHooks>>>,
    #[cfg(test)]
    dependency_cleanup_failure_hooks: Mutex<Option<Arc<dyn DependencyCleanupFailureHooks>>>,
    #[cfg(test)]
    dependency_terminal_cleanup_hooks: Mutex<Option<Arc<dyn DependencyTerminalCleanupHooks>>>,
    #[cfg(test)]
    dependency_pre_wait_hooks: Mutex<Option<Arc<dyn DependencyPreWaitHooks>>>,
    #[cfg(test)]
    close_hooks: Mutex<Option<Arc<dyn CloseEdgeHooks>>>,
    #[cfg(test)]
    leader_phase_hooks: Mutex<Option<Arc<dyn LeaderPhaseHooks>>>,
    #[cfg(test)]
    failure_delivery_hooks: Mutex<Option<Arc<dyn FailureDeliveryHooks>>>,
    #[cfg(test)]
    scheduled_resource_hooks: Mutex<Option<Arc<dyn ScheduledResourceHooks>>>,
    state: Mutex<DomainState>,
}

#[cfg(test)]
pub(crate) trait WaitEdgeHooks: Send + Sync {
    fn add(&self, epoch: u64, id: ObjectId, generation: u64, ordinal: u64);
    fn remove(&self, epoch: u64, id: ObjectId, generation: u64, ordinal: u64);
}

#[cfg(not(test))]
trait WaitEdgeHooks: Send + Sync {
    fn add(&self, epoch: u64, id: ObjectId, generation: u64, ordinal: u64);
    fn remove(&self, epoch: u64, id: ObjectId, generation: u64, ordinal: u64);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct DependencyLeaderToken {
    key: CellKey,
    generation: u64,
    leader_slot: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DependencyEdge {
    from: CellKey,
    to: CellKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DependencyEventKind {
    BeforeChildInstall,
    EdgeAdded,
    InsideChildResolve,
    ChildResolved,
    #[cfg(test)]
    ChildHandleTaken,
    PinInstalled,
    BeforeMemberAdmission,
    #[cfg(test)]
    MemberRequestCreated,
    #[cfg(test)]
    MemberQueued,
    #[cfg(test)]
    MemberGranted,
    #[cfg(test)]
    MemberParseEnter,
    #[cfg(test)]
    MemberParseExit,
    #[cfg(test)]
    MemberReconciledBeforePublication,
    #[cfg(test)]
    MemberPublished,
    CleanupDetached,
    EdgeRemoved,
    CleanupAcknowledged,
    SuccessorElected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DependencyEvent {
    token: DependencyLeaderToken,
    edge: DependencyEdge,
    kind: DependencyEventKind,
    ordinal: u64,
    successor: Option<DependencySuccessor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DependencySuccessor {
    generation: u64,
    leader_slot: usize,
    interest_ordinal: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DependencyTransition {
    Idle,
    Announcing {
        token: DependencyLeaderToken,
        kind: DependencyEventKind,
    },
    Cleaning(DependencyLeaderToken),
    Acknowledging(DependencyLeaderToken),
    CleanupFailed(DependencyLeaderToken),
    ReleasingSuccessor {
        old: DependencyLeaderToken,
        successor: DependencySuccessor,
    },
}

impl DependencyTransition {
    const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }
}

type DependencyHookPanic = Box<dyn Any + Send + 'static>;

struct DependencyCleanupFailure {
    token: DependencyLeaderToken,
    control: CellControlFailure,
    hook_panic: Option<DependencyHookPanic>,
}

struct DependencyCleanupCompletion {
    token: DependencyLeaderToken,
    hook_panic: Option<DependencyHookPanic>,
}

enum PublishedCleanupOwner {
    Ready(Arc<ResolvedObjectOwner>),
    Failure(Arc<FailureOwner>),
}

#[cfg(test)]
trait DependencyHooks: Send + Sync {
    fn event(&self, event: DependencyEvent);
}

#[cfg(test)]
trait DependencyWaitHooks: Send + Sync {
    fn waiting(&self, key: CellKey, transition: DependencyTransition);
}

#[cfg(test)]
trait DependencyCleanupFailureHooks: Send + Sync {
    fn failed(&self, key: CellKey, token: DependencyLeaderToken);
}

#[cfg(test)]
trait DependencyTerminalCleanupHooks: Send + Sync {
    fn before_metadata(&self, key: CellKey, owner: DependencyTerminalCleanupOwner);
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DependencyTerminalCleanupOwner {
    ArenaClose,
    LeaderTerminal,
    ExactTeardown,
}

#[cfg(test)]
trait DependencyPreWaitHooks: Send + Sync {
    fn before_wait(&self, key: CellKey, transition: DependencyTransition);
}

#[cfg(test)]
trait FailureDeliveryHooks: Send + Sync {
    fn delivered(&self, key: CellKey, owner: usize);
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScheduledResourceShape {
    Requested,
    Granted,
    Returned,
    Reconciled,
}

#[cfg(test)]
trait ScheduledResourceHooks: Send + Sync {
    fn enter(&self, key: CellKey, token: DependencyLeaderToken, shape: ScheduledResourceShape);
    fn dropped(&self, key: CellKey, token: DependencyLeaderToken, shape: ScheduledResourceShape);
}

#[cfg(test)]
struct ScheduledParseActiveGuard<'a>(&'a DomainInner);

#[cfg(test)]
impl Drop for ScheduledParseActiveGuard<'_> {
    fn drop(&mut self) {
        let mut domain = lock(&self.0.state);
        if domain.parse_active == 0 {
            domain.invariant_failed = true;
        } else {
            domain.parse_active -= 1;
        }
    }
}

#[cfg(not(test))]
trait DependencyHooks: Send + Sync {
    fn event(&self, event: DependencyEvent);
}

#[cfg(test)]
trait CloseEdgeHooks: Send + Sync {
    fn after_phase_replacement(&self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeaderPhase {
    BeforeRequest,
    QueuedBeforeWait,
    Granted,
    BeforeLoader,
    AfterLoaderResult,
    ReconciledBeforePublication,
}

#[cfg(test)]
trait LeaderPhaseHooks: Send + Sync {
    fn enter(&self, phase: LeaderPhase);
}

struct WaitEdgeGuard {
    hooks: Option<Arc<dyn WaitEdgeHooks>>,
    epoch: u64,
    id: ObjectId,
    generation: u64,
    ordinal: u64,
}

impl Drop for WaitEdgeGuard {
    fn drop(&mut self) {
        if let Some(hooks) = &self.hooks {
            hooks.remove(self.epoch, self.id, self.generation, self.ordinal);
        }
    }
}

#[derive(Default)]
struct DomainState {
    arenas: BTreeMap<u64, Weak<ArenaInner>>,
    cells: BTreeMap<CellKey, Arc<Cell>>,
    touch: u64,
    live_interests: usize,
    cache_bytes: u64,
    loading_metadata_bytes: u64,
    calls: u64,
    loads: u64,
    hits: u64,
    waits: u64,
    negative_hits: u64,
    transient_shares: u64,
    bypasses: u64,
    evictions: u64,
    cancellations: u64,
    closes: u64,
    dependency_cancellations: usize,
    dependency_pins: usize,
    dependency_edges: usize,
    dependency_edge_adds: u64,
    dependency_edge_removes: u64,
    dependency_cleanup_acks: u64,
    #[cfg(test)]
    active_generations: BTreeSet<DependencyLeaderToken>,
    #[cfg(test)]
    parse_active: usize,
    #[cfg(test)]
    close_acknowledgements: u64,
    dependency_leaders: usize,
    dependency_event_ordinal: u64,
    representations: [RepresentationSnapshot; Representation::COUNT],
    invariant_failed: bool,
}

macro_rules! representation_counter {
    ($method:ident, $field:ident) => {
        fn $method(&mut self, representation: Representation, amount: u64) -> bool {
            let Some(total) = self.$field.checked_add(amount) else {
                return false;
            };
            let counter = &mut self.representations[representation.index()].$field;
            let Some(kind) = counter.checked_add(amount) else {
                return false;
            };
            self.$field = total;
            *counter = kind;
            true
        }
    };
}

impl DomainState {
    representation_counter!(add_loads, loads);
    representation_counter!(add_bypasses, bypasses);
    representation_counter!(add_transient_shares, transient_shares);
    representation_counter!(add_evictions, evictions);
    representation_counter!(add_cancellations, cancellations);
}

#[derive(Clone)]
pub(crate) struct ObjectCellArena {
    inner: Arc<ArenaInner>,
}

struct ArenaInner {
    epoch: u64,
    operation: BrokerOperation,
    domain: Arc<DomainInner>,
    closed: AtomicBool,
    _metadata: Mutex<Option<RetainedCharge>>,
}

struct Cell {
    key: CellKey,
    state: Mutex<CellState>,
    ready: Condvar,
    metadata: Mutex<Option<RetainedCharge>>,
}

#[derive(Clone, Copy)]
struct InterestSlot {
    ordinal: u64,
    generation: u64,
}

const EMPTY_INTEREST: InterestSlot = InterestSlot {
    ordinal: 0,
    generation: 0,
};

impl InterestSlot {
    const fn is_active(self) -> bool {
        self.ordinal != 0
    }

    fn deactivate(&mut self) {
        self.ordinal = 0;
        self.generation = 0;
    }
}

struct LoadingState {
    generation: u64,
    leader_slot: usize,
    leader_running: bool,
    cancellation: Arc<AtomicBool>,
    broker_cancellation: Option<ReservationCancellation>,
    permit: Option<ScalarResolutionPermit>,
    dependency: Option<MemberDependencyState>,
    dependency_container: Option<ObjectId>,
}

struct MemberDependencyState {
    token: DependencyLeaderToken,
    edge: DependencyEdge,
    cancellation: Option<CellCancellation>,
    pin: Option<ResolvedObjectStreamPin>,
}

struct DetachedDependency {
    dependency: MemberDependencyState,
    event: Option<DependencyEvent>,
    ordinal_invariant: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellControlTag {
    ArenaClosed,
    DomainFull,
    LoadingMetadataLimit,
    MetadataAccountingOverflow,
    LoadingMetadataAccountingOverflow,
    InterestLimit,
    CellInterestLimit,
    InterestOrdinalOverflow,
    CellInterestOverflow,
    GlobalInterestOverflow,
    CellCallCounterOverflow,
    KindCallCounterOverflow,
    CellWaitCounterOverflow,
    KindWaitCounterOverflow,
    CellHitCounterOverflow,
    KindHitCounterOverflow,
    NegativeHitCounterOverflow,
    KindNegativeHitCounterOverflow,
    TransientShareCounterOverflow,
    KindTransientShareCounterOverflow,
    TouchSequenceOverflow,
    ArenaHeadroomOverflow,
    LoaderHeadroomUnderflow,
    LoaderHeadroomUnavailable,
    InterestCancelled,
    FlightCancelled,
    LoadGenerationOverflow,
    PayloadMismatch,
    PinAdmissionClosed,
    ExternalPinOverflow,
    RetainedWeightOverflow,
    PermitOrMeasurementInvariant,
    ObjectLimitProvenanceInvariant,
    StreamSpanInvariant,
    ObjectStreamBatchSetupInvariant,
    ObjectStreamCacheBypassInvariant,
    DependencyRejectedStale,
    DependencyStateInvariant,
    ErrorAccountingInvariant,
    LoadCounterOverflow,
}

impl CellControlTag {
    const fn detail(self) -> &'static str {
        match self {
            Self::ArenaClosed => "object cell arena is closed",
            Self::DomainFull => "object cell domain is full",
            Self::LoadingMetadataLimit => "object cell loading metadata limit reached",
            Self::MetadataAccountingOverflow => "object cell metadata accounting overflow",
            Self::LoadingMetadataAccountingOverflow => {
                "object cell loading metadata accounting overflow"
            }
            Self::InterestLimit => "object cell interest limit reached",
            Self::CellInterestLimit => "object cell has 64 live interests",
            Self::InterestOrdinalOverflow => "object cell interest ordinal overflow",
            Self::CellInterestOverflow => "cell interest overflow",
            Self::GlobalInterestOverflow => "global interest overflow",
            Self::CellCallCounterOverflow => "cell call counter overflow",
            Self::KindCallCounterOverflow => "kind call counter overflow",
            Self::CellWaitCounterOverflow => "cell wait counter overflow",
            Self::KindWaitCounterOverflow => "kind wait counter overflow",
            Self::CellHitCounterOverflow => "cell hit counter overflow",
            Self::KindHitCounterOverflow => "kind hit counter overflow",
            Self::NegativeHitCounterOverflow => "negative hit counter overflow",
            Self::KindNegativeHitCounterOverflow => "kind negative hit counter overflow",
            Self::TransientShareCounterOverflow => "transient share counter overflow",
            Self::KindTransientShareCounterOverflow => "kind transient share counter overflow",
            Self::TouchSequenceOverflow => "object cell touch sequence overflow",
            Self::ArenaHeadroomOverflow => "object cell arena headroom overflow",
            Self::LoaderHeadroomUnderflow => "broker in-flight estimate accounting underflow",
            Self::LoaderHeadroomUnavailable => "broker-global loader headroom is unavailable",
            Self::InterestCancelled => "object cell interest was cancelled",
            Self::FlightCancelled => "object cell flight was cancelled",
            Self::LoadGenerationOverflow => "object cell load generation overflow",
            Self::PayloadMismatch => "object cell payload representation mismatch",
            Self::PinAdmissionClosed => "object cell arena closed before external pin admission",
            Self::ExternalPinOverflow => "object cell external pin overflow",
            Self::RetainedWeightOverflow => "object-stream failure retained-weight overflow",
            Self::PermitOrMeasurementInvariant => "object-stream permit or measurement invariant",
            Self::ObjectLimitProvenanceInvariant => {
                "object-stream object-limit provenance invariant"
            }
            Self::StreamSpanInvariant => "object-stream stream-span invariant",
            Self::ObjectStreamBatchSetupInvariant => "object-stream batch-setup route invariant",
            Self::ObjectStreamCacheBypassInvariant => "object-stream cache-bypass route invariant",
            Self::DependencyRejectedStale => "stale member dependency operation rejected",
            Self::DependencyStateInvariant => "member dependency state invariant",
            Self::ErrorAccountingInvariant => "cell error accounting overflow",
            Self::LoadCounterOverflow => "object cell load counter overflow",
        }
    }

    const fn is_invariant(self) -> bool {
        matches!(
            self,
            Self::ArenaHeadroomOverflow
                | Self::LoaderHeadroomUnderflow
                | Self::RetainedWeightOverflow
                | Self::PayloadMismatch
                | Self::PermitOrMeasurementInvariant
                | Self::ObjectLimitProvenanceInvariant
                | Self::StreamSpanInvariant
                | Self::ObjectStreamBatchSetupInvariant
                | Self::ObjectStreamCacheBypassInvariant
                | Self::DependencyStateInvariant
                | Self::ErrorAccountingInvariant
                | Self::LoadCounterOverflow
        )
    }

    const fn from_objstm_invariant(stage: InvariantStage) -> Self {
        match stage {
            InvariantStage::PermitOrMeasurement => Self::PermitOrMeasurementInvariant,
            InvariantStage::ObjectLimitProvenance => Self::ObjectLimitProvenanceInvariant,
            InvariantStage::StreamSpan => Self::StreamSpanInvariant,
            InvariantStage::ObjectStreamBatchSetup => Self::ObjectStreamBatchSetupInvariant,
            InvariantStage::ObjectStreamCacheBypass => Self::ObjectStreamCacheBypassInvariant,
            InvariantStage::RetainedWeightOverflow => Self::RetainedWeightOverflow,
            InvariantStage::PayloadMismatch => Self::PayloadMismatch,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CellControlDetail {
    Static(CellControlTag),
    Broker(BrokerError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CellControlFailure {
    id: ObjectId,
    kind: AccessKind,
    detail: CellControlDetail,
}

impl CellControlFailure {
    const fn new(id: ObjectId, kind: AccessKind, tag: CellControlTag) -> Self {
        Self {
            id,
            kind,
            detail: CellControlDetail::Static(tag),
        }
    }

    fn broker(id: ObjectId, error: BrokerError) -> Self {
        let kind = match error {
            BrokerError::Closed | BrokerError::OperationClosed | BrokerError::Cancelled => {
                AccessKind::Backend
            }
            _ => AccessKind::ResourceLimit,
        };
        Self {
            id,
            kind,
            detail: CellControlDetail::Broker(error),
        }
    }

    fn render(&self) -> AccessError {
        match &self.detail {
            CellControlDetail::Static(tag) => cell_error(self.id, self.kind, tag.detail()),
            CellControlDetail::Broker(error) => cell_error(self.id, self.kind, error.to_string()),
        }
    }

    fn is_invariant(&self) -> bool {
        matches!(self.detail, CellControlDetail::Static(tag) if tag.is_invariant())
    }

    #[cfg(test)]
    fn into_access(self) -> AccessError {
        self.render()
    }
}

enum CellPhase {
    Loading(LoadingState),
    Ready(Arc<ResolvedObjectOwner>),
    Negative(Arc<FailureOwner>),
    FlightError(Arc<FailureOwner>),
    Closed(CellControlFailure),
}

struct CellState {
    phase: CellPhase,
    dependency_transition: DependencyTransition,
    interests: [InterestSlot; MAX_INTERESTS_PER_CELL],
    live_interests: usize,
    next_interest_ordinal: u64,
    touch: u64,
    cached: bool,
    transitioning: bool,
    external_pins: usize,
    completed_weight: u64,
}

pub(crate) struct ObjectCellRequest {
    arena: Arc<ArenaInner>,
    cell: Arc<Cell>,
    slot: usize,
    ordinal: u64,
    completed: bool,
}

#[derive(Clone)]
pub(crate) struct CellCancellation {
    arena: Weak<ArenaInner>,
    cell: Weak<Cell>,
    slot: usize,
    ordinal: u64,
}

pub(crate) struct ResolvedObjectOwner {
    payload: CellPayload,
    #[cfg(test)]
    permit: ScalarResolutionPermit,
    transition_gate: Mutex<()>,
    cache_backed: AtomicBool,
    charge: Mutex<Option<RetainedCharge>>,
    self_pin: Mutex<Option<SelfPinCharge>>,
}

enum CellPayload {
    Object(BoundedObject),
    ObjectStream(BoundedObjectStream),
}

#[derive(Clone)]
pub(crate) struct ResolvedObjectPin {
    inner: ResolvedCellPin,
}

pub(crate) struct ResolvedObjectStreamPin {
    inner: ResolvedCellPin,
}

#[derive(Clone)]
struct ResolvedCellPin {
    owner: Arc<ResolvedObjectOwner>,
    _pin: Arc<ExternalPin>,
}

impl std::fmt::Debug for ResolvedCellPin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedCellPin")
            .field("retained_bytes", &self.owner.retained_bytes())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ResolvedObjectPin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedObjectPin")
            .field("retained_bytes", &self.inner.owner.retained_bytes())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ResolvedObjectStreamPin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedObjectStreamPin")
            .field("retained_bytes", &self.inner.owner.retained_bytes())
            .finish_non_exhaustive()
    }
}

struct ExternalPin {
    arena: Weak<ArenaInner>,
    cell: Weak<Cell>,
    owner: Arc<ResolvedObjectOwner>,
    cached: bool,
}

pub(crate) struct FailureOwner {
    payload: FailurePayload,
    retained_weight: u64,
    charge: Mutex<Option<RetainedCharge>>,
    _reservation: Mutex<Option<crate::broker::Reservation>>,
    cell_envelope: bool,
}

pub(crate) enum ContainerCellError {
    Shared(Arc<FailureOwner>),
    Control(CellControlFailure),
}

impl std::fmt::Debug for ContainerCellError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shared(owner) => formatter
                .debug_tuple("Shared")
                .field(&Arc::as_ptr(owner))
                .finish(),
            Self::Control(control) => formatter.debug_tuple("Control").field(control).finish(),
        }
    }
}

impl ContainerCellError {
    #[cfg(test)]
    pub(crate) fn into_access_for_test(self) -> AccessError {
        match self {
            Self::Shared(_) => panic!("test requested rendering at the neutral shared boundary"),
            Self::Control(control) => control.render(),
        }
    }

    #[cfg(test)]
    fn shared_pointer(&self) -> Option<usize> {
        match self {
            Self::Shared(owner) => Some(Arc::as_ptr(owner) as usize),
            Self::Control(_) => None,
        }
    }
}

enum ExactPhaseExpectation<'a> {
    Loading {
        leader_slot: Option<usize>,
        generation: Option<u64>,
        require_publication: bool,
    },
    Ready {
        owner: Option<&'a Arc<ResolvedObjectOwner>>,
    },
    Negative {
        owner: Option<&'a Arc<FailureOwner>>,
    },
    FlightError {
        owner: Option<&'a Arc<FailureOwner>>,
    },
}

impl ExactPhaseExpectation<'_> {
    fn matches(&self, state: &CellState) -> bool {
        match (self, &state.phase) {
            (
                Self::Loading {
                    leader_slot,
                    generation,
                    require_publication,
                },
                CellPhase::Loading(loading),
            ) => {
                leader_slot.is_none_or(|slot| loading.leader_slot == slot)
                    && generation.is_none_or(|value| loading.generation == value)
                    && (!require_publication
                        || (!loading.cancellation.load(Ordering::Acquire)
                            && state
                                .interests
                                .get(loading.leader_slot)
                                .is_some_and(|interest| interest.is_active())))
            }
            (Self::Ready { owner }, CellPhase::Ready(current)) => {
                owner.is_none_or(|expected| Arc::ptr_eq(expected, current))
            }
            (Self::Negative { owner }, CellPhase::Negative(current)) => {
                owner.is_none_or(|expected| Arc::ptr_eq(expected, current))
            }
            (Self::FlightError { owner }, CellPhase::FlightError(current)) => {
                owner.is_none_or(|expected| Arc::ptr_eq(expected, current))
            }
            _ => false,
        }
    }
}

const CELL_FIXED_STRUCTURAL_BYTES: usize = std::mem::size_of::<Cell>()
    + std::mem::size_of::<ResolvedObjectOwner>()
    + BTREE_NODE_ENVELOPE_BYTES
    + PREPARED_STREAM_STRUCTURAL_BYTES
    + ARC_ALLOCATION_HEADERS_BYTES;
const _: () = assert!(CELL_FIXED_STRUCTURAL_BYTES <= CELL_BASE_METADATA_BYTES as usize);
const _: () = assert!(
    CELL_FIXED_STRUCTURAL_BYTES
        > CELL_BASE_METADATA_BYTES as usize - CELL_ERROR_ENVELOPE_BYTES as usize
);
const _: () = assert!(std::mem::size_of::<FailureOwner>() <= ERROR_OWNER_BYTES as usize);
const _: () =
    assert!(std::mem::size_of::<CellControlFailure>() <= CELL_ERROR_ENVELOPE_BYTES as usize);

enum ResolveStep {
    Lead {
        generation: u64,
        cancellation: Arc<AtomicBool>,
    },
    Ready(Arc<ResolvedObjectOwner>),
    Error(Arc<FailureOwner>),
    Closed(CellControlFailure),
    Control(CellControlFailure),
    Wait,
}

enum CellResolveResult {
    Ready(ResolvedCellPin),
    SharedFailure(Arc<FailureOwner>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterestRelease {
    Complete,
    Cancel,
    Abandon,
}

impl ObjectCellDomain {
    pub(crate) fn production() -> &'static Self {
        static DOMAIN: OnceLock<ObjectCellDomain> = OnceLock::new();
        DOMAIN.get_or_init(|| {
            Self::new(
                BudgetBroker::production().clone(),
                ObjectCellConfig::production(),
            )
        })
    }

    pub(crate) fn new(broker: BudgetBroker, config: ObjectCellConfig) -> Self {
        Self {
            inner: Arc::new(DomainInner {
                broker,
                config,
                admission: Mutex::new(()),
                headroom: Mutex::new(()),
                wait_hooks: Mutex::new(None),
                dependency_hooks: Mutex::new(None),
                #[cfg(test)]
                dependency_wait_hooks: Mutex::new(None),
                #[cfg(test)]
                dependency_cleanup_failure_hooks: Mutex::new(None),
                #[cfg(test)]
                dependency_terminal_cleanup_hooks: Mutex::new(None),
                #[cfg(test)]
                dependency_pre_wait_hooks: Mutex::new(None),
                #[cfg(test)]
                close_hooks: Mutex::new(None),
                #[cfg(test)]
                leader_phase_hooks: Mutex::new(None),
                #[cfg(test)]
                failure_delivery_hooks: Mutex::new(None),
                #[cfg(test)]
                scheduled_resource_hooks: Mutex::new(None),
                state: Mutex::new(DomainState::default()),
            }),
        }
    }

    pub(crate) fn open_arena(&self) -> Result<ObjectCellArena, AccessError> {
        let result = {
            let _headroom = lock(&self.inner.headroom);
            (|| -> Result<ObjectCellArena, CellControlFailure> {
                let operation_metadata = self
                    .inner
                    .broker
                    .normal_headroom()
                    .operation_metadata_weight;
                let arena_headroom = ARENA_METADATA_BYTES
                    .checked_add(operation_metadata)
                    .ok_or_else(|| {
                        CellControlFailure::new(
                            (0, 0),
                            AccessKind::ResourceLimit,
                            CellControlTag::ArenaHeadroomOverflow,
                        )
                    })?;
                self.inner.reclaim_loader_headroom(
                    arena_headroom,
                    LOADER_ESTIMATE_BYTES,
                    (0, 0),
                )?;
                let operation = self
                    .inner
                    .broker
                    .register_operation()
                    .map_err(|error| CellControlFailure::broker((0, 0), error))?;
                let epoch = operation.id();
                let reservation = operation
                    .reserve(
                        Lane::Normal {
                            completion_reserve: 0,
                        },
                        ARENA_METADATA_BYTES,
                    )
                    .map_err(|error| CellControlFailure::broker((0, 0), error))?;
                let mut metadata = reservation
                    .reconcile(ARENA_METADATA_BYTES)
                    .map_err(|error| CellControlFailure::broker((0, 0), error))?;
                metadata
                    .transition(OwnershipClass::Cache)
                    .map_err(|error| CellControlFailure::broker((0, 0), error))?;
                let arena = Arc::new(ArenaInner {
                    epoch,
                    operation,
                    domain: Arc::clone(&self.inner),
                    closed: AtomicBool::new(false),
                    _metadata: Mutex::new(Some(metadata)),
                });
                lock(&self.inner.state)
                    .arenas
                    .insert(epoch, Arc::downgrade(&arena));
                Ok(ObjectCellArena { inner: arena })
            })()
        };
        result.map_err(|control| control.render())
    }

    pub(crate) fn snapshot(&self) -> ObjectCellSnapshot {
        self.inner.snapshot()
    }

    #[cfg(test)]
    pub(crate) fn set_wait_hooks(&self, hooks: Arc<dyn WaitEdgeHooks>) {
        *lock(&self.inner.wait_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_dependency_hooks(&self, hooks: Arc<dyn DependencyHooks>) {
        *lock(&self.inner.dependency_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_dependency_wait_hooks(&self, hooks: Arc<dyn DependencyWaitHooks>) {
        *lock(&self.inner.dependency_wait_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_failure_delivery_hooks(&self, hooks: Arc<dyn FailureDeliveryHooks>) {
        *lock(&self.inner.failure_delivery_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_scheduled_resource_hooks(&self, hooks: Arc<dyn ScheduledResourceHooks>) {
        *lock(&self.inner.scheduled_resource_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_dependency_cleanup_failure_hooks(&self, hooks: Arc<dyn DependencyCleanupFailureHooks>) {
        *lock(&self.inner.dependency_cleanup_failure_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_dependency_terminal_cleanup_hooks(
        &self,
        hooks: Arc<dyn DependencyTerminalCleanupHooks>,
    ) {
        *lock(&self.inner.dependency_terminal_cleanup_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_dependency_pre_wait_hooks(&self, hooks: Arc<dyn DependencyPreWaitHooks>) {
        *lock(&self.inner.dependency_pre_wait_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_close_hooks(&self, hooks: Arc<dyn CloseEdgeHooks>) {
        *lock(&self.inner.close_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_leader_phase_hooks(&self, hooks: Arc<dyn LeaderPhaseHooks>) {
        *lock(&self.inner.leader_phase_hooks) = Some(hooks);
    }
}

impl ObjectCellArena {
    pub(crate) fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    pub(crate) fn request(&self, id: ObjectId) -> Result<ObjectCellRequest, AccessError> {
        self.inner.request(id).map_err(|error| error.render())
    }

    pub(crate) fn resolve<F>(
        &self,
        id: ObjectId,
        loader: F,
    ) -> Result<ResolvedObjectPin, AccessError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObject, CellLoadError>,
    {
        self.request(id)?.resolve(loader)
    }

    pub(crate) fn resolve_object_stream<F>(
        &self,
        id: ObjectId,
        loader: F,
    ) -> Result<ResolvedObjectStreamPin, ContainerCellError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObjectStream, CellLoadError>,
    {
        self.inner
            .request_representation(id, Representation::DeclaredObjStmContainer)
            .map_err(ContainerCellError::Control)?
            .resolve_object_stream(loader)
    }

    pub(crate) fn resolve_declared_member<F>(
        &self,
        id: ObjectId,
        loader: F,
    ) -> Result<ResolvedObjectPin, AccessError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObject, CellLoadError>,
    {
        self.inner
            .request_representation(id, Representation::DeclaredObjStmMember)
            .map_err(|error| error.render())?
            .resolve(loader)
    }

    #[cfg(test)]
    pub(crate) fn close(&self) {
        self.inner.close();
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> ObjectCellSnapshot {
        self.inner.domain.snapshot()
    }
}

#[cfg(test)]
impl ObjectCellArena {
    pub(crate) fn set_wait_hooks(&self, hooks: Arc<dyn WaitEdgeHooks>) {
        *lock(&self.inner.domain.wait_hooks) = Some(hooks);
    }

    pub(crate) fn active_container_permit_stats(
        &self,
        id: ObjectId,
    ) -> Option<ScalarResolutionStats> {
        let key = CellKey {
            epoch: self.inner.epoch,
            id,
            representation: Representation::DeclaredObjStmContainer,
        };
        let cell = lock(&self.inner.domain.state).cells.get(&key).cloned()?;
        let state = lock(&cell.state);
        let CellPhase::Loading(loading) = &state.phase else {
            return None;
        };
        loading.permit.as_ref().map(ScalarResolutionPermit::stats)
    }

    pub(crate) fn container_retained_evidence(
        &self,
        id: ObjectId,
    ) -> Option<(u64, ScalarResolutionPermit, u64)> {
        let key = CellKey {
            epoch: self.inner.epoch,
            id,
            representation: Representation::DeclaredObjStmContainer,
        };
        let cell = lock(&self.inner.domain.state).cells.get(&key).cloned()?;
        let state = lock(&cell.state);
        let CellPhase::Ready(owner) = &state.phase else {
            return None;
        };
        let charge = lock(&owner.charge).as_ref()?.bytes();
        Some((
            owner.retained_bytes(),
            test_clone_permit(&owner.permit),
            charge,
        ))
    }

    pub(crate) fn has_container_cell(&self, id: ObjectId) -> bool {
        let key = CellKey {
            epoch: self.inner.epoch,
            id,
            representation: Representation::DeclaredObjStmContainer,
        };
        lock(&self.inner.domain.state).cells.contains_key(&key)
    }
}

impl ObjectCellRequest {
    pub(crate) fn cancellation_handle(&self) -> CellCancellation {
        CellCancellation {
            arena: Arc::downgrade(&self.arena),
            cell: Arc::downgrade(&self.cell),
            slot: self.slot,
            ordinal: self.ordinal,
        }
    }

    #[cfg(test)]
    fn resolve_dependency_synthetic(mut self, container: ObjectId) -> Result<(), AccessError> {
        loop {
            let step = {
                let mut state = lock(&self.cell.state);
                if state.interests.get(self.slot).is_none_or(|interest| {
                    !interest.is_active() || interest.ordinal != self.ordinal
                }) {
                    self.completed = true;
                    ResolveStep::Control(CellControlFailure::new(
                        self.cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::InterestCancelled,
                    ))
                } else {
                    let dependency_idle = state.dependency_transition.is_idle();
                    match &mut state.phase {
                        CellPhase::Loading(loading)
                            if dependency_idle
                                && loading.leader_slot == self.slot
                                && !loading.leader_running
                                && !self.arena.closed.load(Ordering::Acquire) =>
                        {
                            loading.leader_running = true;
                            ResolveStep::Lead {
                                generation: loading.generation,
                                cancellation: test_arc_clone(&loading.cancellation),
                            }
                        }
                        CellPhase::Loading(_) => ResolveStep::Wait,
                        CellPhase::Ready(_)
                        | CellPhase::Negative(_)
                        | CellPhase::FlightError(_)
                        | CellPhase::Closed(_)
                            if !dependency_idle =>
                        {
                            ResolveStep::Wait
                        }
                        CellPhase::Ready(_)
                        | CellPhase::Negative(_)
                        | CellPhase::FlightError(_) => {
                            ResolveStep::Control(CellControlFailure::new(
                                self.cell.key.id,
                                AccessKind::Backend,
                                CellControlTag::DependencyStateInvariant,
                            ))
                        }
                        CellPhase::Closed(error) => ResolveStep::Closed(error.clone()),
                    }
                }
            };
            match step {
                ResolveStep::Lead {
                    generation,
                    cancellation,
                } => {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.run_dependency_synthetic(generation, &cancellation, container)
                    }));
                    match outcome {
                        Ok(result) => {
                            self.finish_interest();
                            self.completed = true;
                            self.arena
                                .leader_terminal(&self.cell, self.slot, generation);
                            return result.map_err(|control| control.render());
                        }
                        Err(payload) => {
                            self.arena.release_interest(
                                &self.cell,
                                self.slot,
                                self.ordinal,
                                InterestRelease::Abandon,
                            );
                            self.completed = true;
                            self.arena
                                .leader_terminal(&self.cell, self.slot, generation);
                            std::panic::resume_unwind(payload);
                        }
                    }
                }
                ResolveStep::Closed(error) | ResolveStep::Control(error) => {
                    self.finish_interest();
                    self.completed = true;
                    return Err(error.render());
                }
                ResolveStep::Wait => {
                    let transition_state = lock(&self.cell.state);
                    if !transition_state.dependency_transition.is_idle() {
                        drop(transition_state);
                        self.arena.wait_for_dependency_transition(&self.cell);
                        continue;
                    }
                    drop(transition_state);
                    let generation = {
                        let state = lock(&self.cell.state);
                        match &state.phase {
                            CellPhase::Loading(loading)
                                if !(loading.leader_slot == self.slot
                                    && !loading.leader_running)
                                    && state.interests.get(self.slot).is_some_and(|interest| {
                                        interest.is_active() && interest.ordinal == self.ordinal
                                    }) =>
                            {
                                Some(loading.generation)
                            }
                            _ => None,
                        }
                    };
                    if let Some(generation) = generation {
                        let edge =
                            self.arena
                                .domain
                                .wait_edge(self.cell.key, generation, self.ordinal);
                        let mut state = lock(&self.cell.state);
                        if matches!(
                            &state.phase,
                            CellPhase::Loading(loading)
                                if !(loading.leader_slot == self.slot && !loading.leader_running)
                        ) && state.interests.get(self.slot).is_some_and(|interest| {
                            interest.is_active() && interest.ordinal == self.ordinal
                        }) {
                            state = wait(&self.cell.ready, state);
                        }
                        drop(state);
                        drop(edge);
                    }
                }
                ResolveStep::Ready(_) | ResolveStep::Error(_) => {
                    unreachable!("synthetic dependency flight has no published payload")
                }
            }
        }
    }

    #[cfg(test)]
    fn run_dependency_synthetic(
        &self,
        generation: u64,
        cancellation: &Arc<AtomicBool>,
        container: ObjectId,
    ) -> Result<(), CellControlFailure> {
        self.prepare_dependency_synthetic(
            generation,
            cancellation,
            container,
            |_| panic!("C1 synthetic dependency requires an already prepared container"),
            false,
        )
    }

    #[cfg(test)]
    fn prepare_dependency_synthetic<F>(
        &self,
        generation: u64,
        cancellation: &Arc<AtomicBool>,
        container: ObjectId,
        child_loader: F,
        c2_ordered_evidence: bool,
    ) -> Result<(), CellControlFailure>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObjectStream, CellLoadError>,
    {
        let token = DependencyLeaderToken {
            key: self.cell.key,
            generation,
            leader_slot: self.slot,
        };
        let edge = DependencyEdge {
            from: token.key,
            to: CellKey {
                epoch: token.key.epoch,
                id: container,
                representation: Representation::DeclaredObjStmContainer,
            },
        };
        if let Some(panic) = self.arena.announce_dependency_event(
            &self.cell,
            token,
            edge,
            DependencyEventKind::BeforeChildInstall,
        )? {
            resume_unwind(panic);
        }
        if cancellation.load(Ordering::Acquire) {
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::InterestCancelled,
            ));
        }
        let child = self
            .arena
            .request_representation(container, Representation::DeclaredObjStmContainer)?;
        let child_cancellation = child.cancellation_handle();
        match self.arena.install_dependency_cancellation(
            &self.cell,
            token,
            container,
            child_cancellation,
        ) {
            Ok(Some(panic)) => resume_unwind(panic),
            Ok(None) => {}
            Err((child_cancellation, control)) => {
                child_cancellation.cancel();
                return Err(control);
            }
        }
        if let Some(panic) = self.arena.announce_dependency_event(
            &self.cell,
            token,
            edge,
            DependencyEventKind::InsideChildResolve,
        )? {
            resume_unwind(panic);
        }
        let child_result = child.resolve_object_stream(child_loader);
        if c2_ordered_evidence {
            if let Some(panic) = self.arena.announce_dependency_event(
                &self.cell,
                token,
                edge,
                DependencyEventKind::ChildResolved,
            )? {
                resume_unwind(panic);
            }
        }
        let Some(child_handle) = self.arena.take_dependency_cancellation(&self.cell, token)? else {
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::DependencyRejectedStale,
            ));
        };
        drop(child_handle);
        let taken_kind = if c2_ordered_evidence {
            DependencyEventKind::ChildHandleTaken
        } else {
            DependencyEventKind::ChildResolved
        };
        if let Some(panic) = self
            .arena
            .announce_dependency_event(&self.cell, token, edge, taken_kind)?
        {
            resume_unwind(panic);
        }
        let pin = child_result.map_err(|error| match error {
            ContainerCellError::Control(control) => control,
            ContainerCellError::Shared(_) => CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::DependencyStateInvariant,
            ),
        })?;
        match self.arena.install_dependency_pin(&self.cell, token, pin) {
            Ok(Some(panic)) => resume_unwind(panic),
            Ok(None) => {}
            Err((pin, control)) => {
                drop(pin);
                return Err(control);
            }
        }
        if let Some(panic) = self.arena.announce_dependency_event(
            &self.cell,
            token,
            edge,
            DependencyEventKind::BeforeMemberAdmission,
        )? {
            resume_unwind(panic);
        }
        if cancellation.load(Ordering::Acquire) {
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::InterestCancelled,
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn resolve_scheduled_member_synthetic<CF, MF>(
        mut self,
        container: ObjectId,
        child_loader: CF,
        member_loader: MF,
    ) -> Result<ResolvedObjectPin, AccessError>
    where
        CF: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObjectStream, CellLoadError>,
        MF: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObject, CellLoadError>,
    {
        let mut child_loader = Some(child_loader);
        let mut member_loader = Some(member_loader);
        loop {
            let step = {
                let mut state = lock(&self.cell.state);
                if state.interests.get(self.slot).is_none_or(|interest| {
                    !interest.is_active() || interest.ordinal != self.ordinal
                }) {
                    self.completed = true;
                    ResolveStep::Control(CellControlFailure::new(
                        self.cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::InterestCancelled,
                    ))
                } else {
                    let dependency_idle = state.dependency_transition.is_idle();
                    match &mut state.phase {
                        CellPhase::Loading(loading)
                            if dependency_idle
                                && loading.leader_slot == self.slot
                                && !loading.leader_running
                                && !self.arena.closed.load(Ordering::Acquire) =>
                        {
                            loading.leader_running = true;
                            ResolveStep::Lead {
                                generation: loading.generation,
                                cancellation: test_arc_clone(&loading.cancellation),
                            }
                        }
                        CellPhase::Loading(_) => ResolveStep::Wait,
                        CellPhase::Ready(_)
                        | CellPhase::Negative(_)
                        | CellPhase::FlightError(_)
                        | CellPhase::Closed(_)
                            if !dependency_idle =>
                        {
                            ResolveStep::Wait
                        }
                        CellPhase::Ready(owner) => ResolveStep::Ready(test_arc_clone(owner)),
                        CellPhase::Negative(error) | CellPhase::FlightError(error) => {
                            ResolveStep::Error(test_arc_clone(error))
                        }
                        CellPhase::Closed(error) => ResolveStep::Closed(test_clone_value(error)),
                    }
                }
            };
            match step {
                ResolveStep::Lead {
                    generation,
                    cancellation,
                } => {
                    let child_loader = child_loader
                        .take()
                        .expect("one child loader invocation per member interest");
                    let member_loader = member_loader
                        .take()
                        .expect("one member loader invocation per member interest");
                    self.run_scheduled_member_synthetic(
                        generation,
                        &cancellation,
                        container,
                        child_loader,
                        member_loader,
                    );
                }
                ResolveStep::Ready(owner) => {
                    if !matches!(&owner.payload, CellPayload::Object(_)) {
                        let failure = self.arena.invalidate_payload_mismatch(&self.cell, &owner);
                        self.finish_interest();
                        self.completed = true;
                        return Err(failure.render());
                    }
                    let pin = self
                        .arena
                        .pin(&self.cell, owner)
                        .map_err(|control| control.render())?;
                    self.finish_interest();
                    self.completed = true;
                    return Ok(ResolvedObjectPin { inner: pin });
                }
                ResolveStep::Error(error) => {
                    let hooks = test_locked_clone(&self.arena.domain.failure_delivery_hooks);
                    if let Some(hooks) = hooks {
                        hooks.delivered(self.cell.key, Arc::as_ptr(&error) as usize);
                    }
                    self.finish_interest();
                    self.completed = true;
                    return match &error.payload {
                        FailurePayload::Access(error) => Err(test_clone_value(error)),
                        FailurePayload::ObjStm(_) => Err(self
                            .arena
                            .invalidate_failure_payload_mismatch(&self.cell, &error)
                            .render()),
                    };
                }
                ResolveStep::Closed(error) | ResolveStep::Control(error) => {
                    self.finish_interest();
                    self.completed = true;
                    return Err(error.render());
                }
                ResolveStep::Wait => {
                    let transition_state = lock(&self.cell.state);
                    if !transition_state.dependency_transition.is_idle() {
                        drop(transition_state);
                        self.arena.wait_for_dependency_transition(&self.cell);
                        continue;
                    }
                    drop(transition_state);
                    let generation = {
                        let state = lock(&self.cell.state);
                        match &state.phase {
                            CellPhase::Loading(loading)
                                if !(loading.leader_slot == self.slot
                                    && !loading.leader_running)
                                    && state.interests.get(self.slot).is_some_and(|interest| {
                                        interest.is_active() && interest.ordinal == self.ordinal
                                    }) =>
                            {
                                Some(loading.generation)
                            }
                            _ => None,
                        }
                    };
                    if let Some(generation) = generation {
                        let edge =
                            self.arena
                                .domain
                                .wait_edge(self.cell.key, generation, self.ordinal);
                        let mut state = lock(&self.cell.state);
                        if matches!(
                            &state.phase,
                            CellPhase::Loading(loading)
                                if !(loading.leader_slot == self.slot && !loading.leader_running)
                        ) && state.interests.get(self.slot).is_some_and(|interest| {
                            interest.is_active() && interest.ordinal == self.ordinal
                        }) {
                            state = wait(&self.cell.ready, state);
                        }
                        drop(state);
                        drop(edge);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn run_scheduled_member_synthetic<CF, MF>(
        &self,
        generation: u64,
        cancellation: &Arc<AtomicBool>,
        container: ObjectId,
        child_loader: CF,
        member_loader: MF,
    ) where
        CF: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObjectStream, CellLoadError>,
        MF: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObject, CellLoadError>,
    {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            self.run_scheduled_member_attempt(
                generation,
                cancellation,
                container,
                child_loader,
                member_loader,
            )
        }));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(control)) => {
                if self.arena.closed.load(Ordering::Acquire) {
                    if !self.arena.close_exact_control(
                        &self.cell,
                        self.slot,
                        generation,
                        CellControlFailure::new(
                            self.cell.key.id,
                            AccessKind::Backend,
                            CellControlTag::ArenaClosed,
                        ),
                        false,
                    ) {
                        self.arena
                            .leader_terminal(&self.cell, self.slot, generation);
                    }
                } else if control.is_invariant() {
                    if !self
                        .arena
                        .close_exact_control(&self.cell, self.slot, generation, control, true)
                    {
                        self.arena
                            .leader_terminal(&self.cell, self.slot, generation);
                    }
                } else {
                    self.arena
                        .leader_terminal(&self.cell, self.slot, generation);
                }
            }
            Err(payload) => {
                self.arena.release_interest(
                    &self.cell,
                    self.slot,
                    self.ordinal,
                    InterestRelease::Abandon,
                );
                self.arena
                    .leader_terminal(&self.cell, self.slot, generation);
                resume_unwind(payload);
            }
        }
    }

    #[cfg(test)]
    fn announce_scheduled_resource_shape(
        &self,
        token: DependencyLeaderToken,
        shape: ScheduledResourceShape,
    ) {
        let hooks = test_locked_clone(&self.arena.domain.scheduled_resource_hooks);
        if let Some(hooks) = hooks {
            hooks.enter(self.cell.key, token, shape);
        }
    }

    #[cfg(test)]
    fn announce_scheduled_resource_drop(
        &self,
        token: DependencyLeaderToken,
        shape: ScheduledResourceShape,
    ) {
        let hooks = test_locked_clone(&self.arena.domain.scheduled_resource_hooks);
        if let Some(hooks) = hooks {
            hooks.dropped(self.cell.key, token, shape);
        }
    }

    #[cfg(test)]
    fn run_scheduled_member_attempt<CF, MF>(
        &self,
        generation: u64,
        cancellation: &Arc<AtomicBool>,
        container: ObjectId,
        child_loader: CF,
        member_loader: MF,
    ) -> Result<(), CellControlFailure>
    where
        CF: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObjectStream, CellLoadError>,
        MF: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObject, CellLoadError>,
    {
        let token = DependencyLeaderToken {
            key: self.cell.key,
            generation,
            leader_slot: self.slot,
        };
        let edge = DependencyEdge {
            from: token.key,
            to: CellKey {
                epoch: token.key.epoch,
                id: container,
                representation: Representation::DeclaredObjStmContainer,
            },
        };
        self.prepare_dependency_synthetic(generation, cancellation, container, child_loader, true)?;
        if cancellation.load(Ordering::Acquire) || self.arena.closed.load(Ordering::Acquire) {
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::InterestCancelled,
            ));
        }
        let estimate = Representation::DeclaredObjStmMember.loader_estimate();
        let pending = {
            let headroom = lock(&self.arena.domain.headroom);
            if let Err(control) =
                self.arena
                    .domain
                    .reclaim_loader_headroom(0, estimate, self.cell.key.id)
            {
                drop(headroom);
                if cancellation.load(Ordering::Acquire) {
                    self.arena
                        .leader_terminal(&self.cell, self.slot, generation);
                } else if self.arena.closed.load(Ordering::Acquire) {
                    self.arena.close_exact_control(
                        &self.cell,
                        self.slot,
                        generation,
                        CellControlFailure::new(
                            self.cell.key.id,
                            AccessKind::Backend,
                            CellControlTag::ArenaClosed,
                        ),
                        false,
                    );
                } else if control.is_invariant() {
                    self.arena
                        .close_exact_control(&self.cell, self.slot, generation, control, true);
                } else {
                    self.arena.publish_cell_envelope_control_error(
                        &self.cell, self.slot, generation, control,
                    );
                    if let Some(panic) = self
                        .arena
                        .announce_member_published(&self.cell, token, edge)?
                    {
                        resume_unwind(panic);
                    }
                }
                return Ok(());
            }
            let pending = match self.arena.operation.request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                estimate,
            ) {
                Ok(pending) => pending,
                Err(error) => {
                    drop(headroom);
                    self.publish_broker_error(generation, error);
                    if let Some(panic) = self
                        .arena
                        .announce_member_published(&self.cell, token, edge)?
                    {
                        resume_unwind(panic);
                    }
                    return Ok(());
                }
            };
            drop(headroom);
            pending
        };
        if let Some(panic) = self.arena.announce_dependency_event(
            &self.cell,
            token,
            edge,
            DependencyEventKind::MemberRequestCreated,
        )? {
            resume_unwind(panic);
        }
        if cancellation.load(Ordering::Acquire) || self.arena.closed.load(Ordering::Acquire) {
            pending.cancel();
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::InterestCancelled,
            ));
        }
        let queued = pending.diagnostic_state() == PendingReservationDiagnostic::Queued;
        let broker_cancellation = pending.cancellation_handle();
        self.arena
            .install_member_broker_cancellation(
                &self.cell,
                token,
                test_clone_value(&broker_cancellation),
            )
            .map_err(|(returned, control)| {
                returned.cancel();
                control
            })?;
        self.announce_scheduled_resource_shape(token, ScheduledResourceShape::Requested);
        if queued {
            if let Some(panic) = self.arena.announce_dependency_event(
                &self.cell,
                token,
                edge,
                DependencyEventKind::MemberQueued,
            )? {
                resume_unwind(panic);
            }
        }
        if cancellation.load(Ordering::Acquire) || self.arena.closed.load(Ordering::Acquire) {
            broker_cancellation.cancel();
        }
        let reservation = match pending.wait() {
            Ok(reservation) => reservation,
            Err(error) => {
                self.publish_broker_error(generation, error);
                if !cancellation.load(Ordering::Acquire)
                    && !self.arena.closed.load(Ordering::Acquire)
                {
                    if let Some(panic) = self
                        .arena
                        .announce_member_published(&self.cell, token, edge)?
                    {
                        resume_unwind(panic);
                    }
                }
                return Ok(());
            }
        };
        if cancellation.load(Ordering::Acquire) || self.arena.closed.load(Ordering::Acquire) {
            reservation.cancel();
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::InterestCancelled,
            ));
        }
        let permit = ScalarResolutionPermit::new(estimate);
        self.arena
            .install_member_permit(&self.cell, token, test_clone_permit(&permit))
            .map_err(|(returned, control)| {
                returned.cancel();
                control
            })?;
        if let Some(panic) = self.arena.announce_dependency_event(
            &self.cell,
            token,
            edge,
            DependencyEventKind::MemberGranted,
        )? {
            resume_unwind(panic);
        }
        self.announce_scheduled_resource_shape(token, ScheduledResourceShape::Granted);
        if cancellation.load(Ordering::Acquire) || self.arena.closed.load(Ordering::Acquire) {
            reservation.cancel();
            permit.cancel();
            let finished_permit = self
                .arena
                .take_member_permit_after_attempt(&self.cell, token)?;
            drop(finished_permit);
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::InterestCancelled,
            ));
        }
        {
            let mut domain = lock(&self.arena.domain.state);
            if !domain.add_loads(self.cell.key.representation, 1) {
                drop(domain);
                return Err(CellControlFailure::new(
                    self.cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::LoadCounterOverflow,
                ));
            }
        }
        if let Some(panic) = self.arena.announce_dependency_event(
            &self.cell,
            token,
            edge,
            DependencyEventKind::MemberParseEnter,
        )? {
            resume_unwind(panic);
        }
        if cancellation.load(Ordering::Acquire) || self.arena.closed.load(Ordering::Acquire) {
            reservation.cancel();
            permit.cancel();
            let finished_permit = self
                .arena
                .take_member_permit_after_attempt(&self.cell, token)?;
            drop(finished_permit);
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::InterestCancelled,
            ));
        }
        {
            let mut domain = lock(&self.arena.domain.state);
            let Some(next) = domain.parse_active.checked_add(1) else {
                domain.invariant_failed = true;
                return Err(CellControlFailure::new(
                    self.cell.key.id,
                    AccessKind::ResourceLimit,
                    CellControlTag::LoadCounterOverflow,
                ));
            };
            domain.parse_active = next;
        }
        let parse_active = ScheduledParseActiveGuard(&self.arena.domain);
        let outcome = member_loader(&permit);
        drop(parse_active);
        if let Some(panic) = self.arena.announce_dependency_event(
            &self.cell,
            token,
            edge,
            DependencyEventKind::MemberParseExit,
        )? {
            resume_unwind(panic);
        }
        let finished_permit = self
            .arena
            .take_member_permit_after_attempt(&self.cell, token)?
            .ok_or_else(|| {
                CellControlFailure::new(
                    self.cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyRejectedStale,
                )
            })?;
        drop(finished_permit);
        self.announce_scheduled_resource_shape(token, ScheduledResourceShape::Returned);
        if cancellation.load(Ordering::Acquire) || self.arena.closed.load(Ordering::Acquire) {
            reservation.cancel();
            permit.cancel();
            drop(outcome);
            drop(permit);
            drop(reservation);
            self.announce_scheduled_resource_drop(token, ScheduledResourceShape::Returned);
            return Err(CellControlFailure::new(
                self.cell.key.id,
                AccessKind::Backend,
                CellControlTag::InterestCancelled,
            ));
        }
        match outcome {
            Ok(object) if object.peak_bytes() <= estimate => {
                let retained = object.retained_bytes();
                match reservation.reconcile(retained) {
                    Ok(charge) => {
                        if let Some(panic) = self.arena.announce_dependency_event(
                            &self.cell,
                            token,
                            edge,
                            DependencyEventKind::MemberReconciledBeforePublication,
                        )? {
                            resume_unwind(panic);
                        }
                        self.announce_scheduled_resource_shape(
                            token,
                            ScheduledResourceShape::Reconciled,
                        );
                        if cancellation.load(Ordering::Acquire)
                            || self.arena.closed.load(Ordering::Acquire)
                        {
                            permit.cancel();
                            drop(charge);
                            drop(object);
                            drop(permit);
                            self.announce_scheduled_resource_drop(
                                token,
                                ScheduledResourceShape::Reconciled,
                            );
                            return Err(CellControlFailure::new(
                                self.cell.key.id,
                                AccessKind::Backend,
                                CellControlTag::InterestCancelled,
                            ));
                        }
                        self.publish_scheduled_value(
                            generation,
                            CellPayload::Object(object),
                            charge,
                            test_clone_permit(&permit),
                        );
                    }
                    Err(error) => self.publish_broker_error(generation, error),
                }
            }
            Ok(_) => {
                drop(reservation);
                self.publish_broker_error(generation, BrokerError::ReconciliationLimit);
            }
            Err(error) => {
                self.publish_load_error(generation, reservation, error);
            }
        }
        if let Some(panic) = self
            .arena
            .announce_member_published(&self.cell, token, edge)?
        {
            resume_unwind(panic);
        }
        Ok(())
    }

    pub(crate) fn resolve<F>(mut self, loader: F) -> Result<ResolvedObjectPin, AccessError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObject, CellLoadError>,
    {
        let id = self.cell.key.id;
        let pin = match self.resolve_payload(|permit| loader(permit).map(CellPayload::Object)) {
            Ok(CellResolveResult::Ready(pin)) => pin,
            Ok(CellResolveResult::SharedFailure(owner)) => match &owner.payload {
                FailurePayload::Access(error) => return Err(error.to_owned()),
                FailurePayload::ObjStm(_) => {
                    return Err(self
                        .arena
                        .invalidate_failure_payload_mismatch(&self.cell, &owner)
                        .render())
                }
            },
            Err(control) => return Err(control.render()),
        };
        if !matches!(&pin.owner.payload, CellPayload::Object(_)) {
            return Err(cell_error(
                id,
                AccessKind::Backend,
                "object cell payload representation mismatch",
            ));
        }
        Ok(ResolvedObjectPin { inner: pin })
    }

    fn resolve_object_stream<F>(
        mut self,
        loader: F,
    ) -> Result<ResolvedObjectStreamPin, ContainerCellError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObjectStream, CellLoadError>,
    {
        let pin = match self.resolve_payload(|permit| loader(permit).map(CellPayload::ObjectStream))
        {
            Ok(CellResolveResult::Ready(pin)) => pin,
            Ok(CellResolveResult::SharedFailure(owner)) => {
                return Err(ContainerCellError::Shared(owner))
            }
            Err(control) => return Err(ContainerCellError::Control(control)),
        };
        debug_assert!(matches!(&pin.owner.payload, CellPayload::ObjectStream(_)));
        Ok(ResolvedObjectStreamPin { inner: pin })
    }

    fn resolve_payload<F>(&mut self, loader: F) -> Result<CellResolveResult, CellControlFailure>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<CellPayload, CellLoadError>,
    {
        let mut loader = Some(loader);
        loop {
            let step = {
                let mut state = lock(&self.cell.state);
                if state.interests.get(self.slot).is_none_or(|interest| {
                    !interest.is_active() || interest.ordinal != self.ordinal
                }) {
                    self.completed = true;
                    ResolveStep::Control(CellControlFailure::new(
                        self.cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::InterestCancelled,
                    ))
                } else {
                    let dependency_idle = state.dependency_transition.is_idle();
                    match &mut state.phase {
                        CellPhase::Loading(loading)
                            if dependency_idle
                                && loading.leader_slot == self.slot
                                && !loading.leader_running
                                && !self.arena.closed.load(Ordering::Acquire) =>
                        {
                            loading.leader_running = true;
                            ResolveStep::Lead {
                                generation: loading.generation,
                                cancellation: Arc::clone(&loading.cancellation),
                            }
                        }
                        CellPhase::Loading(_) => ResolveStep::Wait,
                        CellPhase::Ready(_)
                        | CellPhase::Negative(_)
                        | CellPhase::FlightError(_)
                        | CellPhase::Closed(_)
                            if !dependency_idle =>
                        {
                            ResolveStep::Wait
                        }
                        CellPhase::Ready(owner) => ResolveStep::Ready(Arc::clone(owner)),
                        CellPhase::Negative(error) | CellPhase::FlightError(error) => {
                            ResolveStep::Error(Arc::clone(error))
                        }
                        CellPhase::Closed(error) => ResolveStep::Closed(error.clone()),
                    }
                }
            };
            match step {
                ResolveStep::Lead {
                    generation,
                    cancellation,
                } => {
                    let load = loader.take().expect("one loader invocation per interest");
                    self.run_leader(generation, &cancellation, load);
                }
                ResolveStep::Ready(owner) => {
                    if !owner
                        .payload
                        .matches_representation(self.cell.key.representation)
                    {
                        let failure = self.arena.invalidate_payload_mismatch(&self.cell, &owner);
                        self.finish_interest();
                        self.completed = true;
                        return Err(failure);
                    }
                    let pin = self.arena.pin(&self.cell, owner)?;
                    self.finish_interest();
                    self.completed = true;
                    return Ok(CellResolveResult::Ready(pin));
                }
                ResolveStep::Error(error) => {
                    self.finish_interest();
                    self.completed = true;
                    return Ok(CellResolveResult::SharedFailure(error));
                }
                ResolveStep::Closed(error) => {
                    self.finish_interest();
                    self.completed = true;
                    return Err(error);
                }
                ResolveStep::Control(control) => return Err(control),
                ResolveStep::Wait => {
                    let transition_state = lock(&self.cell.state);
                    if !transition_state.dependency_transition.is_idle() {
                        drop(transition_state);
                        self.arena.wait_for_dependency_transition(&self.cell);
                        continue;
                    }
                    drop(transition_state);
                    let generation = {
                        let state = lock(&self.cell.state);
                        match &state.phase {
                            CellPhase::Loading(loading)
                                if !(loading.leader_slot == self.slot
                                    && !loading.leader_running)
                                    && state.interests.get(self.slot).is_some_and(|interest| {
                                        interest.is_active() && interest.ordinal == self.ordinal
                                    }) =>
                            {
                                Some(loading.generation)
                            }
                            _ => None,
                        }
                    };
                    if let Some(generation) = generation {
                        let edge =
                            self.arena
                                .domain
                                .wait_edge(self.cell.key, generation, self.ordinal);
                        let mut state = lock(&self.cell.state);
                        if matches!(
                            &state.phase,
                            CellPhase::Loading(loading)
                                if !(loading.leader_slot == self.slot && !loading.leader_running)
                        ) && state.interests.get(self.slot).is_some_and(|interest| {
                            interest.is_active() && interest.ordinal == self.ordinal
                        }) {
                            state = wait(&self.cell.ready, state);
                        }
                        drop(state);
                        drop(edge);
                    }
                }
            }
        }
    }

    fn run_leader<F>(&self, generation: u64, load_cancel: &Arc<AtomicBool>, loader: F)
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<CellPayload, CellLoadError>,
    {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run_leader_attempt(generation, load_cancel, loader)
        }));
        if let Err(payload) = outcome {
            self.arena.release_interest(
                &self.cell,
                self.slot,
                self.ordinal,
                InterestRelease::Abandon,
            );
            self.arena
                .leader_terminal(&self.cell, self.slot, generation);
            std::panic::resume_unwind(payload);
        }
    }

    fn run_leader_attempt<F>(&self, generation: u64, load_cancel: &Arc<AtomicBool>, loader: F)
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<CellPayload, CellLoadError>,
    {
        self.arena.domain.leader_phase(LeaderPhase::BeforeRequest);
        if load_cancel.load(Ordering::Acquire) {
            self.arena
                .leader_terminal(&self.cell, self.slot, generation);
            return;
        }
        let loader_estimate = self.cell.key.representation.loader_estimate();
        let pending = {
            let headroom = lock(&self.arena.domain.headroom);
            if let Err(control) =
                self.arena
                    .domain
                    .reclaim_loader_headroom(0, loader_estimate, self.cell.key.id)
            {
                drop(headroom);
                let invariant = control.is_invariant();
                self.arena
                    .close_exact_control(&self.cell, self.slot, generation, control, invariant);
                return;
            }
            match self.arena.operation.request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                loader_estimate,
            ) {
                Ok(pending) => pending,
                Err(error) => {
                    drop(headroom);
                    self.publish_broker_error(generation, error);
                    return;
                }
            }
        };
        let broker_cancellation = pending.cancellation_handle();
        let replaced_broker_cancellation = {
            let mut state = lock(&self.cell.state);
            if let CellPhase::Loading(loading) = &mut state.phase {
                if loading.generation == generation && loading.leader_slot == self.slot {
                    loading
                        .broker_cancellation
                        .replace(broker_cancellation.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(replaced_broker_cancellation);
        self.arena
            .domain
            .leader_phase(LeaderPhase::QueuedBeforeWait);
        if load_cancel.load(Ordering::Acquire) {
            broker_cancellation.cancel();
        }
        let reservation = match pending.wait() {
            Ok(reservation) => reservation,
            Err(error) => {
                self.publish_broker_error(generation, error);
                return;
            }
        };
        self.arena.domain.leader_phase(LeaderPhase::Granted);
        if load_cancel.load(Ordering::Acquire) {
            reservation.cancel();
            drop(reservation);
            self.arena
                .leader_terminal(&self.cell, self.slot, generation);
            return;
        }
        let permit = ScalarResolutionPermit::new(loader_estimate);
        let replaced_permit = {
            let mut state = lock(&self.cell.state);
            if let CellPhase::Loading(loading) = &mut state.phase {
                if loading.generation == generation && loading.leader_slot == self.slot {
                    loading.permit.replace(permit.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(replaced_permit);
        if load_cancel.load(Ordering::Acquire) {
            permit.cancel();
        }
        {
            let mut domain = lock(&self.arena.domain.state);
            if !domain.add_loads(self.cell.key.representation, 1) {
                drop(domain);
                drop(reservation);
                self.arena.close_exact_invariant(
                    &self.cell,
                    self.slot,
                    generation,
                    CellControlTag::LoadCounterOverflow,
                );
                return;
            }
        }
        self.arena.domain.leader_phase(LeaderPhase::BeforeLoader);
        if load_cancel.load(Ordering::Acquire) {
            reservation.cancel();
            permit.cancel();
            drop(reservation);
            self.arena
                .leader_terminal(&self.cell, self.slot, generation);
            return;
        }
        let outcome = loader(&permit);
        self.arena
            .domain
            .leader_phase(LeaderPhase::AfterLoaderResult);
        let finished_permit = {
            let mut state = lock(&self.cell.state);
            if let CellPhase::Loading(loading) = &mut state.phase {
                if loading.generation == generation && loading.leader_slot == self.slot {
                    loading.permit.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(finished_permit);
        match outcome {
            Ok(payload) if payload.peak_bytes(&permit) <= loader_estimate => {
                let retained = payload.retained_bytes();
                match reservation.reconcile(retained) {
                    Ok(charge) => {
                        self.arena
                            .domain
                            .leader_phase(LeaderPhase::ReconciledBeforePublication);
                        self.publish_value(
                            generation,
                            payload,
                            charge,
                            #[cfg(test)]
                            test_clone_permit(&permit),
                        );
                    }
                    Err(error) => self.publish_broker_error(generation, error),
                }
            }
            Ok(_) => {
                drop(reservation);
                self.publish_broker_error(generation, BrokerError::ReconciliationLimit);
            }
            Err(error) => self.publish_load_error(generation, reservation, error),
        }
    }

    fn publish_value(
        &self,
        generation: u64,
        payload: CellPayload,
        charge: RetainedCharge,
        #[cfg(test)] permit: ScalarResolutionPermit,
    ) {
        let owner = Arc::new(ResolvedObjectOwner {
            payload,
            #[cfg(test)]
            permit,
            transition_gate: Mutex::new(()),
            cache_backed: AtomicBool::new(false),
            charge: Mutex::new(Some(charge)),
            self_pin: Mutex::new(None),
        });
        #[cfg(test)]
        self.arena
            .publish_ready(&self.cell, self.slot, generation, owner, false);
        #[cfg(not(test))]
        self.arena
            .publish_ready(&self.cell, self.slot, generation, owner);
    }

    #[cfg(test)]
    fn publish_scheduled_value(
        &self,
        generation: u64,
        payload: CellPayload,
        charge: RetainedCharge,
        permit: ScalarResolutionPermit,
    ) {
        let owner = Arc::new(ResolvedObjectOwner {
            payload,
            permit,
            transition_gate: Mutex::new(()),
            cache_backed: AtomicBool::new(false),
            charge: Mutex::new(Some(charge)),
            self_pin: Mutex::new(None),
        });
        self.arena
            .publish_ready(&self.cell, self.slot, generation, owner, true);
    }

    fn publish_load_error(
        &self,
        generation: u64,
        reservation: crate::broker::Reservation,
        failure: CellLoadError,
    ) {
        let CellLoadError {
            payload,
            disposition,
        } = failure;
        if !failure_payload_matches(self.cell.key.representation, &payload, false) {
            if !self.arena.close_exact_publication_invariant(
                &self.cell,
                self.slot,
                generation,
                CellControlTag::PayloadMismatch,
            ) {
                self.arena
                    .leader_terminal(&self.cell, self.slot, generation);
            }
            return;
        }
        if let FailurePayload::ObjStm(template) = &payload {
            if template.class() == ObjStmFailureClass::ExactKeyInvariant {
                let tag = template
                    .invariant_stage()
                    .map(CellControlTag::from_objstm_invariant)
                    .unwrap_or(CellControlTag::PayloadMismatch);
                if !self
                    .arena
                    .close_exact_publication_invariant(&self.cell, self.slot, generation, tag)
                {
                    self.arena
                        .leader_terminal(&self.cell, self.slot, generation);
                }
                return;
            }
        }
        let Some(disposition) = disposition else {
            if !self.arena.close_exact_publication_invariant(
                &self.cell,
                self.slot,
                generation,
                CellControlTag::PayloadMismatch,
            ) {
                self.arena
                    .leader_terminal(&self.cell, self.slot, generation);
            }
            return;
        };
        let weight = match self
            .arena
            .admit_failure_weight(&self.cell, self.slot, generation, &payload)
        {
            Ok(weight) => weight,
            Err(_) => return,
        };
        let (charge, reservation) = if weight <= self.cell.key.representation.loader_estimate()
            && !reservation.is_cancelled()
        {
            match reservation.reconcile(weight) {
                Ok(charge) => (Some(charge), None),
                Err(error) => {
                    self.publish_broker_error(generation, error);
                    return;
                }
            }
        } else {
            (None, Some(reservation))
        };
        let reconciled = charge.is_some();
        let owner = Arc::new(FailureOwner {
            payload,
            retained_weight: weight,
            charge: Mutex::new(charge),
            _reservation: Mutex::new(reservation),
            cell_envelope: false,
        });
        if reconciled {
            self.arena
                .domain
                .leader_phase(LeaderPhase::ReconciledBeforePublication);
        }
        self.arena
            .publish_error(&self.cell, self.slot, generation, owner, disposition);
    }

    fn publish_broker_error(&self, generation: u64, error: BrokerError) {
        self.arena
            .publish_broker_error(&self.cell, self.slot, generation, error);
    }

    fn finish_interest(&mut self) {
        self.arena.release_interest(
            &self.cell,
            self.slot,
            self.ordinal,
            InterestRelease::Complete,
        );
    }
}

impl Drop for ObjectCellRequest {
    fn drop(&mut self) {
        if !self.completed {
            self.arena.release_interest(
                &self.cell,
                self.slot,
                self.ordinal,
                InterestRelease::Abandon,
            );
        }
    }
}

impl CellCancellation {
    pub(crate) fn cancel(&self) {
        let (Some(arena), Some(cell)) = (self.arena.upgrade(), self.cell.upgrade()) else {
            return;
        };
        arena.release_interest(&cell, self.slot, self.ordinal, InterestRelease::Cancel);
    }
}

impl ResolvedObjectOwner {
    pub(crate) fn as_object(&self) -> &Object {
        match &self.payload {
            CellPayload::Object(object) => object.as_object(),
            CellPayload::ObjectStream(_) => {
                unreachable!("representation-checked object pin owns an object payload")
            }
        }
    }

    pub(crate) fn retained_bytes(&self) -> u64 {
        self.payload.retained_bytes()
    }

    fn transition_broker(&self, ownership: OwnershipClass) -> Result<(), BrokerError> {
        let _transition = lock(&self.transition_gate);
        self.transition_charge_broker(ownership)
    }

    fn transition_charge_broker(&self, ownership: OwnershipClass) -> Result<(), BrokerError> {
        let mut charge = lock(&self.charge);
        if let Some(charge) = charge.as_mut() {
            charge.transition(ownership)?;
            match ownership {
                OwnershipClass::Cache => self.cache_backed.store(true, Ordering::Release),
                OwnershipClass::Bypass => self.cache_backed.store(false, Ordering::Release),
                OwnershipClass::Pin => {}
            }
        }
        Ok(())
    }

    fn acquire_self_pin_broker(&self, operation: &BrokerOperation) -> Result<(), BrokerError> {
        let charge = lock(&self.charge);
        let Some(charge) = charge.as_ref() else {
            return Ok(());
        };
        let pin = charge.pin(operation, charge.bytes())?;
        *lock(&self.self_pin) = Some(pin);
        Ok(())
    }

    fn release_self_pin(&self) {
        drop(lock(&self.self_pin).take());
    }

    #[cfg(test)]
    fn charge_pointer(&self) -> Option<usize> {
        lock(&self.charge)
            .as_ref()
            .map(|charge| std::ptr::from_ref(charge) as usize)
    }
}

impl CellPayload {
    fn retained_bytes(&self) -> u64 {
        match self {
            Self::Object(object) => object.retained_bytes(),
            Self::ObjectStream(stream) => stream.retained_bytes(),
        }
    }

    fn peak_bytes(&self, permit: &ScalarResolutionPermit) -> u64 {
        match self {
            Self::Object(object) => object.peak_bytes(),
            Self::ObjectStream(_) => permit.stats().peak_bytes,
        }
    }

    fn matches_representation(&self, representation: Representation) -> bool {
        matches!(
            (self, representation),
            (
                Self::Object(_),
                Representation::RawNormalObject | Representation::DeclaredObjStmMember
            ) | (
                Self::ObjectStream(_),
                Representation::DeclaredObjStmContainer
            )
        )
    }
}

impl FailureOwner {
    pub(crate) fn payload(&self) -> &FailurePayload {
        &self.payload
    }

    pub(crate) fn retained_weight(&self) -> u64 {
        self.retained_weight
    }

    fn transition_broker(&self, ownership: OwnershipClass) -> Result<(), BrokerError> {
        let mut charge = lock(&self.charge);
        if let Some(charge) = charge.as_mut() {
            charge.transition(ownership)?;
        }
        Ok(())
    }
}

#[cfg(test)]
impl FailureOwner {
    pub(crate) fn objstm_dynamic_allocation(&self) -> Option<(usize, u64)> {
        match &self.payload {
            FailurePayload::ObjStm(template) => template.dynamic_allocation(),
            FailurePayload::Access(_) => None,
        }
    }

    fn charge_pointer(&self) -> Option<usize> {
        lock(&self.charge)
            .as_ref()
            .map(|charge| std::ptr::from_ref(charge) as usize)
    }

    fn charge_bytes(&self) -> Option<u64> {
        lock(&self.charge).as_ref().map(RetainedCharge::bytes)
    }
}

fn failure_payload_matches(
    representation: Representation,
    payload: &FailurePayload,
    cell_envelope: bool,
) -> bool {
    matches!(
        (representation, payload),
        (
            Representation::RawNormalObject | Representation::DeclaredObjStmMember,
            FailurePayload::Access(_)
        ) | (
            Representation::DeclaredObjStmContainer,
            FailurePayload::ObjStm(_)
        )
    ) || (cell_envelope
        && representation == Representation::DeclaredObjStmContainer
        && matches!(payload, FailurePayload::Access(_)))
}

impl ResolvedObjectPin {
    pub(crate) fn owner(&self) -> &Arc<ResolvedObjectOwner> {
        &self.inner.owner
    }

    #[cfg(test)]
    pub(crate) fn pointer(&self) -> usize {
        Arc::as_ptr(&self.inner.owner) as usize
    }
}

impl ResolvedObjectStreamPin {
    pub(crate) fn as_object_stream(&self) -> &BoundedObjectStream {
        match &self.inner.owner.payload {
            CellPayload::ObjectStream(stream) => stream,
            CellPayload::Object(_) => {
                unreachable!("representation-checked object-stream pin owns a container payload")
            }
        }
    }

    #[cfg(test)]
    fn pointer(&self) -> usize {
        Arc::as_ptr(&self.inner.owner) as usize
    }
}

#[cfg(test)]
impl ResolvedObjectStreamPin {
    pub(crate) fn retained_evidence(&self) -> (u64, ScalarResolutionPermit, u64) {
        let retained = self.inner.owner.retained_bytes();
        let charge = lock(&self.inner.owner.charge)
            .as_ref()
            .expect("resolved owner retains its broker charge")
            .bytes();
        (retained, self.inner.owner.permit.clone(), charge)
    }
}

impl Drop for ExternalPin {
    fn drop(&mut self) {
        let (Some(arena), Some(cell)) = (self.arena.upgrade(), self.cell.upgrade()) else {
            return;
        };
        arena.unpin(&cell, &self.owner, self.cached);
    }
}

impl ArenaInner {
    fn install_dependency_cancellation(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
        container: ObjectId,
        cancellation: CellCancellation,
    ) -> Result<Option<DependencyHookPanic>, (CellCancellation, CellControlFailure)> {
        let edge = DependencyEdge {
            from: token.key,
            to: CellKey {
                epoch: token.key.epoch,
                id: container,
                representation: Representation::DeclaredObjStmContainer,
            },
        };
        let event = {
            let mut domain = lock(&self.domain.state);
            let exact_entry = domain
                .cells
                .get(&cell.key)
                .is_some_and(|current| Arc::ptr_eq(current, cell));
            let mut state = lock(&cell.state);
            let active = state
                .interests
                .get(token.leader_slot)
                .is_some_and(|interest| interest.is_active());
            let current = exact_entry
                && token.key == cell.key
                && token.key.representation == Representation::DeclaredObjStmMember
                && state.dependency_transition.is_idle()
                && matches!(
                    &state.phase,
                    CellPhase::Loading(loading)
                        if loading.generation == token.generation
                            && loading.leader_slot == token.leader_slot
                            && loading.leader_running
                            && !loading.cancellation.load(Ordering::Acquire)
                )
                && active;
            if !current {
                return Err((
                    cancellation,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyRejectedStale,
                    ),
                ));
            }
            let duplicate = matches!(
                &state.phase,
                CellPhase::Loading(loading) if loading.dependency.is_some()
            );
            let next_cancellations = domain.dependency_cancellations.checked_add(1);
            let next_edges = domain.dependency_edges.checked_add(1);
            let next_adds = domain.dependency_edge_adds.checked_add(1);
            let next_leaders = domain.dependency_leaders.checked_add(1);
            let next_ordinal = domain.dependency_event_ordinal.checked_add(1);
            if duplicate
                || next_cancellations.is_none()
                || next_edges.is_none()
                || next_adds.is_none()
                || next_leaders.is_none()
                || next_ordinal.is_none()
            {
                domain.invariant_failed = true;
                return Err((
                    cancellation,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyStateInvariant,
                    ),
                ));
            }
            #[cfg(test)]
            if !domain.active_generations.insert(token) {
                domain.invariant_failed = true;
                return Err((
                    cancellation,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyStateInvariant,
                    ),
                ));
            }
            let CellPhase::Loading(loading) = &mut state.phase else {
                unreachable!("validated member dependency loading state")
            };
            loading.dependency = Some(MemberDependencyState {
                token,
                edge,
                cancellation: Some(cancellation),
                pin: None,
            });
            loading.dependency_container = Some(container);
            state.dependency_transition = DependencyTransition::Announcing {
                token,
                kind: DependencyEventKind::EdgeAdded,
            };
            domain.dependency_cancellations = next_cancellations.unwrap();
            domain.dependency_edges = next_edges.unwrap();
            domain.dependency_edge_adds = next_adds.unwrap();
            domain.dependency_leaders = next_leaders.unwrap();
            domain.dependency_event_ordinal = next_ordinal.unwrap();
            DependencyEvent {
                token,
                edge,
                kind: DependencyEventKind::EdgeAdded,
                ordinal: next_ordinal.unwrap(),
                successor: None,
            }
        };
        let panic = self.domain.dispatch_dependency_event(event);
        self.finish_dependency_announcement(cell, token, DependencyEventKind::EdgeAdded);
        Ok(panic)
    }

    fn take_dependency_cancellation(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
    ) -> Result<Option<CellCancellation>, CellControlFailure> {
        let mut domain = lock(&self.domain.state);
        let exact_entry = domain
            .cells
            .get(&cell.key)
            .is_some_and(|current| Arc::ptr_eq(current, cell));
        let mut state = lock(&cell.state);
        if !exact_entry {
            return Ok(None);
        }
        let dependency_idle = state.dependency_transition.is_idle();
        let CellPhase::Loading(loading) = &mut state.phase else {
            return Ok(None);
        };
        if loading.generation != token.generation
            || loading.leader_slot != token.leader_slot
            || !dependency_idle
            || loading
                .dependency
                .as_ref()
                .is_none_or(|dependency| dependency.token != token)
        {
            return Ok(None);
        }
        let Some(cancellation) = loading
            .dependency
            .as_mut()
            .and_then(|dependency| dependency.cancellation.take())
        else {
            return Ok(None);
        };
        let Some(next) = domain.dependency_cancellations.checked_sub(1) else {
            loading
                .dependency
                .as_mut()
                .expect("validated dependency")
                .cancellation = Some(cancellation);
            domain.invariant_failed = true;
            return Err(CellControlFailure::new(
                cell.key.id,
                AccessKind::Backend,
                CellControlTag::DependencyStateInvariant,
            ));
        };
        domain.dependency_cancellations = next;
        Ok(Some(cancellation))
    }

    fn install_dependency_pin(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
        pin: ResolvedObjectStreamPin,
    ) -> Result<Option<DependencyHookPanic>, (ResolvedObjectStreamPin, CellControlFailure)> {
        let event = {
            let mut domain = lock(&self.domain.state);
            let exact_entry = domain
                .cells
                .get(&cell.key)
                .is_some_and(|current| Arc::ptr_eq(current, cell));
            let mut state = lock(&cell.state);
            if !exact_entry {
                return Err((
                    pin,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyRejectedStale,
                    ),
                ));
            }
            let active = state
                .interests
                .get(token.leader_slot)
                .is_some_and(|interest| interest.is_active());
            let dependency_idle = state.dependency_transition.is_idle();
            let CellPhase::Loading(loading) = &mut state.phase else {
                return Err((
                    pin,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyRejectedStale,
                    ),
                ));
            };
            if !active
                || loading.generation != token.generation
                || loading.leader_slot != token.leader_slot
                || !dependency_idle
                || loading.cancellation.load(Ordering::Acquire)
            {
                return Err((
                    pin,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyRejectedStale,
                    ),
                ));
            }
            let Some(dependency) = loading.dependency.as_mut() else {
                return Err((
                    pin,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyRejectedStale,
                    ),
                ));
            };
            if dependency.token != token || dependency.pin.is_some() {
                domain.invariant_failed = true;
                return Err((
                    pin,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyStateInvariant,
                    ),
                ));
            }
            let Some(next_pins) = domain.dependency_pins.checked_add(1) else {
                domain.invariant_failed = true;
                return Err((
                    pin,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyStateInvariant,
                    ),
                ));
            };
            let edge = dependency.edge;
            let Some(next_ordinal) = domain.dependency_event_ordinal.checked_add(1) else {
                domain.invariant_failed = true;
                return Err((
                    pin,
                    CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyStateInvariant,
                    ),
                ));
            };
            dependency.pin = Some(pin);
            domain.dependency_pins = next_pins;
            domain.dependency_event_ordinal = next_ordinal;
            state.dependency_transition = DependencyTransition::Announcing {
                token,
                kind: DependencyEventKind::PinInstalled,
            };
            DependencyEvent {
                token,
                edge,
                kind: DependencyEventKind::PinInstalled,
                ordinal: next_ordinal,
                successor: None,
            }
        };
        let panic = self.domain.dispatch_dependency_event(event);
        self.finish_dependency_announcement(cell, token, DependencyEventKind::PinInstalled);
        Ok(panic)
    }

    #[cfg(test)]
    fn install_member_broker_cancellation(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
        cancellation: ReservationCancellation,
    ) -> Result<(), (ReservationCancellation, CellControlFailure)> {
        let mut domain = lock(&self.domain.state);
        let exact_entry = domain
            .cells
            .get(&cell.key)
            .is_some_and(|current| Arc::ptr_eq(current, cell));
        let mut state = lock(&cell.state);
        let active = state
            .interests
            .get(token.leader_slot)
            .is_some_and(|interest| interest.is_active());
        let dependency_idle = state.dependency_transition.is_idle();
        let CellPhase::Loading(loading) = &mut state.phase else {
            return Err((
                cancellation,
                CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyRejectedStale,
                ),
            ));
        };
        let exact = exact_entry
            && token.key == cell.key
            && token.key.representation == Representation::DeclaredObjStmMember
            && active
            && dependency_idle
            && loading.generation == token.generation
            && loading.leader_slot == token.leader_slot
            && loading.leader_running
            && !loading.cancellation.load(Ordering::Acquire)
            && loading
                .dependency
                .as_ref()
                .is_some_and(|dependency| dependency.token == token && dependency.pin.is_some());
        if !exact {
            return Err((
                cancellation,
                CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyRejectedStale,
                ),
            ));
        }
        if loading.broker_cancellation.is_some() || loading.permit.is_some() {
            domain.invariant_failed = true;
            return Err((
                cancellation,
                CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyStateInvariant,
                ),
            ));
        }
        loading.broker_cancellation = Some(cancellation);
        Ok(())
    }

    #[cfg(test)]
    fn install_member_permit(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
        permit: ScalarResolutionPermit,
    ) -> Result<(), (ScalarResolutionPermit, CellControlFailure)> {
        let mut domain = lock(&self.domain.state);
        let exact_entry = domain
            .cells
            .get(&cell.key)
            .is_some_and(|current| Arc::ptr_eq(current, cell));
        let mut state = lock(&cell.state);
        let active = state
            .interests
            .get(token.leader_slot)
            .is_some_and(|interest| interest.is_active());
        let dependency_idle = state.dependency_transition.is_idle();
        let CellPhase::Loading(loading) = &mut state.phase else {
            return Err((
                permit,
                CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyRejectedStale,
                ),
            ));
        };
        let exact = exact_entry
            && token.key == cell.key
            && active
            && dependency_idle
            && loading.generation == token.generation
            && loading.leader_slot == token.leader_slot
            && loading.leader_running
            && !loading.cancellation.load(Ordering::Acquire)
            && loading.broker_cancellation.is_some()
            && loading
                .dependency
                .as_ref()
                .is_some_and(|dependency| dependency.token == token && dependency.pin.is_some());
        if !exact {
            return Err((
                permit,
                CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyRejectedStale,
                ),
            ));
        }
        if loading.permit.is_some() {
            domain.invariant_failed = true;
            return Err((
                permit,
                CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyStateInvariant,
                ),
            ));
        }
        loading.permit = Some(permit);
        Ok(())
    }

    #[cfg(test)]
    fn take_member_permit_after_attempt(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
    ) -> Result<Option<ScalarResolutionPermit>, CellControlFailure> {
        let mut domain = lock(&self.domain.state);
        let exact_entry = domain
            .cells
            .get(&cell.key)
            .is_some_and(|current| Arc::ptr_eq(current, cell));
        let mut state = lock(&cell.state);
        if !exact_entry || !state.dependency_transition.is_idle() {
            return Ok(None);
        }
        let CellPhase::Loading(loading) = &mut state.phase else {
            return Ok(None);
        };
        if loading.generation != token.generation
            || loading.leader_slot != token.leader_slot
            || loading
                .dependency
                .as_ref()
                .is_none_or(|dependency| dependency.token != token || dependency.pin.is_none())
        {
            return Ok(None);
        }
        let Some(permit) = loading.permit.take() else {
            domain.invariant_failed = true;
            return Err(CellControlFailure::new(
                cell.key.id,
                AccessKind::Backend,
                CellControlTag::DependencyStateInvariant,
            ));
        };
        Ok(Some(permit))
    }

    fn take_dependency_locked(
        domain: &mut DomainState,
        state: &mut CellState,
    ) -> Option<DetachedDependency> {
        if !state.dependency_transition.is_idle() {
            return None;
        }
        let dependency = match &mut state.phase {
            CellPhase::Loading(loading) => loading.dependency.take()?,
            _ => return None,
        };
        let event = DomainInner::dependency_event_locked(
            domain,
            dependency.token,
            dependency.edge,
            DependencyEventKind::CleanupDetached,
            None,
        );
        let ordinal_invariant = event.is_none();
        state.dependency_transition = DependencyTransition::Cleaning(dependency.token);
        Some(DetachedDependency {
            dependency,
            event,
            ordinal_invariant,
        })
    }

    fn cleanup_member_dependency(
        &self,
        cell: &Arc<Cell>,
        detached: DetachedDependency,
    ) -> Result<DependencyCleanupCompletion, DependencyCleanupFailure> {
        self.domain.cleanup_dependency(cell, detached)
    }

    fn record_closed_dependency_cleanup_failure(
        &self,
        cell: &Arc<Cell>,
        failure: DependencyCleanupFailure,
    ) -> DependencyCleanupCompletion {
        let DependencyCleanupFailure {
            token,
            control,
            hook_panic,
        } = failure;
        let mut domain = lock(&self.domain.state);
        let mut state = lock(&cell.state);
        if state.dependency_transition != DependencyTransition::CleanupFailed(token) {
            domain.invariant_failed = true;
        }
        match &mut state.phase {
            CellPhase::Closed(current) => *current = control,
            _ => {
                domain.invariant_failed = true;
                state.phase = CellPhase::Closed(control);
            }
        }
        drop(state);
        drop(domain);
        DependencyCleanupCompletion { token, hook_panic }
    }

    fn finish_dependency_cleanup(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
        expected: DependencyTransition,
    ) {
        let cleared = {
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            let expected_token = match expected {
                DependencyTransition::Acknowledging(expected)
                | DependencyTransition::CleanupFailed(expected) => Some(expected),
                _ => None,
            };
            if expected_token == Some(token) && state.dependency_transition == expected {
                state.dependency_transition = DependencyTransition::Idle;
                #[cfg(test)]
                if !domain.active_generations.remove(&token) {
                    domain.invariant_failed = true;
                }
                true
            } else {
                domain.invariant_failed = true;
                false
            }
        };
        if cleared {
            cell.ready.notify_all();
        }
    }

    fn finish_successful_dependency_cleanup(&self, cell: &Arc<Cell>, token: DependencyLeaderToken) {
        self.finish_dependency_cleanup(cell, token, DependencyTransition::Acknowledging(token));
    }

    fn finish_dependency_cleanup_failure(&self, cell: &Arc<Cell>, token: DependencyLeaderToken) {
        self.finish_dependency_cleanup(cell, token, DependencyTransition::CleanupFailed(token));
    }

    fn close_loading_dependency_cleanup_failure(
        &self,
        cell: &Arc<Cell>,
        failure: DependencyCleanupFailure,
    ) -> Option<DependencyHookPanic> {
        let DependencyCleanupFailure {
            token,
            control,
            hook_panic,
        } = failure;
        let (broker_cancel, permit, old_phase, removed_cell) = {
            let mut domain = lock(&self.domain.state);
            let exact_entry = domain
                .cells
                .get(&cell.key)
                .is_some_and(|current| Arc::ptr_eq(current, cell));
            let mut state = lock(&cell.state);
            let exact_loading = matches!(
                &state.phase,
                CellPhase::Loading(loading)
                    if loading.generation == token.generation
                        && loading.leader_slot == token.leader_slot
            );
            if !exact_entry
                || !exact_loading
                || state.dependency_transition != DependencyTransition::CleanupFailed(token)
            {
                domain.invariant_failed = true;
            }
            let removed_cell = domain.cells.remove(&cell.key);
            if state.cached && !checked_sub(&mut domain.cache_bytes, state.completed_weight) {
                domain.invariant_failed = true;
            }
            if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                domain.invariant_failed = true;
            }
            state.cached = false;
            let (broker_cancel, permit) = match &mut state.phase {
                CellPhase::Loading(loading) => {
                    loading.cancellation.store(true, Ordering::Release);
                    (loading.broker_cancellation.take(), loading.permit.take())
                }
                _ => (None, None),
            };
            let old_phase = std::mem::replace(&mut state.phase, CellPhase::Closed(control));
            (broker_cancel, permit, old_phase, removed_cell)
        };
        if let Some(cancel) = broker_cancel {
            cancel.cancel();
        }
        if let Some(permit) = permit {
            permit.cancel();
        }
        drop(old_phase);
        drop(removed_cell);
        if let Some(metadata) = lock(&cell.metadata).as_mut() {
            let _ = metadata.transition(OwnershipClass::Bypass);
        }
        self.finish_dependency_cleanup_failure(cell, token);
        hook_panic
    }

    fn rollback_published_dependency_cleanup_failure(
        &self,
        cell: &Arc<Cell>,
        failure: DependencyCleanupFailure,
        expected: PublishedCleanupOwner,
    ) -> DependencyCleanupCompletion {
        let DependencyCleanupFailure {
            token,
            control,
            hook_panic,
        } = failure;
        let (old_phase, removed_cell) = {
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            let exact_owner = match (&expected, &state.phase) {
                (PublishedCleanupOwner::Ready(expected), CellPhase::Ready(current)) => {
                    Arc::ptr_eq(expected, current)
                }
                (PublishedCleanupOwner::Failure(expected), CellPhase::Negative(current))
                | (PublishedCleanupOwner::Failure(expected), CellPhase::FlightError(current)) => {
                    Arc::ptr_eq(expected, current)
                }
                _ => false,
            };
            if !exact_owner
                || state.dependency_transition != DependencyTransition::CleanupFailed(token)
            {
                domain.invariant_failed = true;
            }
            let exact_entry = domain
                .cells
                .get(&cell.key)
                .is_some_and(|current| Arc::ptr_eq(current, cell));
            let removed_cell = if exact_entry {
                domain.cells.remove(&cell.key)
            } else {
                None
            };
            if state.cached && !checked_sub(&mut domain.cache_bytes, state.completed_weight) {
                domain.invariant_failed = true;
            }
            state.cached = false;
            state.transitioning = false;
            let old_phase = std::mem::replace(&mut state.phase, CellPhase::Closed(control));
            (old_phase, removed_cell)
        };
        match &old_phase {
            CellPhase::Ready(owner) => {
                let _ = owner.transition_broker(OwnershipClass::Bypass);
            }
            CellPhase::Negative(owner) | CellPhase::FlightError(owner) => {
                let _ = owner.transition_broker(OwnershipClass::Bypass);
            }
            _ => {}
        }
        drop(old_phase);
        drop(removed_cell);
        if let Some(metadata) = lock(&cell.metadata).as_mut() {
            let _ = metadata.transition(OwnershipClass::Bypass);
        }
        DependencyCleanupCompletion { token, hook_panic }
    }

    fn finish_dependency_announcement(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
        kind: DependencyEventKind,
    ) {
        let mut domain = lock(&self.domain.state);
        let mut state = lock(&cell.state);
        if state.dependency_transition == (DependencyTransition::Announcing { token, kind }) {
            state.dependency_transition = DependencyTransition::Idle;
        } else {
            domain.invariant_failed = true;
        }
        drop(state);
        drop(domain);
        cell.ready.notify_all();
    }

    fn announce_dependency_event(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
        edge: DependencyEdge,
        kind: DependencyEventKind,
    ) -> Result<Option<DependencyHookPanic>, CellControlFailure> {
        let event = {
            let mut domain = lock(&self.domain.state);
            let exact_entry = domain
                .cells
                .get(&cell.key)
                .is_some_and(|current| Arc::ptr_eq(current, cell));
            let mut state = lock(&cell.state);
            let valid = exact_entry
                && state.dependency_transition.is_idle()
                && state
                    .interests
                    .get(token.leader_slot)
                    .is_some_and(|interest| interest.is_active())
                && matches!(
                    &state.phase,
                    CellPhase::Loading(loading)
                        if loading.generation == token.generation
                            && loading.leader_slot == token.leader_slot
                            && loading.leader_running
                            && match kind {
                                DependencyEventKind::BeforeChildInstall => {
                                    loading.dependency.is_none()
                                }
                                DependencyEventKind::InsideChildResolve => loading
                                    .dependency
                                    .as_ref()
                                    .is_some_and(|dependency| dependency.token == token),
                                DependencyEventKind::ChildResolved => loading
                                    .dependency
                                    .as_ref()
                                    .is_some_and(|dependency| dependency.token == token),
                                #[cfg(test)]
                                DependencyEventKind::ChildHandleTaken => loading
                                    .dependency
                                    .as_ref()
                                    .is_some_and(|dependency| {
                                        dependency.token == token
                                            && dependency.cancellation.is_none()
                                    }),
                                DependencyEventKind::BeforeMemberAdmission => loading
                                    .dependency
                                    .as_ref()
                                    .is_some_and(|dependency| {
                                        dependency.token == token && dependency.pin.is_some()
                                    }),
                                #[cfg(test)]
                                DependencyEventKind::MemberRequestCreated => loading
                                    .dependency
                                    .as_ref()
                                    .is_some_and(|dependency| {
                                        dependency.token == token
                                            && dependency.pin.is_some()
                                            && loading.broker_cancellation.is_none()
                                            && loading.permit.is_none()
                                    }),
                                #[cfg(test)]
                                DependencyEventKind::MemberQueued => loading
                                    .dependency
                                    .as_ref()
                                    .is_some_and(|dependency| {
                                        dependency.token == token
                                            && dependency.pin.is_some()
                                            && loading.broker_cancellation.is_some()
                                            && loading.permit.is_none()
                                    }),
                                #[cfg(test)]
                                DependencyEventKind::MemberGranted
                                | DependencyEventKind::MemberParseEnter
                                | DependencyEventKind::MemberParseExit => loading
                                    .dependency
                                    .as_ref()
                                    .is_some_and(|dependency| {
                                        dependency.token == token
                                            && dependency.pin.is_some()
                                            && loading.broker_cancellation.is_some()
                                            && loading.permit.is_some()
                                    }),
                                #[cfg(test)]
                                DependencyEventKind::MemberReconciledBeforePublication => loading
                                    .dependency
                                    .as_ref()
                                    .is_some_and(|dependency| {
                                        dependency.token == token
                                            && dependency.pin.is_some()
                                            && loading.broker_cancellation.is_some()
                                            && loading.permit.is_none()
                                    }),
                                _ => false,
                            }
                );
            if !valid {
                return Err(CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyRejectedStale,
                ));
            }
            let Some(event) =
                DomainInner::dependency_event_locked(&mut domain, token, edge, kind, None)
            else {
                return Err(CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyStateInvariant,
                ));
            };
            state.dependency_transition = DependencyTransition::Announcing { token, kind };
            event
        };
        let panic = self.domain.dispatch_dependency_event(event);
        self.finish_dependency_announcement(cell, token, kind);
        Ok(panic)
    }

    #[cfg(test)]
    fn announce_member_published(
        &self,
        cell: &Arc<Cell>,
        token: DependencyLeaderToken,
        edge: DependencyEdge,
    ) -> Result<Option<DependencyHookPanic>, CellControlFailure> {
        let event = {
            let mut domain = lock(&self.domain.state);
            let exact_entry = domain
                .cells
                .get(&cell.key)
                .is_some_and(|current| Arc::ptr_eq(current, cell));
            let state = lock(&cell.state);
            let detached_flight = !exact_entry && matches!(state.phase, CellPhase::FlightError(_));
            let valid = (exact_entry || detached_flight)
                && token.key == cell.key
                && state.dependency_transition.is_idle()
                && state
                    .interests
                    .get(token.leader_slot)
                    .is_some_and(|interest| interest.is_active())
                && matches!(
                    state.phase,
                    CellPhase::Ready(_) | CellPhase::Negative(_) | CellPhase::FlightError(_)
                );
            if !valid {
                return Err(CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyRejectedStale,
                ));
            }
            DomainInner::dependency_event_locked(
                &mut domain,
                token,
                edge,
                DependencyEventKind::MemberPublished,
                None,
            )
            .ok_or_else(|| {
                CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyStateInvariant,
                )
            })?
        };
        Ok(self.domain.dispatch_dependency_event(event))
    }

    fn wait_for_dependency_transition(&self, cell: &Arc<Cell>) {
        loop {
            let transition = lock(&cell.state).dependency_transition;
            if transition.is_idle() {
                return;
            }
            #[cfg(test)]
            let hooks = test_locked_clone(&self.domain.dependency_wait_hooks);
            #[cfg(test)]
            if let Some(hooks) = hooks {
                hooks.waiting(cell.key, transition);
            }
            let mut state = lock(&cell.state);
            while state.dependency_transition == transition {
                #[cfg(test)]
                {
                    let hooks = test_locked_clone(&self.domain.dependency_pre_wait_hooks);
                    if let Some(hooks) = hooks {
                        hooks.before_wait(cell.key, transition);
                    }
                }
                state = wait(&cell.ready, state);
                #[cfg(test)]
                if state.dependency_transition == transition {
                    drop(state);
                    let hooks = test_locked_clone(&self.domain.dependency_wait_hooks);
                    if let Some(hooks) = hooks {
                        hooks.waiting(cell.key, transition);
                    }
                    state = lock(&cell.state);
                }
            }
        }
    }

    fn admit_failure_weight(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        payload: &FailurePayload,
    ) -> Result<u64, CellControlFailure> {
        self.admit_failure_weight_result(cell, leader_slot, generation, payload.retained_weight())
    }

    fn admit_failure_weight_result(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        result: Result<u64, RetainedWeightError>,
    ) -> Result<u64, CellControlFailure> {
        match result {
            Ok(weight) => Ok(weight),
            Err(RetainedWeightError::Overflow | RetainedWeightError::OverAttempt { .. }) => {
                let control = CellControlFailure::new(
                    cell.key.id,
                    AccessKind::ResourceLimit,
                    CellControlTag::RetainedWeightOverflow,
                );
                let closed = self.teardown_exact_key(
                    cell,
                    ExactPhaseExpectation::Loading {
                        leader_slot: Some(leader_slot),
                        generation: Some(generation),
                        require_publication: true,
                    },
                    control.clone(),
                    true,
                );
                if !closed {
                    self.leader_terminal(cell, leader_slot, generation);
                }
                Err(control)
            }
        }
    }

    fn teardown_exact_key(
        &self,
        cell: &Arc<Cell>,
        expected: ExactPhaseExpectation<'_>,
        control: CellControlFailure,
        invariant: bool,
    ) -> bool {
        let (broker_cancel, permit, dependency, old_phase, removed_cell, external_pins) = loop {
            self.wait_for_dependency_transition(cell);
            let mut domain = lock(&self.domain.state);
            let exact_entry = domain
                .cells
                .get(&cell.key)
                .is_some_and(|current| Arc::ptr_eq(current, cell));
            if !exact_entry {
                return false;
            }
            let mut state = lock(&cell.state);
            if !state.dependency_transition.is_idle() {
                drop(state);
                drop(domain);
                continue;
            }
            if !expected.matches(&state) {
                return false;
            }
            if invariant {
                domain.invariant_failed = true;
            }
            let removed_cell = domain.cells.remove(&cell.key);
            if state.cached && !checked_sub(&mut domain.cache_bytes, state.completed_weight) {
                domain.invariant_failed = true;
            }
            state.cached = false;
            state.transitioning = false;
            let (broker_cancel, permit, loading) = match &mut state.phase {
                CellPhase::Loading(loading) => {
                    if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                        domain.invariant_failed = true;
                    }
                    loading.cancellation.store(true, Ordering::Release);
                    (
                        loading.broker_cancellation.take(),
                        loading.permit.take(),
                        true,
                    )
                }
                _ => (None, None, false),
            };
            let dependency = loading
                .then(|| Self::take_dependency_locked(&mut domain, &mut state))
                .flatten();
            let external_pins = state.external_pins;
            let old_phase = std::mem::replace(&mut state.phase, CellPhase::Closed(control));
            break (
                broker_cancel,
                permit,
                dependency,
                old_phase,
                removed_cell,
                external_pins,
            );
        };
        if let Some(cancel) = broker_cancel {
            cancel.cancel();
        }
        if let Some(permit) = permit {
            permit.cancel();
        }
        let (hook_panic, dependency_completion) = match dependency {
            Some(dependency) => match self.cleanup_member_dependency(cell, dependency) {
                Ok(completion) => (
                    completion.hook_panic,
                    Some((
                        completion.token,
                        DependencyTransition::Acknowledging(completion.token),
                    )),
                ),
                Err(failure) => {
                    let completion = self.record_closed_dependency_cleanup_failure(cell, failure);
                    (
                        completion.hook_panic,
                        Some((
                            completion.token,
                            DependencyTransition::CleanupFailed(completion.token),
                        )),
                    )
                }
            },
            None => (None, None),
        };
        match &old_phase {
            CellPhase::Ready(owner) if external_pins == 0 => {
                let _ = owner.transition_broker(OwnershipClass::Bypass);
            }
            CellPhase::Negative(owner) | CellPhase::FlightError(owner) => {
                let _ = owner.transition_broker(OwnershipClass::Bypass);
            }
            _ => {}
        }
        drop(old_phase);
        drop(removed_cell);
        #[cfg(test)]
        if dependency_completion.is_some() {
            self.domain.announce_dependency_terminal_cleanup(
                cell.key,
                DependencyTerminalCleanupOwner::ExactTeardown,
            );
        }
        if let Some(metadata) = lock(&cell.metadata).as_mut() {
            let _ = metadata.transition(OwnershipClass::Bypass);
        }
        if let Some((token, expected)) = dependency_completion {
            self.finish_dependency_cleanup(cell, token, expected);
        } else {
            cell.ready.notify_all();
        }
        if let Some(panic) = hook_panic {
            resume_unwind(panic);
        }
        true
    }

    fn close_exact_control(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        control: CellControlFailure,
        invariant: bool,
    ) -> bool {
        self.teardown_exact_key(
            cell,
            ExactPhaseExpectation::Loading {
                leader_slot: Some(leader_slot),
                generation: Some(generation),
                require_publication: false,
            },
            control,
            invariant,
        )
    }

    fn close_exact_invariant(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        tag: CellControlTag,
    ) -> bool {
        self.close_exact_control(
            cell,
            leader_slot,
            generation,
            CellControlFailure::new(cell.key.id, AccessKind::Backend, tag),
            true,
        )
    }

    fn close_exact_publication_invariant(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        tag: CellControlTag,
    ) -> bool {
        self.teardown_exact_key(
            cell,
            ExactPhaseExpectation::Loading {
                leader_slot: Some(leader_slot),
                generation: Some(generation),
                require_publication: true,
            },
            CellControlFailure::new(cell.key.id, AccessKind::Backend, tag),
            true,
        )
    }

    fn leader_terminal(&self, cell: &Arc<Cell>, leader_slot: usize, generation: u64) {
        self.wait_for_dependency_transition(cell);
        let dependency = {
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            if !state.dependency_transition.is_idle() {
                drop(state);
                drop(domain);
                self.wait_for_dependency_transition(cell);
                return self.leader_terminal(cell, leader_slot, generation);
            }
            let CellPhase::Loading(loading) = &state.phase else {
                return;
            };
            if loading.leader_slot != leader_slot || loading.generation != generation {
                return;
            }
            Self::take_dependency_locked(&mut domain, &mut state)
        };
        let mut dependency_edge = dependency
            .as_ref()
            .map(|dependency| (dependency.dependency.token, dependency.dependency.edge));
        let mut hook_panic = None;
        if let Some(dependency) = dependency {
            match self.cleanup_member_dependency(cell, dependency) {
                Ok(completion) => {
                    hook_panic = completion.hook_panic;
                    self.finish_successful_dependency_cleanup(cell, completion.token);
                    #[cfg(test)]
                    self.domain.announce_dependency_terminal_cleanup(
                        cell.key,
                        DependencyTerminalCleanupOwner::LeaderTerminal,
                    );
                }
                Err(failure) => {
                    let panic = self.close_loading_dependency_cleanup_failure(cell, failure);
                    if let Some(panic) = panic {
                        resume_unwind(panic);
                    }
                    return;
                }
            }
        }

        let mut remove = false;
        let mut old_phase = None;
        let mut old_cancellation = None;
        let mut old_broker_cancellation = None;
        let mut old_permit = None;
        let mut removed_cell = None;
        let mut successor_event = None;
        let mut successor_identity = None;
        {
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            let (current_generation, current_leader) = match &state.phase {
                CellPhase::Loading(loading) => (loading.generation, loading.leader_slot),
                _ => return,
            };
            if current_leader != leader_slot
                || current_generation != generation
                || !state.dependency_transition.is_idle()
            {
                return;
            }
            let next = state
                .interests
                .iter()
                .enumerate()
                .filter(|(_, interest)| interest.is_active())
                .min_by_key(|(_, interest)| interest.ordinal)
                .map(|(slot, interest)| (slot, interest.ordinal));
            if let Some((slot, interest_ordinal)) = next {
                let Some(next_generation) = current_generation.checked_add(1) else {
                    domain.invariant_failed = true;
                    if !checked_sub(&mut domain.cache_bytes, state.completed_weight) {
                        domain.invariant_failed = true;
                    }
                    removed_cell = domain.cells.remove(&cell.key);
                    if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                        domain.invariant_failed = true;
                    }
                    state.cached = false;
                    old_phase = Some(std::mem::replace(
                        &mut state.phase,
                        CellPhase::Closed(CellControlFailure::new(
                            cell.key.id,
                            AccessKind::ResourceLimit,
                            CellControlTag::LoadGenerationOverflow,
                        )),
                    ));
                    drop(state);
                    drop(domain);
                    drop(old_phase.take());
                    drop(removed_cell.take());
                    if let Some(metadata) = lock(&cell.metadata).as_mut() {
                        let _ = metadata.transition(OwnershipClass::Bypass);
                    }
                    cell.ready.notify_all();
                    return;
                };
                let successor = DependencySuccessor {
                    generation: next_generation,
                    leader_slot: slot,
                    interest_ordinal,
                };
                if dependency_edge.is_none() {
                    let container = match &mut state.phase {
                        CellPhase::Loading(loading) => loading.dependency_container.take(),
                        _ => None,
                    };
                    dependency_edge = container.map(|container| {
                        (
                            DependencyLeaderToken {
                                key: cell.key,
                                generation,
                                leader_slot,
                            },
                            DependencyEdge {
                                from: cell.key,
                                to: CellKey {
                                    epoch: cell.key.epoch,
                                    id: container,
                                    representation: Representation::DeclaredObjStmContainer,
                                },
                            },
                        )
                    });
                } else {
                    if let CellPhase::Loading(loading) = &mut state.phase {
                        loading.dependency_container = None;
                    }
                }
                if let Some((token, edge)) = dependency_edge {
                    let Some(event) = DomainInner::dependency_event_locked(
                        &mut domain,
                        token,
                        edge,
                        DependencyEventKind::SuccessorElected,
                        Some(successor),
                    ) else {
                        drop(state);
                        drop(domain);
                        self.close_exact_invariant(
                            cell,
                            leader_slot,
                            generation,
                            CellControlTag::DependencyStateInvariant,
                        );
                        return;
                    };
                    successor_event = Some(event);
                    state.dependency_transition = DependencyTransition::ReleasingSuccessor {
                        old: token,
                        successor,
                    };
                }
                for interest in state
                    .interests
                    .iter_mut()
                    .filter(|interest| interest.is_active())
                {
                    interest.generation = next_generation;
                }
                if let CellPhase::Loading(loading) = &mut state.phase {
                    loading.generation = next_generation;
                    loading.leader_slot = slot;
                    loading.leader_running = false;
                    old_cancellation = Some(std::mem::replace(
                        &mut loading.cancellation,
                        Arc::new(AtomicBool::new(false)),
                    ));
                    old_broker_cancellation = loading.broker_cancellation.take();
                    old_permit = loading.permit.take();
                    debug_assert!(loading.dependency.is_none());
                    successor_identity = Some(successor);
                }
            } else {
                removed_cell = domain.cells.remove(&cell.key);
                if !checked_sub(&mut domain.cache_bytes, state.completed_weight) {
                    domain.invariant_failed = true;
                }
                if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                    domain.invariant_failed = true;
                }
                state.cached = false;
                old_phase = Some(std::mem::replace(
                    &mut state.phase,
                    CellPhase::Closed(CellControlFailure::new(
                        cell.key.id,
                        AccessKind::Backend,
                        CellControlTag::FlightCancelled,
                    )),
                ));
                remove = true;
            }
        }
        drop(old_phase);
        drop(old_cancellation);
        if let Some(cancel) = old_broker_cancellation {
            cancel.cancel();
        }
        if let Some(permit) = old_permit {
            permit.cancel();
        }
        drop(removed_cell);
        if remove {
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
        }
        if let Some(event) = successor_event {
            if let Some(panic) = self.domain.dispatch_dependency_event(event) {
                if hook_panic.is_none() {
                    hook_panic = Some(panic);
                }
            }
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            let expected = event.successor.expect("successor event carries identity");
            if state.dependency_transition
                == (DependencyTransition::ReleasingSuccessor {
                    old: event.token,
                    successor: expected,
                })
            {
                state.dependency_transition = DependencyTransition::Idle;
            } else {
                domain.invariant_failed = true;
            }
        }
        cell.ready.notify_all();
        if let Some(successor) = successor_identity {
            let canceled = {
                let state = lock(&cell.state);
                state
                    .interests
                    .get(successor.leader_slot)
                    .is_none_or(|interest| {
                        !interest.is_active() || interest.ordinal != successor.interest_ordinal
                    })
            };
            if canceled {
                self.leader_terminal(cell, successor.leader_slot, successor.generation);
            }
        }
        if let Some(panic) = hook_panic {
            resume_unwind(panic);
        }
    }

    fn request(self: &Arc<Self>, id: ObjectId) -> Result<ObjectCellRequest, CellControlFailure> {
        self.request_representation(id, Representation::RawNormalObject)
    }

    fn request_representation(
        self: &Arc<Self>,
        id: ObjectId,
        representation: Representation,
    ) -> Result<ObjectCellRequest, CellControlFailure> {
        if self.closed.load(Ordering::Acquire) {
            return Err(CellControlFailure::new(
                id,
                AccessKind::Backend,
                CellControlTag::ArenaClosed,
            ));
        }
        let key = CellKey {
            epoch: self.epoch,
            id,
            representation,
        };
        if let Some(request) = self.join_key(key)? {
            return Ok(request);
        }
        let _admission = lock(&self.domain.admission);
        if let Some(request) = self.join_key(key)? {
            return Ok(request);
        }
        let _headroom = lock(&self.domain.headroom);
        self.domain.reclaim_loader_headroom(
            CELL_METADATA_BYTES,
            representation.loader_estimate(),
            id,
        )?;
        let victims = self.domain.make_cell_room(id, CELL_METADATA_BYTES)?;
        drop(victims);
        let reservation = self
            .operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                CELL_METADATA_BYTES,
            )
            .map_err(|error| CellControlFailure::broker(id, error))?;
        let mut metadata = reservation
            .reconcile(CELL_METADATA_BYTES)
            .map_err(|error| CellControlFailure::broker(id, error))?;
        metadata
            .transition(OwnershipClass::Cache)
            .map_err(|error| CellControlFailure::broker(id, error))?;
        let cancellation = Arc::new(AtomicBool::new(false));
        let cell = Arc::new(Cell {
            key,
            state: Mutex::new(CellState {
                phase: CellPhase::Loading(LoadingState {
                    generation: 1,
                    leader_slot: 0,
                    leader_running: false,
                    cancellation,
                    broker_cancellation: None,
                    permit: None,
                    dependency: None,
                    dependency_container: None,
                }),
                dependency_transition: DependencyTransition::Idle,
                interests: [EMPTY_INTEREST; MAX_INTERESTS_PER_CELL],
                live_interests: 0,
                next_interest_ordinal: 0,
                touch: 0,
                cached: true,
                transitioning: false,
                external_pins: 0,
                completed_weight: CELL_METADATA_BYTES,
            }),
            ready: Condvar::new(),
            metadata: Mutex::new(Some(metadata)),
        });
        let (slot, ordinal) = {
            let mut domain = lock(&self.domain.state);
            if self.closed.load(Ordering::Acquire) {
                return Err(CellControlFailure::new(
                    id,
                    AccessKind::Backend,
                    CellControlTag::ArenaClosed,
                ));
            }
            if domain.cells.len() >= self.domain.config.max_cells {
                return Err(CellControlFailure::new(
                    id,
                    AccessKind::CellFull,
                    CellControlTag::DomainFull,
                ));
            }
            if domain
                .loading_metadata_bytes
                .checked_add(CELL_METADATA_BYTES)
                .is_none_or(|bytes| bytes > MAX_LOADING_METADATA_BYTES)
            {
                return Err(CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::LoadingMetadataLimit,
                ));
            }
            let cache_bytes = domain
                .cache_bytes
                .checked_add(CELL_METADATA_BYTES)
                .ok_or_else(|| {
                    CellControlFailure::new(
                        id,
                        AccessKind::ResourceLimit,
                        CellControlTag::MetadataAccountingOverflow,
                    )
                })?;
            let loading_metadata_bytes = domain
                .loading_metadata_bytes
                .checked_add(CELL_METADATA_BYTES)
                .ok_or_else(|| {
                    CellControlFailure::new(
                        id,
                        AccessKind::ResourceLimit,
                        CellControlTag::LoadingMetadataAccountingOverflow,
                    )
                })?;
            let mut state = lock(&cell.state);
            let attached = attach_interest(
                &mut domain,
                &mut state,
                id,
                representation,
                self.domain.config.max_global_interests,
            )?;
            domain.cache_bytes = cache_bytes;
            domain.loading_metadata_bytes = loading_metadata_bytes;
            domain.cells.insert(key, Arc::clone(&cell));
            attached
        };
        Ok(ObjectCellRequest {
            arena: Arc::clone(self),
            cell,
            slot,
            ordinal,
            completed: false,
        })
    }

    fn join_key(
        self: &Arc<Self>,
        key: CellKey,
    ) -> Result<Option<ObjectCellRequest>, CellControlFailure> {
        let mut domain = lock(&self.domain.state);
        let Some(cell) = domain.cells.get(&key).cloned() else {
            return Ok(None);
        };
        let mut state = lock(&cell.state);
        let attached = attach_interest(
            &mut domain,
            &mut state,
            cell.key.id,
            cell.key.representation,
            self.domain.config.max_global_interests,
        );
        drop(state);
        drop(domain);
        let (slot, ordinal) = attached?;
        Ok(Some(ObjectCellRequest {
            arena: Arc::clone(self),
            cell,
            slot,
            ordinal,
            completed: false,
        }))
    }

    fn publish_ready(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        owner: Arc<ResolvedObjectOwner>,
        #[cfg(test)] require_cache: bool,
    ) {
        let incoming = owner.retained_bytes();
        let mut domain = lock(&self.domain.state);
        let mut state = lock(&cell.state);
        let exact_entry = domain
            .cells
            .get(&cell.key)
            .is_some_and(|current| Arc::ptr_eq(current, cell));
        if !exact_entry || !self.publication_wins(&state, leader_slot, generation) {
            drop(state);
            drop(domain);
            self.leader_terminal(cell, leader_slot, generation);
            return;
        }
        let (cache, victims) =
            self.domain
                .prepare_publication_locked(&mut domain, cell.key, incoming);
        #[cfg(test)]
        if require_cache && !cache {
            drop(state);
            drop(domain);
            drop(victims);
            drop(owner);
            self.close_exact_invariant(
                cell,
                leader_slot,
                generation,
                CellControlTag::ObjectStreamCacheBypassInvariant,
            );
            return;
        }
        if cache {
            if let Err(error) = owner.transition_broker(OwnershipClass::Cache) {
                if !checked_sub(&mut domain.cache_bytes, incoming) {
                    domain.invariant_failed = true;
                }
                drop(state);
                drop(domain);
                drop(victims);
                self.publish_broker_error(cell, leader_slot, generation, error);
                return;
            }
        }
        let Some(next_touch) = domain.touch.checked_add(1) else {
            domain.invariant_failed = true;
            let removed_cell = domain.cells.remove(&cell.key);
            let accounted = state
                .completed_weight
                .checked_add(if cache { incoming } else { 0 });
            if accounted.is_none_or(|bytes| !checked_sub(&mut domain.cache_bytes, bytes)) {
                domain.invariant_failed = true;
            }
            state.cached = false;
            state.transitioning = false;
            if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                domain.invariant_failed = true;
            }
            let dependency = Self::take_dependency_locked(&mut domain, &mut state);
            let old_phase = std::mem::replace(
                &mut state.phase,
                CellPhase::Closed(CellControlFailure::new(
                    cell.key.id,
                    AccessKind::ResourceLimit,
                    CellControlTag::TouchSequenceOverflow,
                )),
            );
            drop(state);
            drop(domain);
            drop(victims);
            let (hook_panic, dependency_completion) = match dependency {
                Some(dependency) => match self.cleanup_member_dependency(cell, dependency) {
                    Ok(completion) => (
                        completion.hook_panic,
                        Some((
                            completion.token,
                            DependencyTransition::Acknowledging(completion.token),
                        )),
                    ),
                    Err(failure) => {
                        let completion =
                            self.record_closed_dependency_cleanup_failure(cell, failure);
                        (
                            completion.hook_panic,
                            Some((
                                completion.token,
                                DependencyTransition::CleanupFailed(completion.token),
                            )),
                        )
                    }
                },
                None => (None, None),
            };
            drop(old_phase);
            drop(removed_cell);
            if cache {
                let _ = owner.transition_broker(OwnershipClass::Bypass);
            }
            #[cfg(test)]
            if dependency_completion.is_some() {
                self.domain.announce_dependency_terminal_cleanup(
                    cell.key,
                    DependencyTerminalCleanupOwner::ExactTeardown,
                );
            }
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
            if let Some((token, expected)) = dependency_completion {
                self.finish_dependency_cleanup(cell, token, expected);
            } else {
                cell.ready.notify_all();
            }
            if let Some(panic) = hook_panic {
                resume_unwind(panic);
            }
            return;
        };
        let removed_cell = if cache {
            let Some(weight) = state.completed_weight.checked_add(incoming) else {
                state.transitioning = false;
                domain.invariant_failed = true;
                drop(state);
                drop(domain);
                drop(victims);
                self.domain.release_publication(incoming);
                let _ = owner.transition_broker(OwnershipClass::Bypass);
                self.leader_terminal(cell, leader_slot, generation);
                return;
            };
            state.completed_weight = weight;
            None
        } else {
            let removed_cell = domain.cells.remove(&cell.key);
            state.cached = false;
            if !domain.add_bypasses(cell.key.representation, 1)
                || !checked_sub(&mut domain.cache_bytes, state.completed_weight)
            {
                domain.invariant_failed = true;
            }
            removed_cell
        };
        if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
            domain.invariant_failed = true;
        }
        let dependency = Self::take_dependency_locked(&mut domain, &mut state);
        let published_owner = owner.to_owned();
        let old_phase = std::mem::replace(&mut state.phase, CellPhase::Ready(owner));
        state.transitioning = false;
        domain.touch = next_touch;
        state.touch = next_touch;
        drop(state);
        drop(domain);
        drop(victims);
        let (hook_panic, dependency_completion, cleanup_failed) = match dependency {
            Some(dependency) => match self.cleanup_member_dependency(cell, dependency) {
                Ok(completion) => (
                    completion.hook_panic,
                    Some((
                        completion.token,
                        DependencyTransition::Acknowledging(completion.token),
                    )),
                    false,
                ),
                Err(failure) => {
                    let completion = self.rollback_published_dependency_cleanup_failure(
                        cell,
                        failure,
                        PublishedCleanupOwner::Ready(published_owner),
                    );
                    (
                        completion.hook_panic,
                        Some((
                            completion.token,
                            DependencyTransition::CleanupFailed(completion.token),
                        )),
                        true,
                    )
                }
            },
            None => (None, None, false),
        };
        drop(old_phase);
        drop(removed_cell);
        if !cache && !cleanup_failed {
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
        }
        if let Some((token, expected)) = dependency_completion {
            self.finish_dependency_cleanup(cell, token, expected);
        } else {
            cell.ready.notify_all();
        }
        if let Some(panic) = hook_panic {
            resume_unwind(panic);
        }
    }

    fn publish_error(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        error: Arc<FailureOwner>,
        disposition: NegativeDisposition,
    ) {
        if !failure_payload_matches(cell.key.representation, &error.payload, error.cell_envelope) {
            if !self.close_exact_publication_invariant(
                cell,
                leader_slot,
                generation,
                CellControlTag::PayloadMismatch,
            ) {
                self.leader_terminal(cell, leader_slot, generation);
            }
            return;
        }
        let incoming = error.retained_weight;
        if error.cell_envelope && incoming > CELL_ERROR_ENVELOPE_BYTES {
            self.close_exact_invariant(
                cell,
                leader_slot,
                generation,
                CellControlTag::ErrorAccountingInvariant,
            );
            return;
        }
        let persistent = disposition == NegativeDisposition::Persistent
            && !error.cell_envelope
            && lock(&error.charge).is_some();
        let mut domain = lock(&self.domain.state);
        let mut state = lock(&cell.state);
        let exact_entry = domain
            .cells
            .get(&cell.key)
            .is_some_and(|current| Arc::ptr_eq(current, cell));
        if !exact_entry || !self.publication_wins(&state, leader_slot, generation) {
            drop(state);
            drop(domain);
            self.leader_terminal(cell, leader_slot, generation);
            return;
        }
        let (cache, victims) = if persistent {
            self.domain
                .prepare_publication_locked(&mut domain, cell.key, incoming)
        } else {
            (false, Vec::new())
        };
        if cache {
            if let Err(transition_error) = error.transition_broker(OwnershipClass::Cache) {
                if !checked_sub(&mut domain.cache_bytes, incoming) {
                    domain.invariant_failed = true;
                }
                drop(state);
                drop(domain);
                drop(victims);
                self.publish_broker_error(cell, leader_slot, generation, transition_error);
                return;
            }
        }
        let Some(next_touch) = domain.touch.checked_add(1) else {
            domain.invariant_failed = true;
            let removed_cell = domain.cells.remove(&cell.key);
            let accounted = state
                .completed_weight
                .checked_add(if cache { incoming } else { 0 });
            if accounted.is_none_or(|bytes| !checked_sub(&mut domain.cache_bytes, bytes)) {
                domain.invariant_failed = true;
            }
            state.cached = false;
            state.transitioning = false;
            if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                domain.invariant_failed = true;
            }
            let dependency = Self::take_dependency_locked(&mut domain, &mut state);
            let old_phase = std::mem::replace(
                &mut state.phase,
                CellPhase::Closed(CellControlFailure::new(
                    cell.key.id,
                    AccessKind::ResourceLimit,
                    CellControlTag::TouchSequenceOverflow,
                )),
            );
            drop(state);
            drop(domain);
            drop(victims);
            let (hook_panic, dependency_completion) = match dependency {
                Some(dependency) => match self.cleanup_member_dependency(cell, dependency) {
                    Ok(completion) => (
                        completion.hook_panic,
                        Some((
                            completion.token,
                            DependencyTransition::Acknowledging(completion.token),
                        )),
                    ),
                    Err(failure) => {
                        let completion =
                            self.record_closed_dependency_cleanup_failure(cell, failure);
                        (
                            completion.hook_panic,
                            Some((
                                completion.token,
                                DependencyTransition::CleanupFailed(completion.token),
                            )),
                        )
                    }
                },
                None => (None, None),
            };
            drop(old_phase);
            drop(removed_cell);
            if cache {
                let _ = error.transition_broker(OwnershipClass::Bypass);
            }
            #[cfg(test)]
            if dependency_completion.is_some() {
                self.domain.announce_dependency_terminal_cleanup(
                    cell.key,
                    DependencyTerminalCleanupOwner::ExactTeardown,
                );
            }
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
            if let Some((token, expected)) = dependency_completion {
                self.finish_dependency_cleanup(cell, token, expected);
            } else {
                cell.ready.notify_all();
            }
            if let Some(panic) = hook_panic {
                resume_unwind(panic);
            }
            return;
        };
        if cache {
            let Some(weight) = state.completed_weight.checked_add(incoming) else {
                drop(state);
                drop(domain);
                drop(victims);
                self.domain.release_publication(incoming);
                let _ = error.transition_broker(OwnershipClass::Bypass);
                if !self.close_exact_publication_invariant(
                    cell,
                    leader_slot,
                    generation,
                    CellControlTag::ErrorAccountingInvariant,
                ) {
                    self.leader_terminal(cell, leader_slot, generation);
                }
                return;
            };
            state.completed_weight = weight;
            if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                domain.invariant_failed = true;
            }
            let dependency = Self::take_dependency_locked(&mut domain, &mut state);
            let published_owner = error.to_owned();
            let old_phase = std::mem::replace(&mut state.phase, CellPhase::Negative(error));
            state.transitioning = false;
            domain.touch = next_touch;
            state.touch = next_touch;
            drop(state);
            drop(domain);
            drop(victims);
            let (hook_panic, dependency_completion) = match dependency {
                Some(dependency) => match self.cleanup_member_dependency(cell, dependency) {
                    Ok(completion) => (
                        completion.hook_panic,
                        Some((
                            completion.token,
                            DependencyTransition::Acknowledging(completion.token),
                        )),
                    ),
                    Err(failure) => {
                        let completion = self.rollback_published_dependency_cleanup_failure(
                            cell,
                            failure,
                            PublishedCleanupOwner::Failure(published_owner),
                        );
                        (
                            completion.hook_panic,
                            Some((
                                completion.token,
                                DependencyTransition::CleanupFailed(completion.token),
                            )),
                        )
                    }
                },
                None => (None, None),
            };
            drop(old_phase);
            if let Some((token, expected)) = dependency_completion {
                self.finish_dependency_cleanup(cell, token, expected);
            } else {
                cell.ready.notify_all();
            }
            if let Some(panic) = hook_panic {
                resume_unwind(panic);
            }
            return;
        } else {
            let removed_cell = domain.cells.remove(&cell.key);
            state.cached = false;
            if !checked_sub(&mut domain.cache_bytes, state.completed_weight) {
                domain.invariant_failed = true;
            }
            let shared = (state.live_interests - 1) as u64;
            if !domain.add_transient_shares(cell.key.representation, shared) {
                domain.invariant_failed = true;
            }
            if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                domain.invariant_failed = true;
            }
            let dependency = Self::take_dependency_locked(&mut domain, &mut state);
            let published_owner = error.to_owned();
            let old_phase = std::mem::replace(&mut state.phase, CellPhase::FlightError(error));
            state.transitioning = false;
            domain.touch = next_touch;
            state.touch = next_touch;
            drop(state);
            drop(domain);
            drop(victims);
            let (hook_panic, dependency_completion, cleanup_failed) = match dependency {
                Some(dependency) => match self.cleanup_member_dependency(cell, dependency) {
                    Ok(completion) => (
                        completion.hook_panic,
                        Some((
                            completion.token,
                            DependencyTransition::Acknowledging(completion.token),
                        )),
                        false,
                    ),
                    Err(failure) => {
                        let completion = self.rollback_published_dependency_cleanup_failure(
                            cell,
                            failure,
                            PublishedCleanupOwner::Failure(published_owner),
                        );
                        (
                            completion.hook_panic,
                            Some((
                                completion.token,
                                DependencyTransition::CleanupFailed(completion.token),
                            )),
                            true,
                        )
                    }
                },
                None => (None, None, false),
            };
            drop(old_phase);
            drop(removed_cell);
            if !cleanup_failed {
                if let Some(metadata) = lock(&cell.metadata).as_mut() {
                    let _ = metadata.transition(OwnershipClass::Bypass);
                }
            }
            if let Some((token, expected)) = dependency_completion {
                self.finish_dependency_cleanup(cell, token, expected);
            } else {
                cell.ready.notify_all();
            }
            if let Some(panic) = hook_panic {
                resume_unwind(panic);
            }
            return;
        }
    }

    fn publish_broker_error(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        error: BrokerError,
    ) {
        if !self.claim_publication(cell, leader_slot, generation) {
            self.leader_terminal(cell, leader_slot, generation);
            return;
        }
        let control = CellControlFailure::broker(cell.key.id, error);
        let payload = FailurePayload::Access(control.render());
        let incoming = payload.retained_weight();
        if incoming
            .as_ref()
            .map_or(true, |bytes| *bytes > CELL_ERROR_ENVELOPE_BYTES)
        {
            self.close_exact_invariant(
                cell,
                leader_slot,
                generation,
                CellControlTag::ErrorAccountingInvariant,
            );
            return;
        }
        let owner = Arc::new(FailureOwner {
            payload,
            retained_weight: incoming.expect("checked emergency failure weight"),
            charge: Mutex::new(None),
            _reservation: Mutex::new(None),
            cell_envelope: true,
        });
        self.publish_error(
            cell,
            leader_slot,
            generation,
            owner,
            NegativeDisposition::FlightOnly,
        );
    }

    #[cfg(test)]
    fn publish_cell_envelope_control_error(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        generation: u64,
        control: CellControlFailure,
    ) {
        if !self.claim_publication(cell, leader_slot, generation) {
            self.leader_terminal(cell, leader_slot, generation);
            return;
        }
        let payload = FailurePayload::Access(control.render());
        let incoming = payload.retained_weight();
        if incoming
            .as_ref()
            .map_or(true, |bytes| *bytes > CELL_ERROR_ENVELOPE_BYTES)
        {
            self.close_exact_invariant(
                cell,
                leader_slot,
                generation,
                CellControlTag::ErrorAccountingInvariant,
            );
            return;
        }
        let owner = Arc::new(FailureOwner {
            payload,
            retained_weight: incoming.expect("checked emergency failure weight"),
            charge: Mutex::new(None),
            _reservation: Mutex::new(None),
            cell_envelope: true,
        });
        self.publish_error(
            cell,
            leader_slot,
            generation,
            owner,
            NegativeDisposition::FlightOnly,
        );
    }

    fn claim_publication(&self, cell: &Arc<Cell>, leader_slot: usize, generation: u64) -> bool {
        let domain = lock(&self.domain.state);
        if !domain
            .cells
            .get(&cell.key)
            .is_some_and(|current| Arc::ptr_eq(current, cell))
        {
            return false;
        }
        let mut state = lock(&cell.state);
        let active = state
            .interests
            .get(leader_slot)
            .is_some_and(|interest| interest.is_active());
        let dependency_idle = state.dependency_transition.is_idle();
        let claimed = match &mut state.phase {
            CellPhase::Loading(loading)
                if active
                    && dependency_idle
                    && loading.leader_slot == leader_slot
                    && loading.generation == generation
                    && !loading.cancellation.load(Ordering::Acquire) =>
            {
                loading.leader_running = true;
                true
            }
            _ => false,
        };
        drop(state);
        drop(domain);
        claimed
    }

    fn publication_wins(&self, state: &CellState, leader_slot: usize, generation: u64) -> bool {
        !self.closed.load(Ordering::Acquire)
            && state.dependency_transition.is_idle()
            && state
                .interests
                .get(leader_slot)
                .is_some_and(|slot| slot.is_active())
            && matches!(&state.phase, CellPhase::Loading(loading) if loading.leader_slot == leader_slot && loading.generation == generation && !loading.cancellation.load(Ordering::Acquire))
    }

    fn invalidate_payload_mismatch(
        &self,
        cell: &Arc<Cell>,
        owner: &Arc<ResolvedObjectOwner>,
    ) -> CellControlFailure {
        let control = CellControlFailure::new(
            cell.key.id,
            AccessKind::Backend,
            CellControlTag::PayloadMismatch,
        );
        self.teardown_exact_key(
            cell,
            ExactPhaseExpectation::Ready { owner: Some(owner) },
            control.clone(),
            true,
        );
        control
    }

    fn invalidate_failure_payload_mismatch(
        &self,
        cell: &Arc<Cell>,
        owner: &Arc<FailureOwner>,
    ) -> CellControlFailure {
        let control = || {
            CellControlFailure::new(
                cell.key.id,
                AccessKind::Backend,
                CellControlTag::PayloadMismatch,
            )
        };
        if !self.teardown_exact_key(
            cell,
            ExactPhaseExpectation::Negative { owner: Some(owner) },
            control(),
            true,
        ) {
            self.teardown_exact_key(
                cell,
                ExactPhaseExpectation::FlightError { owner: Some(owner) },
                control(),
                true,
            );
        }
        control()
    }

    fn pin(
        self: &Arc<Self>,
        cell: &Arc<Cell>,
        owner: Arc<ResolvedObjectOwner>,
    ) -> Result<ResolvedCellPin, CellControlFailure> {
        let _transition = lock(&owner.transition_gate);
        let cache_backed = owner.cache_backed.load(Ordering::Acquire);
        let admission = {
            let _domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            if self.closed.load(Ordering::Acquire)
                || !matches!(&state.phase, CellPhase::Ready(current) if Arc::ptr_eq(current, &owner))
            {
                Err(CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::PinAdmissionClosed,
                ))
            } else if let Some(external_pins) = state.external_pins.checked_add(1) {
                let cached = state.cached;
                state.external_pins = external_pins;
                let first = state.external_pins == 1;
                if first {
                    state.transitioning = true;
                }
                Ok((cached, first))
            } else {
                Err(CellControlFailure::new(
                    cell.key.id,
                    AccessKind::ResourceLimit,
                    CellControlTag::ExternalPinOverflow,
                ))
            }
        };
        let (cached, first) = admission?;
        if first {
            let transition = (if cache_backed {
                owner.transition_charge_broker(OwnershipClass::Pin)
            } else {
                Ok(())
            })
            .and_then(|()| owner.acquire_self_pin_broker(&self.operation));
            if let Err(error) = transition {
                owner.release_self_pin();
                if cache_backed {
                    let _ = owner.transition_charge_broker(OwnershipClass::Cache);
                }
                let _domain = lock(&self.domain.state);
                let mut state = lock(&cell.state);
                state.external_pins -= 1;
                state.transitioning = false;
                return Err(CellControlFailure::broker(cell.key.id, error));
            }
            let _domain = lock(&self.domain.state);
            lock(&cell.state).transitioning = false;
        }
        Ok(ResolvedCellPin {
            owner: Arc::clone(&owner),
            _pin: Arc::new(ExternalPin {
                arena: Arc::downgrade(self),
                cell: Arc::downgrade(cell),
                owner: Arc::clone(&owner),
                cached,
            }),
        })
    }

    fn unpin(&self, cell: &Arc<Cell>, owner: &Arc<ResolvedObjectOwner>, was_cached: bool) {
        let _transition = lock(&owner.transition_gate);
        let (last, final_ownership) = {
            let _domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            if state.external_pins == 0 {
                return;
            }
            state.external_pins -= 1;
            let last = state.external_pins == 0;
            let final_ownership = (last && was_cached).then_some(if state.cached {
                OwnershipClass::Cache
            } else {
                OwnershipClass::Bypass
            });
            if last {
                state.transitioning = true;
            }
            (last, final_ownership)
        };
        if last {
            owner.release_self_pin();
            if let Some(ownership) = final_ownership {
                let _ = owner.transition_charge_broker(ownership);
            }
            let _domain = lock(&self.domain.state);
            lock(&cell.state).transitioning = false;
        }
    }

    fn release_interest(
        &self,
        cell: &Arc<Cell>,
        slot: usize,
        ordinal: u64,
        release: InterestRelease,
    ) {
        let (broker_cancel, permit, leader) = {
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            if release == InterestRelease::Cancel && !matches!(state.phase, CellPhase::Loading(_)) {
                return;
            }
            let Some(interest) = state.interests.get_mut(slot) else {
                return;
            };
            if !interest.is_active() || interest.ordinal != ordinal {
                return;
            }
            interest.deactivate();
            state.live_interests -= 1;
            domain.live_interests -= 1;
            let cancel_loading = release != InterestRelease::Complete
                && matches!(state.phase, CellPhase::Loading(_));
            if cancel_loading && !domain.add_cancellations(cell.key.representation, 1) {
                domain.invariant_failed = true;
            }
            match &mut state.phase {
                CellPhase::Loading(loading) if cancel_loading && loading.leader_slot == slot => {
                    loading.cancellation.store(true, Ordering::Release);
                    (
                        loading.broker_cancellation.clone(),
                        loading.permit.clone(),
                        Some((loading.generation, loading.leader_running)),
                    )
                }
                _ => (None, None, None),
            }
        };
        if let Some(cancel) = broker_cancel {
            cancel.cancel();
        }
        if let Some(permit) = permit {
            permit.cancel();
        }
        let mut hook_panic = None;
        if let Some((generation, leader_running)) = leader {
            let mut cleanup_failed = false;
            self.wait_for_dependency_transition(cell);
            let dependency = {
                let mut domain = lock(&self.domain.state);
                let mut state = lock(&cell.state);
                let exact = matches!(
                    &state.phase,
                    CellPhase::Loading(loading)
                        if loading.generation == generation && loading.leader_slot == slot
                ) && state.dependency_transition.is_idle();
                exact
                    .then(|| Self::take_dependency_locked(&mut domain, &mut state))
                    .flatten()
            };
            if let Some(dependency) = dependency {
                match self.cleanup_member_dependency(cell, dependency) {
                    Ok(completion) => {
                        hook_panic = completion.hook_panic;
                        self.finish_successful_dependency_cleanup(cell, completion.token);
                    }
                    Err(failure) => {
                        hook_panic = self.close_loading_dependency_cleanup_failure(cell, failure);
                        cleanup_failed = true;
                    }
                }
            }
            if !leader_running && !cleanup_failed {
                self.leader_terminal(cell, slot, generation);
            }
        }
        cell.ready.notify_all();
        if let Some(panic) = hook_panic {
            resume_unwind(panic);
        }
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let (cells, removed_arena) = loop {
            let mut domain = lock(&self.domain.state);
            let pending = domain
                .cells
                .iter()
                .find(|(key, cell)| {
                    key.epoch == self.epoch && !lock(&cell.state).dependency_transition.is_idle()
                })
                .map(|(_, cell)| cell.to_owned());
            if let Some(cell) = pending {
                drop(domain);
                self.wait_for_dependency_transition(&cell);
                continue;
            }
            if !checked_add(&mut domain.closes, 1) {
                domain.invariant_failed = true;
            }
            let removed_arena = domain.arenas.remove(&self.epoch);
            let keys: Vec<_> = domain
                .cells
                .keys()
                .filter(|key| key.epoch == self.epoch)
                .copied()
                .collect();
            let mut cells = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(cell) = domain.cells.remove(&key) {
                    let mut state = lock(&cell.state);
                    if !checked_sub(&mut domain.cache_bytes, state.completed_weight) {
                        domain.invariant_failed = true;
                    }
                    state.cached = false;
                    let (broker_cancel, permit, loading) = if let CellPhase::Loading(loading) =
                        &mut state.phase
                    {
                        if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                            domain.invariant_failed = true;
                        }
                        loading.cancellation.store(true, Ordering::Release);
                        (
                            loading.broker_cancellation.take(),
                            loading.permit.take(),
                            true,
                        )
                    } else {
                        (None, None, false)
                    };
                    let dependency = loading
                        .then(|| Self::take_dependency_locked(&mut domain, &mut state))
                        .flatten();
                    let external_pins = state.external_pins;
                    let old_phase = std::mem::replace(
                        &mut state.phase,
                        CellPhase::Closed(CellControlFailure::new(
                            key.id,
                            AccessKind::Backend,
                            CellControlTag::ArenaClosed,
                        )),
                    );
                    drop(state);
                    cells.push((
                        cell,
                        broker_cancel,
                        permit,
                        dependency,
                        old_phase,
                        external_pins,
                    ));
                }
            }
            break (cells, removed_arena);
        };
        drop(removed_arena);
        #[cfg(test)]
        {
            let hooks = lock(&self.domain.close_hooks).clone();
            if let Some(hooks) = hooks {
                hooks.after_phase_replacement();
            }
        }
        let mut hook_panic = None;
        let mut dependency_completions = Vec::new();
        for (cell, broker_cancel, permit, dependency, old_phase, external_pins) in cells {
            if let Some(cancel) = broker_cancel {
                cancel.cancel();
            }
            if let Some(permit) = permit {
                permit.cancel();
            }
            let dependency_completion = if let Some(dependency) = dependency {
                let (panic, dependency_completion) =
                    match self.cleanup_member_dependency(&cell, dependency) {
                        Ok(completion) => (
                            completion.hook_panic,
                            Some((
                                completion.token,
                                DependencyTransition::Acknowledging(completion.token),
                            )),
                        ),
                        Err(failure) => {
                            let completion =
                                self.record_closed_dependency_cleanup_failure(&cell, failure);
                            (
                                completion.hook_panic,
                                Some((
                                    completion.token,
                                    DependencyTransition::CleanupFailed(completion.token),
                                )),
                            )
                        }
                    };
                if hook_panic.is_none() {
                    hook_panic = panic;
                }
                dependency_completion
            } else {
                None
            };
            #[cfg(test)]
            if dependency_completion.is_some() {
                self.domain.announce_dependency_terminal_cleanup(
                    cell.key,
                    DependencyTerminalCleanupOwner::ArenaClose,
                );
            }
            if let Some(mut metadata) = lock(&cell.metadata).take() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
            match &old_phase {
                CellPhase::Ready(owner) if external_pins == 0 => {
                    let _ = owner.transition_broker(OwnershipClass::Bypass);
                }
                CellPhase::Negative(error) => {
                    let _ = error.transition_broker(OwnershipClass::Bypass);
                }
                _ => {}
            }
            drop(old_phase);
            if let Some((token, expected)) = dependency_completion {
                dependency_completions.push((cell.to_owned(), token, expected));
            } else {
                cell.ready.notify_all();
            }
        }
        if let Some(mut metadata) = lock(&self._metadata).take() {
            let _ = metadata.transition(OwnershipClass::Bypass);
        }
        self.operation.close();
        for (cell, token, expected) in dependency_completions {
            self.finish_dependency_cleanup(&cell, token, expected);
        }
        #[cfg(test)]
        {
            let mut domain = lock(&self.domain.state);
            if let Some(next) = domain.close_acknowledgements.checked_add(1) {
                domain.close_acknowledgements = next;
            } else {
                domain.invariant_failed = true;
            }
        }
        if let Some(panic) = hook_panic {
            if !std::thread::panicking() {
                resume_unwind(panic);
            }
        }
    }
}

impl Drop for ArenaInner {
    fn drop(&mut self) {
        self.close();
    }
}

impl DomainInner {
    fn leader_phase(&self, phase: LeaderPhase) {
        #[cfg(test)]
        if let Some(hooks) = lock(&self.leader_phase_hooks).clone() {
            hooks.enter(phase);
        }
        #[cfg(not(test))]
        let _ = phase;
    }

    fn wait_edge(&self, key: CellKey, generation: u64, ordinal: u64) -> WaitEdgeGuard {
        let hooks = lock(&self.wait_hooks).clone();
        if let Some(hooks) = &hooks {
            hooks.add(key.epoch, key.id, generation, ordinal);
        }
        WaitEdgeGuard {
            hooks,
            epoch: key.epoch,
            id: key.id,
            generation,
            ordinal,
        }
    }

    fn dependency_event_locked(
        domain: &mut DomainState,
        token: DependencyLeaderToken,
        edge: DependencyEdge,
        kind: DependencyEventKind,
        successor: Option<DependencySuccessor>,
    ) -> Option<DependencyEvent> {
        let Some(ordinal) = domain.dependency_event_ordinal.checked_add(1) else {
            domain.invariant_failed = true;
            return None;
        };
        domain.dependency_event_ordinal = ordinal;
        Some(DependencyEvent {
            token,
            edge,
            kind,
            ordinal,
            successor,
        })
    }

    fn dispatch_dependency_event(&self, event: DependencyEvent) -> Option<DependencyHookPanic> {
        let hooks = lock(&self.dependency_hooks).to_owned();
        hooks.and_then(|hooks| catch_unwind(AssertUnwindSafe(|| hooks.event(event))).err())
    }

    #[cfg(test)]
    fn pause_dependency_cleanup_failure(&self, key: CellKey, token: DependencyLeaderToken) {
        let hooks = test_locked_clone(&self.dependency_cleanup_failure_hooks);
        if let Some(hooks) = hooks {
            hooks.failed(key, token);
        }
    }

    #[cfg(test)]
    fn announce_dependency_terminal_cleanup(
        &self,
        key: CellKey,
        owner: DependencyTerminalCleanupOwner,
    ) {
        let hooks = test_locked_clone(&self.dependency_terminal_cleanup_hooks);
        if let Some(hooks) = hooks {
            hooks.before_metadata(key, owner);
        }
    }

    fn cleanup_dependency(
        &self,
        cell: &Arc<Cell>,
        mut detached: DetachedDependency,
    ) -> Result<DependencyCleanupCompletion, DependencyCleanupFailure> {
        let mut hook_panic = detached
            .event
            .take()
            .and_then(|event| self.dispatch_dependency_event(event));
        let token = detached.dependency.token;
        let edge = detached.dependency.edge;
        let had_cancellation = detached.dependency.cancellation.is_some();
        let had_pin = detached.dependency.pin.is_some();
        if let Some(cancellation) = detached.dependency.cancellation.take() {
            cancellation.cancel();
        }
        drop(detached.dependency.pin.take());

        let events = {
            let mut domain = lock(&self.state);
            let mut state = lock(&cell.state);
            let exact_transition =
                state.dependency_transition == DependencyTransition::Cleaning(token);
            let next_cancellations = if had_cancellation {
                domain.dependency_cancellations.checked_sub(1)
            } else {
                Some(domain.dependency_cancellations)
            };
            let next_pins = if had_pin {
                domain.dependency_pins.checked_sub(1)
            } else {
                Some(domain.dependency_pins)
            };
            let next_edges = domain.dependency_edges.checked_sub(1);
            let next_leaders = domain.dependency_leaders.checked_sub(1);
            let next_removes = domain.dependency_edge_removes.checked_add(1);
            let next_acks = domain.dependency_cleanup_acks.checked_add(1);
            let ordinal_room = domain.dependency_event_ordinal.checked_add(2);
            if detached.ordinal_invariant
                || !exact_transition
                || next_cancellations.is_none()
                || next_pins.is_none()
                || next_edges.is_none()
                || next_leaders.is_none()
                || next_removes.is_none()
                || next_acks.is_none()
                || ordinal_room.is_none()
            {
                domain.invariant_failed = true;
                state.dependency_transition = DependencyTransition::CleanupFailed(token);
                None
            } else {
                let removed = Self::dependency_event_locked(
                    &mut domain,
                    token,
                    edge,
                    DependencyEventKind::EdgeRemoved,
                    None,
                )
                .expect("prechecked two cleanup ordinals");
                let acknowledged = Self::dependency_event_locked(
                    &mut domain,
                    token,
                    edge,
                    DependencyEventKind::CleanupAcknowledged,
                    None,
                )
                .expect("prechecked two cleanup ordinals");
                domain.dependency_cancellations = next_cancellations.unwrap();
                domain.dependency_pins = next_pins.unwrap();
                domain.dependency_edges = next_edges.unwrap();
                domain.dependency_leaders = next_leaders.unwrap();
                domain.dependency_edge_removes = next_removes.unwrap();
                domain.dependency_cleanup_acks = next_acks.unwrap();
                state.dependency_transition = DependencyTransition::Acknowledging(token);
                Some((removed, acknowledged))
            }
        };

        let Some((removed, acknowledged)) = events else {
            #[cfg(test)]
            self.pause_dependency_cleanup_failure(cell.key, token);
            return Err(DependencyCleanupFailure {
                token,
                control: CellControlFailure::new(
                    cell.key.id,
                    AccessKind::Backend,
                    CellControlTag::DependencyStateInvariant,
                ),
                hook_panic,
            });
        };
        if let Some(panic) = self.dispatch_dependency_event(removed) {
            if hook_panic.is_none() {
                hook_panic = Some(panic);
            }
        }
        if let Some(panic) = self.dispatch_dependency_event(acknowledged) {
            if hook_panic.is_none() {
                hook_panic = Some(panic);
            }
        }
        Ok(DependencyCleanupCompletion { token, hook_panic })
    }

    fn snapshot(&self) -> ObjectCellSnapshot {
        let mut domain = lock(&self.state);
        domain.arenas.retain(|_, arena| arena.strong_count() > 0);
        let mut snapshot = ObjectCellSnapshot {
            arenas: domain.arenas.len(),
            cells: domain.cells.len(),
            live_interests: domain.live_interests,
            cache_bytes: domain.cache_bytes,
            calls: domain.calls,
            loads: domain.loads,
            hits: domain.hits,
            waits: domain.waits,
            negative_hits: domain.negative_hits,
            transient_shares: domain.transient_shares,
            bypasses: domain.bypasses,
            evictions: domain.evictions,
            cancellations: domain.cancellations,
            closes: domain.closes,
            dependency_cancellations: domain.dependency_cancellations,
            dependency_pins: domain.dependency_pins,
            dependency_edges: domain.dependency_edges,
            dependency_leaders: domain.dependency_leaders,
            dependency_edge_adds: domain.dependency_edge_adds,
            dependency_edge_removes: domain.dependency_edge_removes,
            dependency_cleanup_acks: domain.dependency_cleanup_acks,
            #[cfg(test)]
            parse_active: domain.parse_active,
            #[cfg(test)]
            active_generations: domain.active_generations.len(),
            #[cfg(test)]
            close_acknowledgements: domain.close_acknowledgements,
            raw: domain.representations[Representation::RawNormalObject.index()],
            containers: domain.representations[Representation::DeclaredObjStmContainer.index()],
            members: domain.representations[Representation::DeclaredObjStmMember.index()],
            invariant_failed: domain.invariant_failed,
            ..ObjectCellSnapshot::default()
        };
        for cell in domain.cells.values() {
            let state = lock(&cell.state);
            snapshot.external_pins = snapshot.external_pins.saturating_add(state.external_pins);
            match state.phase {
                CellPhase::Loading(ref loading) => {
                    snapshot.loading += 1;
                    #[cfg(not(test))]
                    let _ = loading;
                    #[cfg(test)]
                    {
                        snapshot.permit_current += usize::from(loading.permit.clone().is_some());
                        snapshot.broker_cancellation_current +=
                            usize::from(loading.broker_cancellation.clone().is_some());
                    }
                }
                CellPhase::Ready(_) => snapshot.ready += 1,
                CellPhase::Negative(_) => snapshot.negative += 1,
                CellPhase::FlightError(_) | CellPhase::Closed(_) => {}
            }
        }
        snapshot
    }

    fn make_cell_room(
        &self,
        id: ObjectId,
        incoming: u64,
    ) -> Result<Vec<Arc<Cell>>, CellControlFailure> {
        let mut domain = lock(&self.state);
        let mut victims = Vec::new();
        while domain.cells.len() >= self.config.max_cells {
            let Some(key) = oldest_evictable(&domain, None) else {
                return Err(CellControlFailure::new(
                    id,
                    AccessKind::CellFull,
                    CellControlTag::DomainFull,
                ));
            };
            if let Some(cell) = remove_victim(&mut domain, key) {
                victims.push(cell);
            }
        }
        if domain
            .loading_metadata_bytes
            .checked_add(incoming)
            .is_none_or(|bytes| bytes > MAX_LOADING_METADATA_BYTES)
        {
            return Err(CellControlFailure::new(
                id,
                AccessKind::ResourceLimit,
                CellControlTag::LoadingMetadataLimit,
            ));
        }
        Ok(victims)
    }

    fn reclaim_loader_headroom(
        &self,
        additional: u64,
        loader_estimate: u64,
        id: ObjectId,
    ) -> Result<(), CellControlFailure> {
        loop {
            let broker = self.broker.normal_headroom();
            let drained_payload = broker
                .normal_payload_bytes
                .checked_sub(broker.normal_in_flight_estimate_bytes)
                .ok_or_else(|| {
                    CellControlFailure::new(
                        id,
                        AccessKind::ResourceLimit,
                        CellControlTag::LoaderHeadroomUnderflow,
                    )
                })?;
            let projected = drained_payload
                .checked_add(broker.metadata_bytes)
                .and_then(|bytes| bytes.checked_add(broker.completion_reserve_bytes))
                .and_then(|bytes| bytes.checked_add(additional))
                .and_then(|bytes| bytes.checked_add(loader_estimate))
                .and_then(|bytes| bytes.checked_add(broker.queue_metadata_weight));
            if projected.is_some_and(|bytes| bytes <= broker.normal_limit_bytes) {
                return Ok(());
            }

            let victim = {
                let mut domain = lock(&self.state);
                let Some(key) = oldest_evictable(&domain, None) else {
                    return Err(CellControlFailure::new(
                        id,
                        AccessKind::ResourceLimit,
                        CellControlTag::LoaderHeadroomUnavailable,
                    ));
                };
                remove_victim(&mut domain, key)
            };
            drop(victim);
        }
    }

    fn prepare_publication_locked(
        &self,
        domain: &mut DomainState,
        exclude: CellKey,
        incoming: u64,
    ) -> (bool, Vec<Arc<Cell>>) {
        let mut victims = Vec::new();
        loop {
            let Some(projected) = domain.cache_bytes.checked_add(incoming) else {
                domain.invariant_failed = true;
                return (false, victims);
            };
            if projected <= self.config.cache_target_bytes {
                domain.cache_bytes = projected;
                return (true, victims);
            }
            let Some(key) = oldest_evictable(domain, Some(exclude)) else {
                return (false, victims);
            };
            if let Some(cell) = remove_victim(domain, key) {
                victims.push(cell);
            }
        }
    }

    fn release_publication(&self, incoming: u64) {
        let mut domain = lock(&self.state);
        if let Some(updated) = domain.cache_bytes.checked_sub(incoming) {
            domain.cache_bytes = updated;
        } else {
            domain.invariant_failed = true;
        }
    }
}

fn oldest_evictable(domain: &DomainState, exclude: Option<CellKey>) -> Option<CellKey> {
    domain
        .cells
        .iter()
        .filter(|(key, _)| Some(**key) != exclude)
        .filter_map(|(key, cell)| {
            let state = lock(&cell.state);
            let complete = matches!(state.phase, CellPhase::Ready(_) | CellPhase::Negative(_));
            (complete
                && state.cached
                && state.external_pins == 0
                && state.live_interests == 0
                && !state.transitioning)
                .then_some((state.touch, *key))
        })
        .min()
        .map(|(_, key)| key)
}

fn remove_victim(domain: &mut DomainState, key: CellKey) -> Option<Arc<Cell>> {
    let cell = domain.cells.remove(&key)?;
    let weight = {
        let mut state = lock(&cell.state);
        state.cached = false;
        state.completed_weight
    };
    if !checked_sub(&mut domain.cache_bytes, weight) || !domain.add_evictions(key.representation, 1)
    {
        domain.invariant_failed = true;
    }
    Some(cell)
}

fn attach_interest(
    domain: &mut DomainState,
    state: &mut CellState,
    id: ObjectId,
    representation: Representation,
    max_global_interests: usize,
) -> Result<(usize, u64), CellControlFailure> {
    if domain.live_interests >= max_global_interests {
        return Err(CellControlFailure::new(
            id,
            AccessKind::ResourceLimit,
            CellControlTag::InterestLimit,
        ));
    }
    let slot = state
        .interests
        .iter()
        .position(|slot| !slot.is_active())
        .ok_or_else(|| {
            CellControlFailure::new(
                id,
                AccessKind::ResourceLimit,
                CellControlTag::CellInterestLimit,
            )
        })?;
    let ordinal = state.next_interest_ordinal.checked_add(1).ok_or_else(|| {
        CellControlFailure::new(
            id,
            AccessKind::ResourceLimit,
            CellControlTag::InterestOrdinalOverflow,
        )
    })?;
    let cell_interests = state.live_interests.checked_add(1).ok_or_else(|| {
        CellControlFailure::new(
            id,
            AccessKind::ResourceLimit,
            CellControlTag::CellInterestOverflow,
        )
    })?;
    let global_interests = domain.live_interests.checked_add(1).ok_or_else(|| {
        CellControlFailure::new(
            id,
            AccessKind::ResourceLimit,
            CellControlTag::GlobalInterestOverflow,
        )
    })?;
    let calls = domain.calls.checked_add(1).ok_or_else(|| {
        CellControlFailure::new(
            id,
            AccessKind::ResourceLimit,
            CellControlTag::CellCallCounterOverflow,
        )
    })?;
    let mut kind = domain.representations[representation.index()];
    kind.calls = kind.calls.checked_add(1).ok_or_else(|| {
        CellControlFailure::new(
            id,
            AccessKind::ResourceLimit,
            CellControlTag::KindCallCounterOverflow,
        )
    })?;
    let mut waits = domain.waits;
    let mut hits = domain.hits;
    let mut negative_hits = domain.negative_hits;
    let mut transient_shares = domain.transient_shares;
    let touch = match &state.phase {
        CellPhase::Loading(_) if cell_interests > 1 => {
            waits = waits.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::CellWaitCounterOverflow,
                )
            })?;
            kind.waits = kind.waits.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::KindWaitCounterOverflow,
                )
            })?;
            None
        }
        CellPhase::Loading(_) => None,
        CellPhase::Ready(_) => {
            hits = hits.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::CellHitCounterOverflow,
                )
            })?;
            kind.hits = kind.hits.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::KindHitCounterOverflow,
                )
            })?;
            Some(domain.touch.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::TouchSequenceOverflow,
                )
            })?)
        }
        CellPhase::Negative(_) => {
            negative_hits = negative_hits.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::NegativeHitCounterOverflow,
                )
            })?;
            kind.negative_hits = kind.negative_hits.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::KindNegativeHitCounterOverflow,
                )
            })?;
            Some(domain.touch.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::TouchSequenceOverflow,
                )
            })?)
        }
        CellPhase::FlightError(_) => {
            transient_shares = transient_shares.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::TransientShareCounterOverflow,
                )
            })?;
            kind.transient_shares = kind.transient_shares.checked_add(1).ok_or_else(|| {
                CellControlFailure::new(
                    id,
                    AccessKind::ResourceLimit,
                    CellControlTag::KindTransientShareCounterOverflow,
                )
            })?;
            None
        }
        CellPhase::Closed(_) => None,
    };
    let generation = match &state.phase {
        CellPhase::Loading(loading) => loading.generation,
        _ => 0,
    };

    state.next_interest_ordinal = ordinal;
    state.live_interests = cell_interests;
    state.interests[slot] = InterestSlot {
        ordinal,
        generation,
    };
    if cell_interests == 1 {
        if let CellPhase::Loading(loading) = &mut state.phase {
            loading.leader_slot = slot;
        }
    }
    domain.live_interests = global_interests;
    domain.calls = calls;
    domain.waits = waits;
    domain.hits = hits;
    domain.negative_hits = negative_hits;
    domain.transient_shares = transient_shares;
    domain.representations[representation.index()] = kind;
    if let Some(touch) = touch {
        domain.touch = touch;
        state.touch = touch;
    }
    Ok((slot, ordinal))
}

fn checked_add(value: &mut u64, amount: u64) -> bool {
    let Some(updated) = value.checked_add(amount) else {
        return false;
    };
    *value = updated;
    true
}

fn checked_sub(value: &mut u64, amount: u64) -> bool {
    let Some(updated) = value.checked_sub(amount) else {
        return false;
    };
    *value = updated;
    true
}

#[cfg(test)]
fn broker_error(id: ObjectId, error: BrokerError) -> AccessError {
    let kind = match error {
        BrokerError::Closed | BrokerError::OperationClosed | BrokerError::Cancelled => {
            AccessKind::Backend
        }
        _ => AccessKind::ResourceLimit,
    };
    cell_error(id, kind, error.to_string())
}

fn cell_error(id: ObjectId, kind: AccessKind, detail: impl Into<String>) -> AccessError {
    AccessError::typed(id, kind, detail.into())
}

#[cfg(test)]
fn test_clone_permit(permit: &ScalarResolutionPermit) -> ScalarResolutionPermit {
    permit.clone()
}

#[cfg(test)]
fn test_clone_value<T: Clone>(value: &T) -> T {
    value.clone()
}

#[cfg(test)]
fn test_arc_clone<T>(value: &Arc<T>) -> Arc<T> {
    Arc::clone(value)
}

#[cfg(test)]
fn test_locked_clone<T: Clone>(value: &Mutex<T>) -> T {
    lock(value).clone()
}

#[cfg(not(test))]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(not(test))]
fn wait<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
thread_local! {
    static OBJECT_CELL_LOCK_DEPTH: ThreadCell<usize> = const { ThreadCell::new(0) };
}

#[cfg(test)]
struct TrackedMutexGuard<'a, T> {
    guard: Option<MutexGuard<'a, T>>,
}

#[cfg(test)]
impl<T> Deref for TrackedMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard.as_deref().expect("tracked guard owns its lock")
    }
}

#[cfg(test)]
impl<T> DerefMut for TrackedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard
            .as_deref_mut()
            .expect("tracked guard owns its lock")
    }
}

#[cfg(test)]
impl<T> Drop for TrackedMutexGuard<'_, T> {
    fn drop(&mut self) {
        if self.guard.is_some() {
            OBJECT_CELL_LOCK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        }
    }
}

#[cfg(test)]
fn lock<T>(mutex: &Mutex<T>) -> TrackedMutexGuard<'_, T> {
    let guard = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    OBJECT_CELL_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
    TrackedMutexGuard { guard: Some(guard) }
}

#[cfg(test)]
fn wait<'a, T>(condvar: &Condvar, mut guard: TrackedMutexGuard<'a, T>) -> TrackedMutexGuard<'a, T> {
    let raw = guard
        .guard
        .take()
        .expect("tracked wait guard owns its lock");
    OBJECT_CELL_LOCK_DEPTH.with(|depth| depth.set(depth.get() - 1));
    let raw = condvar
        .wait(raw)
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    OBJECT_CELL_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
    TrackedMutexGuard { guard: Some(raw) }
}

#[cfg(test)]
fn caller_thread_lock_depth() -> usize {
    OBJECT_CELL_LOCK_DEPTH.with(ThreadCell::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{
        dictionary, BytesSource, Document, IndexedObjectLocation, IndexedReader,
        IndexedReaderOptions, Object, RandomAccessSource, SaveOptions, ScalarResolutionPermit,
        SourceResult,
    };
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::{mpsc, Barrier};
    use std::thread;

    fn assert_gate4_terminal(
        domain: &ObjectCellDomain,
        broker: &BudgetBroker,
        expected_closes: u64,
    ) {
        let cells = domain.snapshot();
        assert_eq!(cells.arenas, 0);
        assert_eq!(cells.cells, 0);
        assert_eq!(cells.loading, 0);
        assert_eq!(cells.ready, 0);
        assert_eq!(cells.negative, 0);
        assert_eq!(cells.live_interests, 0);
        assert_eq!(cells.external_pins, 0);
        assert_eq!(cells.dependency_cancellations, 0);
        assert_eq!(cells.dependency_pins, 0);
        assert_eq!(cells.dependency_edges, 0);
        assert_eq!(cells.dependency_leaders, 0);
        assert_eq!(cells.dependency_edge_adds, cells.dependency_edge_removes);
        assert_eq!(cells.dependency_cleanup_acks, cells.dependency_edge_removes);
        assert_eq!(cells.active_generations, 0);
        assert!(lock(&domain.inner.state).active_generations.is_empty());
        assert_eq!(cells.parse_active, 0);
        assert_eq!(cells.permit_current, 0);
        assert_eq!(cells.broker_cancellation_current, 0);
        assert_eq!(cells.closes, expected_closes);
        assert_eq!(cells.close_acknowledgements, expected_closes);
        assert_eq!(cells.cache_bytes, 0);
        assert_eq!(lock(&domain.inner.state).loading_metadata_bytes, 0);
        assert!(!cells.invariant_failed);
        assert_representation_counter_sums(&cells);

        let ledger = broker.snapshot();
        assert_eq!(ledger.normal_payload_bytes, 0);
        assert_eq!(ledger.normal_in_flight_estimate_bytes, 0);
        assert_eq!(ledger.metadata_bytes, 0);
        assert_eq!(ledger.completion_reserve_bytes, 0);
        assert_eq!(ledger.oversize_bytes, 0);
        assert_eq!(ledger.aggregate_bytes, 0);
        assert_eq!(ledger.queued, 0);
        assert_eq!(ledger.in_flight, 0);
        assert_eq!(ledger.live_request_records, 0);
        assert_eq!(ledger.error_metadata_bytes, 0);
        assert_eq!(ledger.reservation_metadata_bytes, 0);
        assert_eq!(ledger.active_operations, 0);
        assert_eq!(ledger.oversize_owners, 0);
        assert_eq!(ledger.cache_bytes, 0);
        assert_eq!(ledger.pin_bytes, 0);
        assert_eq!(ledger.bypass_bytes, 0);
        assert!(ledger.operations.is_empty());
        assert!(!ledger.invariant_failed);
        assert!(!ledger.closed);
        assert!(ledger.normal_limit_bytes > 0);
        assert!(ledger.peak_normal_bytes <= ledger.normal_limit_bytes);
        assert!(ledger.peak_completion_reserve_bytes <= ledger.normal_limit_bytes);
        assert!(ledger.peak_cache_bytes <= ledger.peak_normal_bytes);
        assert!(ledger.peak_pin_bytes <= ledger.peak_normal_bytes);
        assert!(ledger.peak_bypass_bytes <= ledger.peak_normal_bytes);
        assert!(ledger.reconciliations <= ledger.grants);
        let _historical_terminal_inventory = (
            ledger.peak_oversize_bytes,
            ledger.peak_aggregate_bytes,
            ledger.grants,
            ledger.denials,
            ledger.cancellations,
            ledger.reconciliations,
            ledger.maximum_admissible_lag,
        );
    }

    fn assert_representation_counter_sums(snapshot: &ObjectCellSnapshot) {
        let kinds = [snapshot.raw, snapshot.containers, snapshot.members];
        macro_rules! assert_sum {
            ($field:ident) => {
                assert_eq!(
                    snapshot.$field,
                    kinds.iter().map(|kind| kind.$field).sum::<u64>(),
                    "aggregate {} counter diverged from representation counters",
                    stringify!($field)
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

    fn assert_literal_counter_vectors(
        snapshot: &ObjectCellSnapshot,
        aggregate: RepresentationSnapshot,
        raw: RepresentationSnapshot,
        containers: RepresentationSnapshot,
        members: RepresentationSnapshot,
    ) {
        assert_eq!(snapshot.raw, raw);
        assert_eq!(snapshot.containers, containers);
        assert_eq!(snapshot.members, members);
        assert_eq!(
            RepresentationSnapshot {
                calls: snapshot.calls,
                loads: snapshot.loads,
                hits: snapshot.hits,
                waits: snapshot.waits,
                negative_hits: snapshot.negative_hits,
                transient_shares: snapshot.transient_shares,
                bypasses: snapshot.bypasses,
                evictions: snapshot.evictions,
                cancellations: snapshot.cancellations,
            },
            aggregate
        );
        assert_representation_counter_sums(snapshot);
    }

    fn assert_gate4_held_after_close(
        domain: &ObjectCellDomain,
        broker: &BudgetBroker,
        epoch: u64,
        retained: u64,
        pin: u64,
        bypass: u64,
        self_pin: u64,
    ) {
        let cells = domain.snapshot();
        assert_eq!(cells.arenas, 0);
        assert_eq!(cells.cells, 0);
        assert_eq!(cells.loading, 0);
        assert_eq!(cells.ready, 0);
        assert_eq!(cells.negative, 0);
        assert_eq!(cells.live_interests, 0);
        assert_eq!(cells.external_pins, 0);
        assert_eq!(cells.dependency_cancellations, 0);
        assert_eq!(cells.dependency_pins, 0);
        assert_eq!(cells.dependency_edges, 0);
        assert_eq!(cells.dependency_leaders, 0);
        assert_eq!(cells.dependency_edge_adds, cells.dependency_edge_removes);
        assert_eq!(cells.cache_bytes, 0);
        assert_eq!(lock(&domain.inner.state).loading_metadata_bytes, 0);

        let ledger = broker.snapshot();
        assert_eq!(ledger.normal_payload_bytes, retained);
        assert_eq!(ledger.normal_in_flight_estimate_bytes, 0);
        assert_eq!(
            ledger.metadata_bytes,
            crate::broker::OPERATION_METADATA_WEIGHT
        );
        assert_eq!(ledger.completion_reserve_bytes, 0);
        assert_eq!(ledger.oversize_bytes, 0);
        assert_eq!(
            ledger.aggregate_bytes,
            retained + crate::broker::OPERATION_METADATA_WEIGHT
        );
        assert_eq!(ledger.queued, 0);
        assert_eq!(ledger.in_flight, 0);
        assert_eq!(ledger.live_request_records, 0);
        assert_eq!(ledger.error_metadata_bytes, 0);
        assert_eq!(ledger.reservation_metadata_bytes, 0);
        assert_eq!(ledger.active_operations, 1);
        assert_eq!(ledger.oversize_owners, 0);
        assert_eq!(ledger.cache_bytes, 0);
        assert_eq!(ledger.pin_bytes, pin);
        assert_eq!(ledger.bypass_bytes, bypass);
        let operation = &ledger.operations[&epoch];
        assert_eq!(operation.queued, 0);
        assert_eq!(operation.in_flight, 0);
        assert_eq!(operation.error_owners, 0);
        assert_eq!(operation.cache_bytes, 0);
        assert_eq!(operation.pin_bytes, pin);
        assert_eq!(operation.bypass_bytes, bypass);
        assert_eq!(operation.self_pinned_bytes, self_pin);
        assert!(!ledger.invariant_failed);
        assert!(!ledger.closed);
    }

    #[test]
    fn production_cell_precharges_cover_typed_owner_and_error_structures() {
        assert!(CELL_FIXED_STRUCTURAL_BYTES <= CELL_BASE_METADATA_BYTES as usize);
        assert!(
            CELL_FIXED_STRUCTURAL_BYTES
                > CELL_BASE_METADATA_BYTES as usize - CELL_ERROR_ENVELOPE_BYTES as usize
        );
        assert!(std::mem::size_of::<FailureOwner>() <= ERROR_OWNER_BYTES as usize);
        assert!(std::mem::size_of::<CellControlFailure>() <= CELL_ERROR_ENVELOPE_BYTES as usize);
        assert!(!std::mem::needs_drop::<CellControlFailure>());
        assert_eq!(BTREE_NODE_ENVELOPE_BYTES, 2 * 1024);
        assert_eq!(
            PREPARED_STREAM_STRUCTURAL_BYTES as u64,
            BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES
        );

        let (reader, _, container, _) = object_stream_reader();
        let permit = ScalarResolutionPermit::new(64 * 1024 * 1024);
        let prepared = reader
            .prepare_object_stream_with_permit(container, &permit)
            .unwrap();
        assert!(
            prepared.excluded_structural_bytes() <= BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES
        );
    }

    #[test]
    fn exact_key_invariant_teardown_is_generation_scoped_and_preserves_sibling() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let hooks = Arc::new(CountingHooks {
            adds: AtomicU64::new(0),
            removes: AtomicU64::new(0),
            entered: Mutex::new(Some(entered_tx)),
            seen: Mutex::new(Vec::new()),
        });
        domain.set_wait_hooks(hooks.clone());
        let arena = domain.open_arena().unwrap();
        let id = (41, 0);
        let raw = arena.request(id).unwrap();
        let container = arena
            .inner
            .request_representation(id, Representation::DeclaredObjStmContainer)
            .unwrap();
        let waiter = arena
            .inner
            .request_representation(id, Representation::DeclaredObjStmContainer)
            .unwrap();
        let waiter_join = thread::spawn(move || {
            waiter.resolve_object_stream(|_| panic!("closed waiter must not load"))
        });
        entered_rx.recv().unwrap();
        let pending = arena
            .inner
            .operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1,
            )
            .unwrap();
        let broker_cancel = pending.cancellation_handle();
        let permit = ScalarResolutionPermit::new(1);
        {
            let mut state = lock(&container.cell.state);
            let CellPhase::Loading(loading) = &mut state.phase else {
                panic!("expected loading")
            };
            loading.broker_cancellation = Some(broker_cancel);
            loading.permit = Some(permit.clone());
        }
        assert!(!arena.inner.close_exact_invariant(
            &container.cell,
            container.slot,
            2,
            CellControlTag::LoaderHeadroomUnderflow
        ));
        assert_eq!(domain.snapshot().cells, 2);
        assert!(arena.inner.close_exact_invariant(
            &container.cell,
            container.slot,
            1,
            CellControlTag::LoaderHeadroomUnderflow
        ));
        match pending.wait() {
            Ok(reservation) => assert!(reservation.is_cancelled()),
            Err(error) => assert_eq!(error, BrokerError::Cancelled),
        }
        assert!(permit.stats().cancelled);
        let waiter_error = waiter_join
            .join()
            .unwrap()
            .unwrap_err()
            .into_access_for_test();
        assert_eq!(
            waiter_error.detail,
            "broker in-flight estimate accounting underflow"
        );
        assert_eq!(hooks.adds.load(Ordering::Relaxed), 1);
        assert_eq!(hooks.removes.load(Ordering::Relaxed), 1);
        let error = container
            .resolve_object_stream(|_| panic!("closed key must not load"))
            .unwrap_err()
            .into_access_for_test();
        assert_eq!(error.object, id);
        assert_eq!(
            error.detail,
            "broker in-flight estimate accounting underflow"
        );
        let object_reader = reader(99);
        let pin = raw
            .resolve(|permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 99);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 1);
        assert_eq!(snapshot.raw.loads, 1);
        assert_eq!(snapshot.containers.loads, 0);
        assert!(snapshot.invariant_failed);
    }

    #[test]
    fn exact_key_teardown_authority_covers_completed_and_bypass_shaped_phases() {
        let control =
            |id| CellControlFailure::new(id, AccessKind::Backend, CellControlTag::PayloadMismatch);

        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let ready_request = arena.request((43, 0)).unwrap();
        let ready_cell = Arc::clone(&ready_request.cell);
        let object_reader = reader(107);
        let ready_pin = ready_request
            .resolve(|permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        let ready_owner = Arc::clone(ready_pin.owner());
        assert!(arena.inner.teardown_exact_key(
            &ready_cell,
            ExactPhaseExpectation::Ready {
                owner: Some(&ready_owner)
            },
            control((43, 0)),
            true,
        ));
        assert_eq!(ready_pin.owner().as_object().as_i64().unwrap(), 107);

        let negative_request = arena.request((44, 0)).unwrap();
        let negative_cell = Arc::clone(&negative_request.cell);
        negative_request
            .resolve(|_| {
                Err(CellLoadError::new(
                    cell_error((44, 0), AccessKind::Backend, "persistent"),
                    NegativeDisposition::Persistent,
                ))
            })
            .unwrap_err();
        let negative_owner = {
            let state = lock(&negative_cell.state);
            let CellPhase::Negative(owner) = &state.phase else {
                panic!("expected negative")
            };
            Arc::clone(owner)
        };
        assert!(arena.inner.teardown_exact_key(
            &negative_cell,
            ExactPhaseExpectation::Negative {
                owner: Some(&negative_owner)
            },
            control((44, 0)),
            true,
        ));

        let flight_request = arena.request((45, 0)).unwrap();
        let flight_cell = Arc::clone(&flight_request.cell);
        flight_request
            .resolve(|_| Err(transient((45, 0), "flight")))
            .unwrap_err();
        let flight_owner = {
            let state = lock(&flight_cell.state);
            let CellPhase::FlightError(owner) = &state.phase else {
                panic!("expected flight")
            };
            Arc::clone(owner)
        };
        lock(&domain.inner.state)
            .cells
            .insert(flight_cell.key, Arc::clone(&flight_cell));
        assert!(arena.inner.teardown_exact_key(
            &flight_cell,
            ExactPhaseExpectation::FlightError {
                owner: Some(&flight_owner)
            },
            control((45, 0)),
            true,
        ));

        let bypass_domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(0));
        let bypass_arena = bypass_domain.open_arena().unwrap();
        let bypass_request = bypass_arena.request((46, 0)).unwrap();
        let bypass_cell = Arc::clone(&bypass_request.cell);
        let bypass_reader = reader(109);
        let bypass_pin = bypass_request
            .resolve(|permit| load(&bypass_reader, (1, 0), permit))
            .unwrap();
        let bypass_owner = Arc::clone(bypass_pin.owner());
        lock(&bypass_domain.inner.state)
            .cells
            .insert(bypass_cell.key, Arc::clone(&bypass_cell));
        assert!(!lock(&bypass_cell.state).cached);
        assert!(bypass_arena.inner.teardown_exact_key(
            &bypass_cell,
            ExactPhaseExpectation::Ready {
                owner: Some(&bypass_owner)
            },
            control((46, 0)),
            true,
        ));
        assert_eq!(bypass_pin.owner().as_object().as_i64().unwrap(), 109);
    }

    #[test]
    fn teardown_external_pin_transitions_to_bypass_and_drains_after_last_owner() {
        let lifecycle_broker = broker();
        let domain = ObjectCellDomain::new(
            lifecycle_broker.clone(),
            ObjectCellConfig::scaled(32 * 1024 * 1024),
        );
        let arena = domain.open_arena().unwrap();
        let request = arena.request((53, 0)).unwrap();
        let cell = Arc::clone(&request.cell);
        let object_reader = reader(131);
        let pin = request
            .resolve(|permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        let owner = Arc::clone(pin.owner());
        let pinned = lifecycle_broker.snapshot();
        assert!(pinned.pin_bytes > 0);
        assert!(pinned
            .operations
            .values()
            .any(|operation| operation.self_pinned_bytes > 0));

        assert!(arena.inner.teardown_exact_key(
            &cell,
            ExactPhaseExpectation::Ready {
                owner: Some(&owner)
            },
            CellControlFailure::new(
                (53, 0),
                AccessKind::Backend,
                CellControlTag::PayloadMismatch,
            ),
            true,
        ));
        assert_eq!(domain.snapshot().cells, 0);
        assert_eq!(domain.snapshot().cache_bytes, 0);
        assert!(lifecycle_broker.snapshot().pin_bytes > 0);

        drop(pin);
        let bypassed = lifecycle_broker.snapshot();
        assert_eq!(bypassed.pin_bytes, 0);
        assert!(bypassed.bypass_bytes > 0);
        assert!(bypassed
            .operations
            .values()
            .all(|operation| operation.self_pinned_bytes == 0));

        drop(owner);
        drop(cell);
        drop(arena);
        drop(domain);
        let drained = lifecycle_broker.snapshot();
        assert_eq!(drained.aggregate_bytes, 0);
        assert_eq!(drained.cache_bytes, 0);
        assert_eq!(drained.pin_bytes, 0);
        assert_eq!(drained.bypass_bytes, 0);
        assert_eq!(drained.active_operations, 0);
        assert!(drained.operations.is_empty());
        assert!(!drained.invariant_failed);
    }

    #[test]
    fn failure_weight_authority_closes_both_causes_but_is_stale_and_sibling_safe() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let sibling = arena.request((47, 0)).unwrap();
        let overflow = arena.request((48, 0)).unwrap();
        let over_attempt = arena.request((49, 0)).unwrap();
        let stale_first = arena.request((50, 0)).unwrap();
        let stale_cancel = stale_first.cancellation_handle();
        let stale_successor = arena.request((50, 0)).unwrap();

        assert_eq!(
            arena
                .inner
                .admit_failure_weight_result(
                    &overflow.cell,
                    overflow.slot,
                    1,
                    Err(RetainedWeightError::Overflow),
                )
                .unwrap_err()
                .detail,
            CellControlDetail::Static(CellControlTag::RetainedWeightOverflow)
        );
        assert_eq!(
            arena
                .inner
                .admit_failure_weight_result(
                    &over_attempt.cell,
                    over_attempt.slot,
                    1,
                    Err(RetainedWeightError::OverAttempt {
                        weight: 64 * 1024 * 1024 + 1,
                        limit: 64 * 1024 * 1024,
                    }),
                )
                .unwrap_err()
                .detail,
            CellControlDetail::Static(CellControlTag::RetainedWeightOverflow)
        );

        stale_cancel.cancel();
        assert!(arena
            .inner
            .admit_failure_weight_result(
                &stale_first.cell,
                stale_first.slot,
                1,
                Err(RetainedWeightError::Overflow),
            )
            .is_err());

        let ok_payload = FailurePayload::Access(cell_error((51, 0), AccessKind::Backend, "ok"));
        let expected_weight = ok_payload.retained_weight().unwrap();
        assert_eq!(
            arena.inner.admit_failure_weight(
                &stale_successor.cell,
                stale_successor.slot,
                2,
                &ok_payload,
            ),
            Ok(expected_weight)
        );
        let successor_reader = reader(113);
        assert_eq!(
            stale_successor
                .resolve(|permit| load(&successor_reader, (1, 0), permit))
                .unwrap()
                .owner()
                .as_object()
                .as_i64()
                .unwrap(),
            113
        );
        let sibling_reader = reader(127);
        assert_eq!(
            sibling
                .resolve(|permit| load(&sibling_reader, (1, 0), permit))
                .unwrap()
                .owner()
                .as_object()
                .as_i64()
                .unwrap(),
            127
        );
    }

    #[test]
    fn stale_leader_publication_cannot_close_or_advance_its_successor_generation() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let id = (42, 0);
        let first = arena.request(id).unwrap();
        let cancel = first.cancellation_handle();
        let second = arena.request(id).unwrap();

        cancel.cancel();
        first.publish_broker_error(1, BrokerError::Closed);

        let object_reader = reader(103);
        let pin = second
            .resolve(|permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 103);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.loads, 1);
        assert_eq!(snapshot.ready, 1);
        assert!(!snapshot.invariant_failed);
    }

    #[test]
    fn load_counter_overflow_closes_exact_loading_key_without_broker_envelope() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let id = (52, 0);
        let raw = arena.request(id).unwrap();
        let container = arena
            .inner
            .request_representation(id, Representation::DeclaredObjStmContainer)
            .unwrap();
        lock(&domain.inner.state).loads = u64::MAX;
        let error = container
            .resolve_object_stream(|_| panic!("counter overflow precedes loader"))
            .unwrap_err()
            .into_access_for_test();
        assert_eq!(error.detail, CellControlTag::LoadCounterOverflow.detail());
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 1);
        assert_eq!(snapshot.loading, 1);
        assert!(snapshot.invariant_failed);
        lock(&domain.inner.state).loads = 0;
        let object_reader = reader(101);
        let pin = raw
            .resolve(|permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 101);
    }

    #[test]
    fn every_classifier_invariant_has_a_closed_heap_free_control_tag() {
        let tags = [
            CellControlTag::PermitOrMeasurementInvariant,
            CellControlTag::ObjectLimitProvenanceInvariant,
            CellControlTag::StreamSpanInvariant,
            CellControlTag::ObjectStreamBatchSetupInvariant,
            CellControlTag::ObjectStreamCacheBypassInvariant,
            CellControlTag::RetainedWeightOverflow,
            CellControlTag::PayloadMismatch,
        ];
        for tag in tags {
            assert!(tag.is_invariant());
            assert!(!tag.detail().is_empty());
        }
        assert!(!std::mem::needs_drop::<CellControlFailure>());
    }

    #[test]
    fn representation_keys_share_one_domain_but_keep_typed_owners_and_estimates_distinct() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let (object_reader, member, container, index) = object_stream_reader();

        let first_container = arena
            .resolve_object_stream(container, |permit| {
                assert_eq!(permit.limit_bytes(), 64 * 1024 * 1024);
                object_reader
                    .prepare_object_stream_with_permit(container, permit)
                    .map_err(|error| {
                        CellLoadError::new(
                            AccessError::typed(container, AccessKind::Backend, error),
                            NegativeDisposition::Persistent,
                        )
                    })
            })
            .unwrap();
        let second_container = arena
            .resolve_object_stream(container, |_| panic!("container must hit"))
            .unwrap();
        assert_eq!(first_container.pointer(), second_container.pointer());
        assert_eq!(first_container.as_object_stream().container_id(), container);

        let first_member = arena
            .resolve_declared_member(member, |permit| {
                assert_eq!(permit.limit_bytes(), 4 * 1024 * 1024);
                first_container
                    .as_object_stream()
                    .resolve_member_with_permit(member, index, permit)
                    .map(BoundedObject::Scalar)
                    .map_err(|error| {
                        CellLoadError::new(
                            AccessError::typed(member, AccessKind::Backend, error),
                            NegativeDisposition::Persistent,
                        )
                    })
            })
            .unwrap();
        let second_member = arena
            .resolve_declared_member(member, |_| panic!("member must hit"))
            .unwrap();
        assert_eq!(first_member.pointer(), second_member.pointer());
        assert_eq!(
            first_member
                .owner()
                .as_object()
                .as_dict()
                .unwrap()
                .get(b"Answer")
                .unwrap()
                .as_i64()
                .unwrap(),
            42
        );

        let raw_reader = reader(42);
        let raw = arena
            .resolve(member, |permit| load(&raw_reader, (1, 0), permit))
            .unwrap();
        assert_ne!(raw.pointer(), first_member.pointer());

        let snapshot = domain.snapshot();
        assert_eq!(snapshot.calls, 5);
        assert_eq!(snapshot.loads, 3);
        assert_eq!(snapshot.raw.calls, 1);
        assert_eq!(snapshot.raw.loads, 1);
        assert_eq!(snapshot.containers.calls, 2);
        assert_eq!(snapshot.containers.loads, 1);
        assert_eq!(snapshot.containers.hits, 1);
        assert_eq!(snapshot.members.calls, 2);
        assert_eq!(snapshot.members.loads, 1);
        assert_eq!(snapshot.members.hits, 1);
        assert_representation_counter_sums(&snapshot);

        drop(raw);
        drop(first_member);
        drop(second_member);
        drop(first_container);
        drop(second_container);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn every_counter_is_transactionally_aggregated_across_one_mixed_domain() {
        let broker = broker();
        let domain = ObjectCellDomain::new(
            broker.clone(),
            ObjectCellConfig::scaled(CELL_METADATA_BYTES + 1024),
        );
        let arena = domain.open_arena().unwrap();
        let scalar_reader = reader(17);
        let (stream_reader, member_id, container_id, member_index) = object_stream_reader();

        for (offset, representation) in [
            Representation::RawNormalObject,
            Representation::DeclaredObjStmContainer,
            Representation::DeclaredObjStmMember,
        ]
        .into_iter()
        .enumerate()
        {
            let id = (100 + u32::try_from(offset).unwrap(), 0);
            let failure = || {
                CellLoadError::new(
                    AccessError::typed(id, AccessKind::Backend, "stable mixed negative"),
                    NegativeDisposition::Persistent,
                )
            };
            match representation {
                Representation::DeclaredObjStmContainer => {
                    arena
                        .resolve_object_stream(id, |_| Err(container_persistent(id)))
                        .unwrap_err();
                    arena
                        .resolve_object_stream(id, |_| Err(container_persistent(id)))
                        .unwrap_err();
                }
                Representation::RawNormalObject | Representation::DeclaredObjStmMember => {
                    let resolve = |arena: &ObjectCellArena| match representation {
                        Representation::RawNormalObject => arena.resolve(id, |_| Err(failure())),
                        Representation::DeclaredObjStmMember => {
                            arena.resolve_declared_member(id, |_| Err(failure()))
                        }
                        Representation::DeclaredObjStmContainer => unreachable!(),
                    };
                    resolve(&arena).unwrap_err();
                    resolve(&arena).unwrap_err();
                }
            }
        }

        let raw = arena
            .resolve((200, 0), |permit| load(&scalar_reader, (1, 0), permit))
            .unwrap();
        let raw_pointer = raw.pointer();
        drop(raw);
        let raw_hit = arena
            .resolve((200, 0), |_| panic!("raw value must hit"))
            .unwrap();
        assert_eq!(raw_hit.pointer(), raw_pointer);
        drop(raw_hit);

        let container = arena
            .resolve_object_stream(container_id, |permit| {
                stream_reader
                    .prepare_object_stream_with_permit(container_id, permit)
                    .map_err(|error| {
                        CellLoadError::new(
                            AccessError::typed(container_id, AccessKind::Backend, error),
                            NegativeDisposition::Persistent,
                        )
                    })
            })
            .unwrap();
        let container_pointer = container.pointer();
        drop(container);
        let container_hit = arena
            .resolve_object_stream(container_id, |_| panic!("container must hit"))
            .unwrap();
        assert_eq!(container_hit.pointer(), container_pointer);
        drop(container_hit);

        let member = arena
            .resolve_declared_member(member_id, |permit| {
                let container_permit = ScalarResolutionPermit::new(64 * 1024 * 1024);
                let container = stream_reader
                    .prepare_object_stream_with_permit(container_id, &container_permit)
                    .unwrap();
                container
                    .resolve_member_with_permit(member_id, member_index, permit)
                    .map(BoundedObject::Scalar)
                    .map_err(|error| {
                        CellLoadError::new(
                            AccessError::typed(member_id, AccessKind::Backend, error),
                            NegativeDisposition::Persistent,
                        )
                    })
            })
            .unwrap();
        let member_pointer = member.pointer();
        drop(member);
        let member_hit = arena
            .resolve_declared_member(member_id, |_| panic!("member must hit"))
            .unwrap();
        assert_eq!(member_hit.pointer(), member_pointer);
        drop(member_hit);

        let pinned_raw = arena
            .resolve((300, 0), |permit| load(&scalar_reader, (1, 0), permit))
            .unwrap();
        let bypass_raw = arena
            .resolve((301, 0), |permit| load(&scalar_reader, (1, 0), permit))
            .unwrap();
        let bypass_container = arena
            .resolve_object_stream(container_id, |permit| {
                stream_reader
                    .prepare_object_stream_with_permit(container_id, permit)
                    .map_err(|error| {
                        CellLoadError::new(
                            AccessError::typed(container_id, AccessKind::Backend, error),
                            NegativeDisposition::Persistent,
                        )
                    })
            })
            .unwrap();
        let bypass_member = arena
            .resolve_declared_member((302, 0), |permit| load(&scalar_reader, (1, 0), permit))
            .unwrap();

        for (offset, representation) in [
            Representation::RawNormalObject,
            Representation::DeclaredObjStmContainer,
            Representation::DeclaredObjStmMember,
        ]
        .into_iter()
        .enumerate()
        {
            let id = (400 + u32::try_from(offset).unwrap(), 0);
            let leader = arena
                .inner
                .request_representation(id, representation)
                .unwrap();
            let follower = arena
                .inner
                .request_representation(id, representation)
                .unwrap();
            match representation {
                Representation::DeclaredObjStmContainer => {
                    leader
                        .resolve_object_stream(|_| Err(container_transient(id)))
                        .unwrap_err();
                    follower
                        .resolve_object_stream(|_| panic!("follower must share"))
                        .unwrap_err();
                }
                Representation::RawNormalObject | Representation::DeclaredObjStmMember => {
                    leader
                        .resolve(|_| Err(transient(id, "mixed transient")))
                        .unwrap_err();
                    follower
                        .resolve(|_| panic!("follower must share"))
                        .unwrap_err();
                }
            }

            let cancelled = arena
                .inner
                .request_representation((500 + u32::try_from(offset).unwrap(), 0), representation)
                .unwrap();
            cancelled.cancellation_handle().cancel();
            match representation {
                Representation::DeclaredObjStmContainer => {
                    cancelled
                        .resolve_object_stream(|_| panic!("cancelled loader must not run"))
                        .unwrap_err();
                }
                Representation::RawNormalObject | Representation::DeclaredObjStmMember => {
                    cancelled
                        .resolve(|_| panic!("cancelled loader must not run"))
                        .unwrap_err();
                }
            };
        }

        let snapshot = domain.snapshot();
        assert_representation_counter_sums(&snapshot);
        for kind in [snapshot.raw, snapshot.containers, snapshot.members] {
            assert!(kind.calls > 0);
            assert!(kind.loads > 0);
            assert!(kind.hits > 0);
            assert!(kind.waits > 0);
            assert!(kind.negative_hits > 0);
            assert!(kind.transient_shares > 0);
            assert!(kind.bypasses > 0);
            assert!(kind.evictions > 0);
            assert!(kind.cancellations > 0);
        }

        drop(bypass_member);
        drop(bypass_container);
        drop(bypass_raw);
        drop(pinned_raw);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn per_kind_counter_overflow_refuses_without_partial_aggregate_mutation() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        {
            let mut state = lock(&domain.inner.state);
            state.calls = 7;
            state.representations[Representation::DeclaredObjStmContainer.index()].calls = u64::MAX;
        }
        let error = arena
            .inner
            .request_representation((77, 0), Representation::DeclaredObjStmContainer)
            .err()
            .expect("kind counter overflow must refuse the interest");
        assert_eq!(error.kind, AccessKind::ResourceLimit);
        let state = lock(&domain.inner.state);
        assert_eq!(state.calls, 7);
        assert_eq!(
            state.representations[Representation::DeclaredObjStmContainer.index()].calls,
            u64::MAX
        );
        assert_eq!(state.live_interests, 0);
        assert!(state.cells.is_empty());
    }

    #[test]
    fn typed_payload_mismatch_closes_only_its_exact_key() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let object_reader = reader(7);
        let raw = arena
            .resolve((1, 0), |permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        let member = arena
            .resolve_declared_member((1, 0), |permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        let raw_pointer = raw.pointer();
        let member_pointer = member.pointer();
        drop(raw);
        drop(member);

        let request = arena
            .inner
            .request_representation((1, 0), Representation::DeclaredObjStmContainer)
            .unwrap();
        let error = request
            .resolve(|permit| load(&object_reader, (1, 0), permit))
            .expect_err("an object payload cannot inhabit a container key");
        assert_eq!(error.kind, AccessKind::Backend);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 2);
        assert!(snapshot.invariant_failed);

        let surviving_raw = arena
            .resolve((1, 0), |_| panic!("raw sibling must hit"))
            .unwrap();
        let surviving_member = arena
            .resolve_declared_member((1, 0), |_| panic!("member sibling must hit"))
            .unwrap();
        assert_eq!(surviving_raw.pointer(), raw_pointer);
        assert_eq!(surviving_member.pointer(), member_pointer);
        assert_ne!(surviving_raw.pointer(), surviving_member.pointer());
        drop(surviving_raw);
        drop(surviving_member);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    fn broker() -> BudgetBroker {
        BudgetBroker::new(crate::broker::BrokerConfig {
            normal_limit: 128 * 1024 * 1024,
            oversize_limit: 64 * 1024 * 1024,
            completion_reserve_limit: 32 * 1024 * 1024,
            queue_metadata_weight: crate::broker::QUEUE_METADATA_WEIGHT,
            operation_metadata_weight: crate::broker::OPERATION_METADATA_WEIGHT,
            max_active_operations: 128,
            max_queued_requests: 128,
        })
        .unwrap()
    }

    fn wide_broker() -> BudgetBroker {
        BudgetBroker::new(crate::broker::BrokerConfig {
            normal_limit: 256 * 1024 * 1024,
            oversize_limit: 64 * 1024 * 1024,
            completion_reserve_limit: 32 * 1024 * 1024,
            queue_metadata_weight: crate::broker::QUEUE_METADATA_WEIGHT,
            operation_metadata_weight: crate::broker::OPERATION_METADATA_WEIGHT,
            max_active_operations: 128,
            max_queued_requests: 128,
        })
        .unwrap()
    }

    fn headroom_broker() -> BudgetBroker {
        BudgetBroker::new(crate::broker::BrokerConfig {
            normal_limit: 80 * 1024 * 1024,
            oversize_limit: 64 * 1024 * 1024,
            completion_reserve_limit: 16 * 1024 * 1024,
            queue_metadata_weight: crate::broker::QUEUE_METADATA_WEIGHT,
            operation_metadata_weight: crate::broker::OPERATION_METADATA_WEIGHT,
            max_active_operations: 4_096,
            max_queued_requests: 4_096,
        })
        .unwrap()
    }

    fn one_object_pdf(value: i64) -> Vec<u8> {
        let body = format!("1 0 obj\n{value}\nendobj\n");
        let xref = 9 + body.len();
        format!(
            "%PDF-1.4\n{body}xref\n0 2\n0000000000 65535 f \n0000000009 00000 n \ntrailer\n<< /Size 2 >>\nstartxref\n{xref}\n%%EOF\n"
        )
        .into_bytes()
    }

    fn reader(value: i64) -> Arc<IndexedReader> {
        Arc::new(
            IndexedReader::open_with_options(
                BytesSource::from(one_object_pdf(value)),
                IndexedReaderOptions::default(),
            )
            .unwrap(),
        )
    }

    fn object_stream_fixture() -> (Arc<[u8]>, ObjectId, ObjectId, u32) {
        let mut document = Document::with_version("1.7");
        let pages = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0,
        }));
        let catalog = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages),
        }));
        let member = document.add_object(Object::Dictionary(dictionary! {
            "Answer" => 42,
        }));
        document.trailer.set("Root", Object::Reference(catalog));
        document.trailer.set("Info", Object::Reference(member));
        let options = SaveOptions::builder()
            .use_object_streams(true)
            .use_xref_streams(true)
            .build();
        let mut raw = Vec::new();
        document.save_with_options(&mut raw, options).unwrap();
        let raw: Arc<[u8]> = Arc::from(raw);
        let reader = IndexedReader::open(BytesSource::from(Arc::clone(&raw))).unwrap();
        let IndexedObjectLocation::Compressed { container, index } =
            reader.object_location(member).unwrap()
        else {
            panic!("fixture member must be declared compressed")
        };
        (raw, member, container, index)
    }

    fn object_stream_reader() -> (Arc<IndexedReader>, ObjectId, ObjectId, u32) {
        let (raw, member, container, index) = object_stream_fixture();
        let reader = Arc::new(IndexedReader::open(BytesSource::from(raw)).unwrap());
        (reader, member, container, index)
    }

    fn load(
        reader: &IndexedReader,
        id: ObjectId,
        permit: &ScalarResolutionPermit,
    ) -> Result<BoundedObject, CellLoadError> {
        reader
            .resolve_object_with_permit(id, permit)
            .map_err(|error| {
                CellLoadError::new(
                    AccessError::typed(id, AccessKind::Backend, error.to_string()),
                    NegativeDisposition::Persistent,
                )
            })
    }

    fn transient(id: ObjectId, detail: &'static str) -> CellLoadError {
        CellLoadError::new(
            AccessError::typed(id, AccessKind::SourceIo, detail),
            NegativeDisposition::FlightOnly,
        )
    }

    fn container_persistent(id: ObjectId) -> CellLoadError {
        CellLoadError::objstm(crate::objstm_failures::classify(
            id,
            lopdf::IndexedReaderError::ObjectStreamContainerNotStream { id, container: id },
        ))
    }

    fn container_transient(id: ObjectId) -> CellLoadError {
        CellLoadError::objstm(crate::objstm_failures::classify(
            id,
            lopdf::IndexedReaderError::Source(lopdf::SourceError::SourceChanged),
        ))
    }

    #[test]
    fn same_key_has_one_load_and_pointer_identical_owners() {
        for callers in [2, 4, 16] {
            let domain =
                ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = Arc::new(domain.open_arena().unwrap());
            let reader = reader(42);
            let barrier = Arc::new(Barrier::new(callers + 1));
            let loads = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let mut joins = Vec::new();
            for _ in 0..callers {
                let arena = Arc::clone(&arena);
                let reader = Arc::clone(&reader);
                let barrier = Arc::clone(&barrier);
                let loads = Arc::clone(&loads);
                joins.push(thread::spawn(move || {
                    barrier.wait();
                    arena
                        .resolve((1, 0), |permit| {
                            loads.fetch_add(1, Ordering::Relaxed);
                            load(&reader, (1, 0), permit)
                        })
                        .unwrap()
                }));
            }
            barrier.wait();
            let pins: Vec<_> = joins.into_iter().map(|join| join.join().unwrap()).collect();
            assert_eq!(loads.load(Ordering::Relaxed), 1, "{callers} callers");
            assert!(pins.iter().all(|pin| pin.pointer() == pins[0].pointer()));
            assert_eq!(pins[0].owner().as_object().as_i64().unwrap(), 42);
            let snapshot = domain.snapshot();
            assert_eq!(snapshot.loads, 1);
            assert_eq!(snapshot.ready, 1);
            assert_eq!(snapshot.live_interests, 0);
        }
    }

    #[test]
    fn persistent_negative_reuses_cell_while_flight_only_retries() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let stable_loads = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..2 {
            let error = arena
                .resolve((7, 0), |_| {
                    stable_loads.fetch_add(1, Ordering::Relaxed);
                    Err(CellLoadError::new(
                        AccessError::typed((7, 0), AccessKind::Backend, "stable"),
                        NegativeDisposition::Persistent,
                    ))
                })
                .unwrap_err();
            assert_eq!(error.detail, "stable");
        }
        assert_eq!(stable_loads.load(Ordering::Relaxed), 1);

        let transient_loads = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..2 {
            let error = arena
                .resolve((8, 0), |_| {
                    transient_loads.fetch_add(1, Ordering::Relaxed);
                    Err(CellLoadError::new(
                        AccessError::typed((8, 0), AccessKind::SourceIo, "retry"),
                        NegativeDisposition::FlightOnly,
                    ))
                })
                .unwrap_err();
            assert_eq!(error.detail, "retry");
        }
        assert_eq!(transient_loads.load(Ordering::Relaxed), 2);
        assert_eq!(domain.snapshot().negative_hits, 1);
    }

    #[test]
    fn neutral_objstm_failures_share_one_arc_and_one_stored_weight() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let persistent_id = (70, 0);
        let first = arena
            .resolve_object_stream(persistent_id, |_| {
                Err(CellLoadError::objstm(crate::objstm_failures::classify(
                    persistent_id,
                    lopdf::IndexedReaderError::ObjectStreamContainerNotStream {
                        id: persistent_id,
                        container: persistent_id,
                    },
                )))
            })
            .unwrap_err();
        let second = arena
            .resolve_object_stream(persistent_id, |_| panic!("persistent failure must hit"))
            .unwrap_err();
        assert_eq!(first.shared_pointer(), second.shared_pointer());
        let ContainerCellError::Shared(owner) = &first else {
            panic!("persistent template must remain Arc-shared")
        };
        assert!(matches!(
            owner.payload(),
            FailurePayload::ObjStm(template)
                if template.class() == ObjStmFailureClass::PersistentNative
        ));
        assert_eq!(
            owner.retained_weight(),
            owner.payload().retained_weight().unwrap()
        );

        let flight_id = (71, 0);
        let flight_leader = arena
            .inner
            .request_representation(flight_id, Representation::DeclaredObjStmContainer)
            .unwrap();
        let flight_waiter = arena
            .inner
            .request_representation(flight_id, Representation::DeclaredObjStmContainer)
            .unwrap();
        let flight_first = flight_leader
            .resolve_object_stream(|_| {
                Err(CellLoadError::objstm(crate::objstm_failures::classify(
                    flight_id,
                    lopdf::IndexedReaderError::Source(lopdf::SourceError::SourceChanged),
                )))
            })
            .unwrap_err();
        let flight_second = flight_waiter
            .resolve_object_stream(|_| panic!("attached waiter must share the flight"))
            .unwrap_err();
        assert_eq!(
            flight_first.shared_pointer(),
            flight_second.shared_pointer()
        );
        let ContainerCellError::Shared(owner) = &flight_first else {
            panic!("flight template must remain Arc-shared")
        };
        assert!(matches!(
            owner.payload(),
            FailurePayload::ObjStm(template)
                if template.class() == ObjStmFailureClass::FlightOnly
        ));
        assert_eq!(domain.snapshot().negative_hits, 1);
        assert_eq!(domain.snapshot().transient_shares, 1);

        drop(flight_second);
        drop(flight_first);
        drop(second);
        drop(first);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn dynamic_persistent_publication_preserves_one_owned_allocation_and_exact_weight() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let id = (80, 0);
        let mut detail = String::with_capacity(191);
        detail.push_str("stable dynamic object-stream failure");
        let expected_pointer = detail.as_ptr() as usize;
        let expected_capacity = u64::try_from(detail.capacity()).unwrap();
        let expected_weight = ERROR_OWNER_BYTES + expected_capacity;

        let first = match arena
            .resolve_object_stream(id, |_| {
                Err(CellLoadError::objstm(crate::objstm_failures::classify(
                    id,
                    lopdf::IndexedReaderError::ObjectStreamMember {
                        id,
                        container: id,
                        index: 0,
                        source: lopdf::Error::InvalidObjectStream(detail),
                    },
                )))
            })
            .expect_err("dynamic failure must persist")
        {
            ContainerCellError::Shared(owner) => owner,
            ContainerCellError::Control(_) => panic!("dynamic failure became control"),
        };
        let second = match arena
            .resolve_object_stream(id, |_| panic!("persistent dynamic failure must hit"))
            .expect_err("dynamic failure hit")
        {
            ContainerCellError::Shared(owner) => owner,
            ContainerCellError::Control(_) => panic!("dynamic hit became control"),
        };

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            first.objstm_dynamic_allocation(),
            Some((expected_pointer, expected_capacity))
        );
        assert_eq!(first.retained_weight(), expected_weight);
        assert_eq!(first.payload().retained_weight(), Ok(expected_weight));
        assert_eq!(
            lock(&first.charge)
                .as_ref()
                .expect("persistent failure charge")
                .bytes(),
            expected_weight
        );
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 1);
        assert_eq!(snapshot.loading, 0);
        assert_eq!(snapshot.negative, 1);
        assert_eq!(snapshot.cache_bytes, CELL_METADATA_BYTES + expected_weight);
        assert_eq!(snapshot.containers.calls, 2);
        assert_eq!(snapshot.containers.loads, 1);
        assert_eq!(snapshot.containers.negative_hits, 1);
        assert_eq!(snapshot.containers.hits, 0);
        assert_eq!(snapshot.containers.transient_shares, 0);
        assert_eq!(snapshot.containers.bypasses, 0);
        assert_eq!(snapshot.containers.evictions, 0);
        assert_eq!(snapshot.containers.cancellations, 0);
        assert_eq!(snapshot.raw, RepresentationSnapshot::default());
        assert_eq!(snapshot.members, RepresentationSnapshot::default());
        assert_representation_counter_sums(&snapshot);
        let operation = broker.snapshot().operations[&arena.epoch()].clone();
        assert_eq!(
            operation.cache_bytes,
            ARENA_METADATA_BYTES + CELL_METADATA_BYTES + expected_weight
        );
        assert_eq!(operation.pin_bytes, 0);
        assert_eq!(operation.bypass_bytes, 0);

        drop(second);
        drop(first);
        arena.close();
        drop(arena);
        let drained = broker.snapshot();
        assert_eq!(drained.aggregate_bytes, 0);
        assert_eq!(drained.normal_payload_bytes, 0);
        assert_eq!(drained.completion_reserve_bytes, 0);
        assert_eq!(drained.oversize_bytes, 0);
        assert_eq!(drained.active_operations, 0);
    }

    #[test]
    fn post_cell_broker_error_shares_attached_flight_then_drops_before_one_successor() {
        for callers in [2_usize, 4, 16] {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let (stream_reader, _, container, _) = object_stream_reader();
            let leader = arena
                .inner
                .request_representation(container, Representation::DeclaredObjStmContainer)
                .unwrap();
            let mut followers = Vec::new();
            for _ in 1..callers {
                followers.push(
                    arena
                        .inner
                        .request_representation(container, Representation::DeclaredObjStmContainer)
                        .unwrap(),
                );
            }
            leader.publish_broker_error(1, BrokerError::Closed);

            let mut owners = Vec::new();
            owners.push(
                match leader
                    .resolve_object_stream(|_| panic!("broker failure bypasses loader"))
                    .expect_err("leader broker failure")
                {
                    ContainerCellError::Shared(owner) => owner,
                    ContainerCellError::Control(_) => {
                        panic!("post-cell broker error became control")
                    }
                },
            );
            for follower in followers {
                owners.push(
                    match follower
                        .resolve_object_stream(|_| panic!("broker waiter bypasses loader"))
                        .expect_err("waiter broker failure")
                    {
                        ContainerCellError::Shared(owner) => owner,
                        ContainerCellError::Control(_) => {
                            panic!("attached broker waiter became control")
                        }
                    },
                );
            }
            assert!(owners.iter().all(|owner| Arc::ptr_eq(owner, &owners[0])));
            assert!(owners[0].cell_envelope);
            assert!(matches!(owners[0].payload(), FailurePayload::Access(_)));
            let flight = domain.snapshot();
            assert_eq!(flight.cells, 0);
            assert_eq!(flight.loading, 0);
            assert_eq!(flight.live_interests, 0);
            assert_eq!(flight.containers.calls, callers as u64);
            assert_eq!(flight.containers.loads, 0);
            assert_eq!(flight.containers.waits, (callers - 1) as u64);
            assert_eq!(flight.containers.transient_shares, (callers - 1) as u64);
            assert_eq!(flight.containers.negative_hits, 0);
            assert_eq!(flight.containers.bypasses, 0);
            assert_eq!(flight.containers.evictions, 0);
            assert_eq!(flight.containers.cancellations, 0);
            assert_eq!(flight.raw, RepresentationSnapshot::default());
            assert_eq!(flight.members, RepresentationSnapshot::default());
            assert_representation_counter_sums(&flight);
            let old_dropped = broker.snapshot().operations[&arena.epoch()].clone();
            assert_eq!(old_dropped.queued, 0);
            assert_eq!(old_dropped.in_flight, 0);
            assert_eq!(old_dropped.error_owners, 0);
            assert_eq!(old_dropped.cache_bytes, ARENA_METADATA_BYTES);
            assert_eq!(old_dropped.pin_bytes, 0);
            assert_eq!(old_dropped.bypass_bytes, 0);
            drop(owners);
            let after_owner_drop = broker.snapshot().operations[&arena.epoch()].clone();
            assert_eq!(after_owner_drop, old_dropped);

            let pin = arena
                .resolve_object_stream(container, |permit| {
                    stream_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                })
                .unwrap();
            assert_eq!(pin.as_object_stream().container_id(), container);
            let terminal = domain.snapshot();
            assert_eq!(terminal.containers.calls, callers as u64 + 1);
            assert_eq!(terminal.containers.loads, 1);
            assert_eq!(terminal.containers.hits, 0);
            assert_eq!(terminal.containers.waits, (callers - 1) as u64);
            assert_eq!(terminal.containers.negative_hits, 0);
            assert_eq!(terminal.containers.transient_shares, (callers - 1) as u64);
            assert_eq!(terminal.containers.bypasses, 0);
            assert_eq!(terminal.containers.evictions, 0);
            assert_eq!(terminal.containers.cancellations, 0);
            assert_representation_counter_sums(&terminal);
            let (retained, retained_permit, charge) = pin.retained_evidence();
            assert_eq!(retained_permit.stats().current_bytes, retained);
            assert_eq!(charge, retained);

            drop(pin);
            arena.close();
            drop(arena);
            assert_eq!(retained_permit.stats().current_bytes, 0);
            let drained = broker.snapshot();
            assert_eq!(drained.aggregate_bytes, 0);
            assert_eq!(drained.normal_payload_bytes, 0);
            assert_eq!(drained.normal_in_flight_estimate_bytes, 0);
            assert_eq!(drained.completion_reserve_bytes, 0);
            assert_eq!(drained.oversize_bytes, 0);
            assert_eq!(drained.oversize_owners, 0);
            assert_eq!(drained.cache_bytes, 0);
            assert_eq!(drained.pin_bytes, 0);
            assert_eq!(drained.bypass_bytes, 0);
            assert_eq!(drained.queued, 0);
            assert_eq!(drained.in_flight, 0);
            assert_eq!(drained.active_operations, 0);
            assert!(drained.operations.is_empty());
        }
    }

    #[test]
    fn objstm_exact_invariant_closes_only_the_exact_container_generation() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let id = (72, 0);
        let raw = arena.request(id).unwrap();
        let control = arena
            .resolve_object_stream(id, |_| {
                Err(CellLoadError::objstm(crate::objstm_failures::classify(
                    id,
                    lopdf::IndexedReaderError::ScalarResourceLimit {
                        id,
                        requested: LOADER_ESTIMATE_BYTES,
                        limit: LOADER_ESTIMATE_BYTES,
                        phase: "scalar-frame",
                    },
                )))
            })
            .unwrap_err()
            .into_access_for_test();
        assert_eq!(
            control.detail,
            CellControlTag::PermitOrMeasurementInvariant.detail()
        );
        let value_reader = reader(173);
        let raw_pin = raw
            .resolve(|permit| load(&value_reader, (1, 0), permit))
            .unwrap();
        assert_eq!(raw_pin.owner().as_object().as_i64().unwrap(), 173);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 1);
        assert_eq!(snapshot.ready, 1);
        assert_eq!(snapshot.containers.loads, 1);
        assert!(snapshot.invariant_failed);
    }

    #[test]
    fn cancelled_running_exact_failure_is_dropped_before_fifo_successor() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let id = (73, 0);
        let leader = arena
            .inner
            .request_representation(id, Representation::DeclaredObjStmContainer)
            .unwrap();
        let cancel = leader.cancellation_handle();
        let successor = arena
            .inner
            .request_representation(id, Representation::DeclaredObjStmContainer)
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (return_tx, return_rx) = mpsc::sync_channel(0);
        let leader_join = thread::spawn(move || {
            leader.resolve_object_stream(|_| {
                entered_tx.send(()).unwrap();
                return_rx.recv().unwrap();
                Err(CellLoadError::objstm(crate::objstm_failures::classify(
                    id,
                    lopdf::IndexedReaderError::ScalarResourceLimit {
                        id,
                        requested: LOADER_ESTIMATE_BYTES,
                        limit: LOADER_ESTIMATE_BYTES,
                        phase: "scalar-frame",
                    },
                )))
            })
        });
        entered_rx.recv().unwrap();
        cancel.cancel();
        return_tx.send(()).unwrap();
        assert!(leader_join.join().unwrap().is_err());
        assert!(!domain.snapshot().invariant_failed);

        let (stream_reader, _, container, _) = object_stream_reader();
        let pin = successor
            .resolve_object_stream(|permit| {
                stream_reader
                    .prepare_object_stream_with_permit(container, permit)
                    .map_err(|error| {
                        CellLoadError::objstm(crate::objstm_failures::classify(id, error))
                    })
            })
            .unwrap();
        assert_eq!(pin.as_object_stream().container_id(), container);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.ready, 1);
        assert_eq!(snapshot.negative, 0);
        assert_eq!(snapshot.containers.loads, 2);
        assert_eq!(snapshot.containers.transient_shares, 0);
    }

    #[test]
    fn cancelled_running_weight_refusals_are_stale_and_re_elect_fifo() {
        for result in [
            Err(RetainedWeightError::Overflow),
            Err(RetainedWeightError::OverAttempt {
                weight: LOADER_ESTIMATE_BYTES + 1,
                limit: LOADER_ESTIMATE_BYTES,
            }),
        ] {
            let domain =
                ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let leader = arena.request((74, 0)).unwrap();
            let cancel = leader.cancellation_handle();
            let successor = arena.request((74, 0)).unwrap();
            {
                let mut state = lock(&leader.cell.state);
                let CellPhase::Loading(loading) = &mut state.phase else {
                    panic!("expected loading cell")
                };
                loading.leader_running = true;
            }
            cancel.cancel();
            assert!(arena
                .inner
                .admit_failure_weight_result(&leader.cell, leader.slot, 1, result)
                .is_err());
            assert!(!domain.snapshot().invariant_failed);
            let value_reader = reader(174);
            let pin = successor
                .resolve(|permit| load(&value_reader, (1, 0), permit))
                .unwrap();
            assert_eq!(pin.owner().as_object().as_i64().unwrap(), 174);
            assert_eq!(domain.snapshot().ready, 1);
        }
    }

    #[test]
    fn failure_payload_representation_mismatch_closes_before_any_shared_publication() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();

        let container_id = (75, 0);
        let raw_sibling = arena.request(container_id).unwrap();
        let container_error = arena
            .resolve_object_stream(container_id, |_| {
                Err(CellLoadError::new(
                    AccessError::typed(container_id, AccessKind::Backend, "wrong payload"),
                    NegativeDisposition::Persistent,
                ))
            })
            .unwrap_err()
            .into_access_for_test();
        assert_eq!(
            container_error.detail,
            CellControlTag::PayloadMismatch.detail()
        );

        for (offset, representation) in [
            Representation::RawNormalObject,
            Representation::DeclaredObjStmMember,
        ]
        .into_iter()
        .enumerate()
        {
            let id = (76 + u32::try_from(offset).unwrap(), 0);
            let request = arena
                .inner
                .request_representation(id, representation)
                .unwrap();
            let error = request
                .resolve(|_| Err(container_persistent(id)))
                .unwrap_err();
            assert_eq!(error.detail, CellControlTag::PayloadMismatch.detail());
        }
        let value_reader = reader(175);
        let pin = raw_sibling
            .resolve(|permit| load(&value_reader, (1, 0), permit))
            .unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 175);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 1);
        assert_eq!(snapshot.ready, 1);
        assert_eq!(snapshot.negative, 0);
        assert_eq!(snapshot.transient_shares, 0);
        assert!(snapshot.invariant_failed);

        let broker_id = (78, 0);
        let broker_request = arena
            .inner
            .request_representation(broker_id, Representation::DeclaredObjStmContainer)
            .unwrap();
        broker_request.publish_broker_error(1, BrokerError::Closed);
        let broker_owner = match broker_request
            .resolve_object_stream(|_| panic!("broker envelope bypasses loader"))
            .unwrap_err()
        {
            ContainerCellError::Shared(owner) => owner,
            ContainerCellError::Control(_) => panic!("claimed broker envelope must be shared"),
        };
        assert!(broker_owner.cell_envelope);
        assert!(matches!(broker_owner.payload(), FailurePayload::Access(_)));
    }

    #[test]
    fn cached_negative_accounting_overflow_exact_tears_down_without_flight_fallback() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let request = arena.request((79, 0)).unwrap();
        let payload = FailurePayload::Access(AccessError::typed(
            (79, 0),
            AccessKind::Backend,
            "cached negative overflow",
        ));
        let retained_weight = payload.retained_weight().unwrap();
        let reservation = arena
            .inner
            .operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                retained_weight,
            )
            .unwrap();
        let charge = reservation.reconcile(retained_weight).unwrap();
        let owner = Arc::new(FailureOwner {
            payload,
            retained_weight,
            charge: Mutex::new(Some(charge)),
            _reservation: Mutex::new(None),
            cell_envelope: false,
        });
        lock(&request.cell.state).completed_weight = u64::MAX;
        arena.inner.publish_error(
            &request.cell,
            request.slot,
            1,
            Arc::clone(&owner),
            NegativeDisposition::Persistent,
        );
        assert!(matches!(
            lock(&request.cell.state).phase,
            CellPhase::Closed(_)
        ));
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 0);
        assert_eq!(snapshot.negative, 0);
        assert_eq!(snapshot.transient_shares, 0);
        assert!(snapshot.invariant_failed);
        assert_eq!(
            broker.snapshot().operations[&arena.epoch()].bypass_bytes,
            CELL_METADATA_BYTES + retained_weight
        );
        drop(owner);
        drop(request);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn broker_failure_uses_precharged_cell_error_envelope() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let request = arena.request((4, 0)).unwrap();
        arena.inner.operation.close();
        let error = request
            .resolve(|_| panic!("closed operation must fail before loader invocation"))
            .unwrap_err();
        assert_eq!(error.kind, AccessKind::Backend);
        assert_eq!(error.detail, "budget broker operation is closed");
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 0);
        assert_eq!(snapshot.cache_bytes, 0);
        assert_eq!(snapshot.live_interests, 0);
        assert_eq!(snapshot.loads, 0);
        assert!(!snapshot.invariant_failed);
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn every_broker_error_variant_fits_the_checked_cell_envelope() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let errors = [
            BrokerError::InvalidConfig,
            BrokerError::Closed,
            BrokerError::OperationClosed,
            BrokerError::Cancelled,
            BrokerError::ArithmeticOverflow,
            BrokerError::ResourceLimit,
            BrokerError::QueueFull,
            BrokerError::OperationFull,
            BrokerError::SelfPinned,
            BrokerError::ReconciliationLimit,
        ];
        for (index, error) in errors.into_iter().enumerate() {
            let id = (u32::try_from(index + 1).unwrap(), 0);
            let expected = broker_error(id, error.clone());
            assert!(ERROR_OWNER_BYTES
                .checked_add(expected.detail.capacity() as u64)
                .is_some_and(|bytes| bytes <= CELL_ERROR_ENVELOPE_BYTES));
            let request = arena.request(id).unwrap();
            request.publish_broker_error(1, error);
            let actual = request
                .resolve(|_| panic!("injected broker error must bypass the loader"))
                .unwrap_err();
            assert_eq!(actual, expected);
        }
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.loads, 0);
        assert_eq!(snapshot.cells, 0);
        assert_eq!(snapshot.live_interests, 0);
        assert!(!snapshot.invariant_failed);
    }

    #[test]
    fn leader_cancellation_acknowledges_before_fifo_re_election() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let first = arena.request((1, 0)).unwrap();
        let cancel = first.cancellation_handle();
        let second = arena.request((1, 0)).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = mpsc::sync_channel(0);
        let first_join = thread::spawn(move || {
            first.resolve(|_| {
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                Err(transient((1, 0), "cancelled leader"))
            })
        });
        started_rx.recv().unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let reader = reader(77);
        let active_second = Arc::clone(&active);
        let peak_second = Arc::clone(&peak);
        let second_join = thread::spawn(move || {
            second.resolve(|permit| {
                let now = active_second.fetch_add(1, Ordering::AcqRel) + 1;
                peak_second.fetch_max(now, Ordering::AcqRel);
                let result = load(&reader, (1, 0), permit);
                active_second.fetch_sub(1, Ordering::AcqRel);
                result
            })
        });
        cancel.cancel();
        finish_tx.send(()).unwrap();
        assert!(first_join.join().unwrap().is_err());
        let pin = second_join.join().unwrap().unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 77);
        assert_eq!(peak.load(Ordering::Acquire), 1);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.loads, 2);
        assert_eq!(snapshot.cancellations, 1);
        assert_eq!(snapshot.live_interests, 0);
    }

    #[test]
    fn waiter_cancel_is_local_and_stale_handle_cannot_cancel_reused_slot() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let leader = arena.request((1, 0)).unwrap();
        let waiter = arena.request((1, 0)).unwrap();
        let waiter_cancel = waiter.cancellation_handle();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = mpsc::sync_channel(0);
        let reader = reader(11);
        let leader_join = thread::spawn(move || {
            leader.resolve(|permit| {
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                load(&reader, (1, 0), permit)
            })
        });
        started_rx.recv().unwrap();
        let waiter_join =
            thread::spawn(move || waiter.resolve(|_| panic!("cancelled waiter must not lead")));
        waiter_cancel.cancel();
        assert!(waiter_join.join().unwrap().is_err());
        finish_tx.send(()).unwrap();
        let first_pin = leader_join.join().unwrap().unwrap();
        drop(first_pin);

        let reused = arena.request((1, 0)).unwrap();
        waiter_cancel.cancel();
        let pin = reused
            .resolve(|_| panic!("ready hit must not invoke loader"))
            .unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 11);
        assert_eq!(domain.snapshot().live_interests, 0);
    }

    #[test]
    fn panic_re_elects_oldest_waiter_without_duplicate_loader() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let first = arena.request((1, 0)).unwrap();
        let second = arena.request((1, 0)).unwrap();
        let panic_join = thread::spawn(move || {
            first.resolve(|_| -> Result<BoundedObject, CellLoadError> {
                panic!("injected loader panic")
            })
        });
        let reader = reader(23);
        let second_join =
            thread::spawn(move || second.resolve(|permit| load(&reader, (1, 0), permit)));
        assert!(panic_join.join().is_err());
        let pin = second_join.join().unwrap().unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 23);
        assert_eq!(domain.snapshot().live_interests, 0);
    }

    #[test]
    fn transient_failure_is_shared_only_with_attached_generation_then_retried() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = Arc::new(domain.open_arena().unwrap());
        let attached = Arc::new(Barrier::new(9));
        let (leader_tx, leader_rx) = mpsc::sync_channel(0);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let loads = Arc::new(AtomicU64::new(0));
        let mut joins = Vec::new();
        for _ in 0..8 {
            let arena = Arc::clone(&arena);
            let attached = Arc::clone(&attached);
            let gate = Arc::clone(&gate);
            let loads = Arc::clone(&loads);
            let leader_tx = leader_tx.clone();
            joins.push(thread::spawn(move || {
                let request = arena.request((9, 0)).unwrap();
                attached.wait();
                request.resolve(|_| {
                    loads.fetch_add(1, Ordering::Relaxed);
                    let _ = leader_tx.send(());
                    let (open, ready) = &*gate;
                    let mut open = lock(open);
                    while !*open {
                        open = wait(ready, open);
                    }
                    Err(transient((9, 0), "one flight"))
                })
            }));
        }
        attached.wait();
        leader_rx.recv().unwrap();
        {
            let (open, ready) = &*gate;
            *lock(open) = true;
            ready.notify_all();
        }
        for join in joins {
            assert_eq!(join.join().unwrap().unwrap_err().detail, "one flight");
        }
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        let reader = reader(91);
        let retry = arena
            .resolve((9, 0), |permit| load(&reader, (1, 0), permit))
            .unwrap();
        assert_eq!(retry.owner().as_object().as_i64().unwrap(), 91);
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(domain.snapshot().transient_shares, 7);
    }

    #[test]
    fn full_id_and_document_epoch_are_isolated() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let first_arena = domain.open_arena().unwrap();
        let second_arena = domain.open_arena().unwrap();
        assert_ne!(first_arena.epoch(), second_arena.epoch());
        let first_reader = reader(1);
        let second_reader = reader(2);
        let first = first_arena
            .resolve((1, 0), |permit| load(&first_reader, (1, 0), permit))
            .unwrap();
        let second = second_arena
            .resolve((1, 0), |permit| load(&second_reader, (1, 0), permit))
            .unwrap();
        assert_ne!(first.pointer(), second.pointer());
        assert_eq!(first.owner().as_object().as_i64().unwrap(), 1);
        assert_eq!(second.owner().as_object().as_i64().unwrap(), 2);
        let wrong_generation = first_arena
            .resolve((1, 1), |permit| load(&first_reader, (1, 1), permit))
            .unwrap_err();
        assert_eq!(wrong_generation.object, (1, 1));
        assert_eq!(domain.snapshot().cells, 3);
    }

    #[test]
    fn global_lru_evicts_across_documents_but_never_evicts_pins() {
        let domain = ObjectCellDomain::new(
            broker(),
            ObjectCellConfig::scaled(CELL_METADATA_BYTES + 512),
        );
        let first_arena = domain.open_arena().unwrap();
        let second_arena = domain.open_arena().unwrap();
        let first_reader = reader(10);
        let second_reader = reader(20);
        let first_loads = AtomicU64::new(0);
        let second_loads = AtomicU64::new(0);

        let first = first_arena
            .resolve((1, 0), |permit| {
                first_loads.fetch_add(1, Ordering::Relaxed);
                load(&first_reader, (1, 0), permit)
            })
            .unwrap();
        drop(first);
        let second = second_arena
            .resolve((1, 0), |permit| {
                second_loads.fetch_add(1, Ordering::Relaxed);
                load(&second_reader, (1, 0), permit)
            })
            .unwrap();
        drop(second);
        assert_eq!(domain.snapshot().cells, 1);
        let reloaded = first_arena
            .resolve((1, 0), |permit| {
                first_loads.fetch_add(1, Ordering::Relaxed);
                load(&first_reader, (1, 0), permit)
            })
            .unwrap();
        assert_eq!(reloaded.owner().as_object().as_i64().unwrap(), 10);
        assert_eq!(first_loads.load(Ordering::Relaxed), 2);
        assert_eq!(second_loads.load(Ordering::Relaxed), 1);
        assert!(domain.snapshot().evictions >= 2);

        let pinned_domain = ObjectCellDomain::new(
            broker(),
            ObjectCellConfig::scaled(CELL_METADATA_BYTES + 512),
        );
        let pinned_arena = pinned_domain.open_arena().unwrap();
        let bypass_arena = pinned_domain.open_arena().unwrap();
        let pinned_reader = reader(30);
        let bypass_reader = reader(40);
        let pinned = pinned_arena
            .resolve((1, 0), |permit| load(&pinned_reader, (1, 0), permit))
            .unwrap();
        let bypass_loads = AtomicU64::new(0);
        for _ in 0..2 {
            let value = bypass_arena
                .resolve((1, 0), |permit| {
                    bypass_loads.fetch_add(1, Ordering::Relaxed);
                    load(&bypass_reader, (1, 0), permit)
                })
                .unwrap();
            assert_eq!(value.owner().as_object().as_i64().unwrap(), 40);
        }
        assert_eq!(bypass_loads.load(Ordering::Relaxed), 2);
        assert_eq!(pinned.owner().as_object().as_i64().unwrap(), 30);
        let snapshot = pinned_domain.snapshot();
        assert_eq!(snapshot.cells, 1);
        assert_eq!(snapshot.external_pins, 1);
        assert_eq!(snapshot.bypasses, 2);
    }

    #[test]
    fn completed_cache_at_target_leaves_exact_loader_headroom() {
        let broker = broker();
        let domain = ObjectCellDomain::new(
            broker.clone(),
            ObjectCellConfig::scaled(PRODUCTION_CACHE_TARGET_BYTES),
        );
        let arena = domain.open_arena().unwrap();
        for number in 1..=7_600u32 {
            let id = (number, 0);
            arena
                .resolve(id, |_| {
                    Err(CellLoadError::new(
                        AccessError::typed(id, AccessKind::Backend, "stable headroom miss"),
                        NegativeDisposition::Persistent,
                    ))
                })
                .unwrap_err();
        }
        let before = domain.snapshot();
        assert!(before.cache_bytes > 31 * 1024 * 1024);
        assert!(before.cache_bytes <= PRODUCTION_CACHE_TARGET_BYTES);

        let object_reader = reader(73);
        let pin = arena
            .resolve((8_000, 0), |permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 73);
        assert_eq!(broker.snapshot().queued, 0);
        // One arena precharge plus one cell and one loader grant per call.
        assert_eq!(arena.inner.operation.grant_count(), 1 + 2 * 7_601);
    }

    #[test]
    fn broker_global_arena_metadata_reclaims_cache_before_loader_enqueue() {
        let broker = headroom_broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(8 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        for number in 1..=1_920u32 {
            let id = (number, 0);
            arena
                .resolve(id, |_| {
                    Err(CellLoadError::new(
                        AccessError::typed(id, AccessKind::Backend, "global headroom cache"),
                        NegativeDisposition::Persistent,
                    ))
                })
                .unwrap_err();
        }
        let filled = domain.snapshot();
        assert!(filled.cache_bytes > 7 * 1024 * 1024);

        let mut idle_arenas = Vec::new();
        for _ in 0..1_500 {
            idle_arenas.push(domain.open_arena().unwrap());
        }
        let reclaimed = domain.snapshot();
        assert!(reclaimed.evictions > filled.evictions);
        assert!(reclaimed.cache_bytes < filled.cache_bytes);

        let object_reader = reader(74);
        let pin = idle_arenas
            .last()
            .unwrap()
            .resolve((9_000, 0), |permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 74);
        assert_eq!(broker.snapshot().queued, 0);
    }

    #[test]
    fn loading_metadata_has_an_independent_exact_sixteen_mib_cap() {
        let domain = ObjectCellDomain::new(
            broker(),
            ObjectCellConfig::scaled(PRODUCTION_CACHE_TARGET_BYTES),
        );
        let arena = domain.open_arena().unwrap();
        let mut requests = Vec::new();
        let exact_cells = MAX_LOADING_METADATA_BYTES / CELL_METADATA_BYTES;
        for number in 1..=u32::try_from(exact_cells).unwrap() {
            let representation = match number % 3 {
                0 => Representation::RawNormalObject,
                1 => Representation::DeclaredObjStmContainer,
                _ => Representation::DeclaredObjStmMember,
            };
            requests.push(
                arena
                    .inner
                    .request_representation((number, 0), representation)
                    .unwrap(),
            );
        }
        let error = arena
            .inner
            .request_representation(
                (u32::try_from(exact_cells).unwrap() + 1, 0),
                Representation::DeclaredObjStmMember,
            )
            .err()
            .expect("the first Loading cell above the byte cap must be refused");
        assert_eq!(error.kind, AccessKind::ResourceLimit);
        assert_eq!(
            lock(&domain.inner.state).loading_metadata_bytes,
            exact_cells * CELL_METADATA_BYTES
        );
        assert_eq!(domain.snapshot().loading, exact_cells as usize);
        drop(requests);
        assert_eq!(lock(&domain.inner.state).loading_metadata_bytes, 0);
        assert_eq!(domain.snapshot().cells, 0);
    }

    #[test]
    fn published_cancel_is_inert_drop_detaches_and_bypass_is_self_pinned() {
        let broker = broker();
        let domain = ObjectCellDomain::new(
            broker.clone(),
            ObjectCellConfig::scaled(CELL_METADATA_BYTES),
        );
        let arena = domain.open_arena().unwrap();
        let leader = arena.request((1, 0)).unwrap();
        let abandoned = arena.request((1, 0)).unwrap();
        let stale_cancel = abandoned.cancellation_handle();
        let object_reader = reader(91);
        let pin = leader
            .resolve(|permit| load(&object_reader, (1, 0), permit))
            .unwrap();

        assert_eq!(domain.snapshot().live_interests, 1);
        stale_cancel.cancel();
        assert_eq!(domain.snapshot().live_interests, 1);
        drop(abandoned);
        assert_eq!(domain.snapshot().live_interests, 0);
        assert_eq!(domain.snapshot().cells, 0);

        let operation = &broker.snapshot().operations[&arena.epoch()];
        assert_eq!(operation.bypass_bytes, pin.owner().retained_bytes());
        assert_eq!(operation.self_pinned_bytes, pin.owner().retained_bytes());
        drop(pin);
        assert_eq!(
            broker.snapshot().operations[&arena.epoch()].self_pinned_bytes,
            0
        );
    }

    #[test]
    fn hard_cell_cap_refuses_before_loader_work_when_every_cell_is_pinned() {
        let config = ObjectCellConfig::scaled(32 * 1024 * 1024).with_caps(1, 64);
        let domain = ObjectCellDomain::new(broker(), config);
        let first_arena = domain.open_arena().unwrap();
        let second_arena = domain.open_arena().unwrap();
        let first_reader = reader(1);
        let pin = first_arena
            .resolve((1, 0), |permit| load(&first_reader, (1, 0), permit))
            .unwrap();
        let called = AtomicBool::new(false);
        let error = second_arena
            .resolve_declared_member((9, 7), |_| {
                called.store(true, Ordering::Release);
                Err(transient((9, 7), "must not run"))
            })
            .unwrap_err();
        assert_eq!(error.object, (9, 7));
        assert_eq!(error.kind, AccessKind::CellFull);
        assert!(!called.load(Ordering::Acquire));
        assert_eq!(domain.snapshot().cells, 1);
        drop(pin);
    }

    #[test]
    fn global_cell_cap_and_publication_bytes_hold_under_two_epoch_races() {
        let cap_domain = ObjectCellDomain::new(
            broker(),
            ObjectCellConfig::scaled(32 * 1024 * 1024).with_caps(1, 64),
        );
        let first_arena = cap_domain.open_arena().unwrap();
        let second_arena = cap_domain.open_arena().unwrap();
        let _cap_keeps = [first_arena.clone(), second_arena.clone()];
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (result_tx, result_rx) = mpsc::channel();
        for (arena, value) in [(first_arena, 1), (second_arena, 2)] {
            let gate = Arc::clone(&gate);
            let started_tx = started_tx.clone();
            let result_tx = result_tx.clone();
            thread::spawn(move || {
                let value_reader = reader(value);
                let result = arena.resolve((1, 0), |permit| {
                    started_tx.send(()).unwrap();
                    let (open, ready) = &*gate;
                    let mut open = lock(open);
                    while !*open {
                        open = wait(ready, open);
                    }
                    load(&value_reader, (1, 0), permit)
                });
                result_tx.send(result).unwrap();
            });
        }
        started_rx.recv().unwrap();
        let refused = result_rx.recv().unwrap().unwrap_err();
        assert_eq!(refused.kind, AccessKind::CellFull);
        assert_eq!(cap_domain.snapshot().cells, 1);
        {
            let (open, ready) = &*gate;
            *lock(open) = true;
            ready.notify_all();
        }
        assert!(result_rx.recv().unwrap().is_ok());
        assert!(cap_domain.snapshot().cells <= 1);

        let publication_domain = ObjectCellDomain::new(
            wide_broker(),
            ObjectCellConfig::scaled(CELL_METADATA_BYTES + 512),
        );
        let first_arena = publication_domain.open_arena().unwrap();
        let second_arena = publication_domain.open_arena().unwrap();
        let _publication_keeps = [first_arena.clone(), second_arena.clone()];
        let publish = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for (arena, value) in [(first_arena, 3), (second_arena, 4)] {
            let publish = Arc::clone(&publish);
            joins.push(thread::spawn(move || {
                let value_reader = reader(value);
                arena.resolve((1, 0), |permit| {
                    let object = load(&value_reader, (1, 0), permit)?;
                    publish.wait();
                    Ok(object)
                })
            }));
        }
        publish.wait();
        let pins: Vec<_> = joins
            .into_iter()
            .map(|join| join.join().unwrap().unwrap())
            .collect();
        let snapshot = publication_domain.snapshot();
        assert!(snapshot.cache_bytes <= CELL_METADATA_BYTES + 512);
        assert_eq!(snapshot.cells, 1);
        assert_eq!(snapshot.bypasses, 1);
        assert_ne!(pins[0].pointer(), pins[1].pointer());
    }

    #[test]
    fn fixed_interest_limits_refuse_without_allocating_waiter_nodes() {
        let domain = ObjectCellDomain::new(
            broker(),
            ObjectCellConfig::scaled(32 * 1024 * 1024).with_caps(16_384, 2),
        );
        let arena = domain.open_arena().unwrap();
        let first = arena.request((1, 0)).unwrap();
        let second = arena.request((1, 0)).unwrap();
        let global = arena.request((2, 0)).err().expect("global interest cap");
        assert_eq!(global.kind, AccessKind::ResourceLimit);
        drop(first);
        drop(second);

        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let mut requests = Vec::new();
        for _ in 0..64 {
            requests.push(arena.request((1, 0)).unwrap());
        }
        let per_cell = arena.request((1, 0)).err().expect("per-cell interest cap");
        assert_eq!(per_cell.kind, AccessKind::ResourceLimit);
        drop(requests);
        assert_eq!(domain.snapshot().live_interests, 0);
    }

    #[test]
    fn failed_initial_attach_and_touch_overflow_leave_no_partial_state() {
        let domain = ObjectCellDomain::new(
            broker(),
            ObjectCellConfig::scaled(32 * 1024 * 1024).with_caps(16_384, 0),
        );
        let arena = domain.open_arena().unwrap();
        let error = arena
            .request((1, 0))
            .err()
            .expect("initial attach must fail");
        assert_eq!(error.kind, AccessKind::ResourceLimit);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 0);
        assert_eq!(snapshot.cache_bytes, 0);
        assert_eq!(snapshot.live_interests, 0);

        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let request = arena.request((1, 0)).unwrap();
        lock(&domain.inner.state).touch = u64::MAX;
        let object_reader = reader(14);
        let error = request
            .resolve(|permit| load(&object_reader, (1, 0), permit))
            .unwrap_err();
        assert_eq!(error.kind, AccessKind::ResourceLimit);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 0);
        assert_eq!(snapshot.cache_bytes, 0);
        assert_eq!(snapshot.live_interests, 0);
        assert!(snapshot.invariant_failed);
    }

    #[test]
    fn ten_thousand_cell_churn_keeps_hot_key_and_exact_caps() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(64 * 1024));
        let arena = domain.open_arena().unwrap();
        let hot_reader = reader(55);
        let hot_loads = AtomicU64::new(0);
        let hot = arena
            .resolve((1, 0), |permit| {
                hot_loads.fetch_add(1, Ordering::Relaxed);
                load(&hot_reader, (1, 0), permit)
            })
            .unwrap();
        for number in 2..=10_001u32 {
            let id = (number, 0);
            let error = arena
                .resolve(id, |_| {
                    Err(CellLoadError::new(
                        AccessError::typed(id, AccessKind::Backend, "stable churn miss"),
                        NegativeDisposition::Persistent,
                    ))
                })
                .unwrap_err();
            assert_eq!(error.object, id);
            if number % 1_000 == 0 {
                let hit = arena
                    .resolve((1, 0), |_| panic!("pinned hot key must remain resident"))
                    .unwrap();
                assert_eq!(hit.pointer(), hot.pointer());
            }
        }
        assert_eq!(hot_loads.load(Ordering::Relaxed), 1);
        let snapshot = domain.snapshot();
        assert!(snapshot.cells <= 16_384);
        assert!(snapshot.cache_bytes <= 64 * 1024);
        assert!(snapshot.evictions > 9_900);
        assert_eq!(snapshot.external_pins, 1);
        assert!(!snapshot.invariant_failed);
    }

    struct CountingHooks {
        adds: AtomicU64,
        removes: AtomicU64,
        entered: Mutex<Option<mpsc::SyncSender<()>>>,
        seen: Mutex<Vec<(u64, u64)>>,
    }

    struct SignallingWaitHooks {
        adds: AtomicU64,
        removes: AtomicU64,
        entered: mpsc::Sender<()>,
    }

    struct BlockingReadSource {
        bytes: Arc<[u8]>,
        armed: AtomicBool,
        entered: Mutex<Option<mpsc::SyncSender<()>>>,
        resume: Mutex<mpsc::Receiver<()>>,
    }

    impl BlockingReadSource {
        fn new(bytes: Arc<[u8]>) -> (Arc<Self>, mpsc::Receiver<()>, mpsc::SyncSender<()>) {
            let (entered_tx, entered_rx) = mpsc::sync_channel(0);
            let (resume_tx, resume_rx) = mpsc::sync_channel(0);
            (
                Arc::new(Self {
                    bytes,
                    armed: AtomicBool::new(false),
                    entered: Mutex::new(Some(entered_tx)),
                    resume: Mutex::new(resume_rx),
                }),
                entered_rx,
                resume_tx,
            )
        }

        fn arm(&self) {
            self.armed.store(true, Ordering::Release);
        }
    }

    impl RandomAccessSource for BlockingReadSource {
        fn len(&self) -> SourceResult<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
            if self.armed.swap(false, Ordering::AcqRel) {
                if let Some(entered) = lock(&self.entered).take() {
                    entered.send(()).unwrap();
                }
                lock(&self.resume).recv().unwrap();
            }
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= self.bytes.len() {
                return Ok(0);
            }
            let count = out.len().min(self.bytes.len() - start);
            out[..count].copy_from_slice(&self.bytes[start..start + count]);
            Ok(count)
        }
    }

    struct PausingCloseHooks {
        entered: Barrier,
        release: Barrier,
    }

    #[derive(Clone, Copy)]
    enum PhaseAction {
        Continue,
        Panic,
    }

    struct PausingLeaderPhaseHooks {
        target: LeaderPhase,
        armed: AtomicBool,
        entered: mpsc::SyncSender<()>,
        action: Mutex<mpsc::Receiver<PhaseAction>>,
    }

    struct PausingDependencyHooks {
        target: DependencyEventKind,
        target_generation: Option<u64>,
        armed: AtomicBool,
        entered: mpsc::SyncSender<DependencyEvent>,
        action: Mutex<mpsc::Receiver<PhaseAction>>,
        events: Mutex<Vec<DependencyEvent>>,
        lock_probes: AtomicU64,
    }

    struct RecordingDependencyHooks {
        events: Mutex<Vec<DependencyEvent>>,
    }

    struct RecordingFailureDeliveryHooks {
        deliveries: Mutex<Vec<(CellKey, usize)>>,
    }

    struct MultiPausingDependencyHooks {
        target: DependencyEventKind,
        entered: mpsc::Sender<DependencyEvent>,
        action: Mutex<mpsc::Receiver<()>>,
        events: Mutex<Vec<DependencyEvent>>,
    }

    impl DependencyHooks for MultiPausingDependencyHooks {
        fn event(&self, event: DependencyEvent) {
            assert_eq!(caller_thread_lock_depth(), 0);
            lock(&self.events).push(event);
            if event.kind == self.target {
                self.entered.send(event).unwrap();
                lock(&self.action).recv().unwrap();
            }
        }
    }

    struct PausingScheduledResourceHooks {
        target: ScheduledResourceShape,
        entered: mpsc::SyncSender<(CellKey, DependencyLeaderToken, ScheduledResourceShape)>,
        action: Mutex<mpsc::Receiver<()>>,
        dropped: mpsc::SyncSender<(CellKey, DependencyLeaderToken, ScheduledResourceShape)>,
        drop_acknowledgements: AtomicU64,
    }

    struct PerTokenChainHooks {
        events: Mutex<Vec<DependencyEvent>>,
        child_entered: mpsc::Sender<DependencyEvent>,
        request_entered: mpsc::Sender<DependencyEvent>,
        grant_entered: mpsc::Sender<DependencyEvent>,
        child_actions: BTreeMap<ObjectId, Mutex<mpsc::Receiver<()>>>,
        request_actions: BTreeMap<ObjectId, Mutex<mpsc::Receiver<()>>>,
        grant_actions: BTreeMap<ObjectId, Mutex<mpsc::Receiver<()>>>,
    }

    impl DependencyHooks for PerTokenChainHooks {
        fn event(&self, event: DependencyEvent) {
            assert_eq!(caller_thread_lock_depth(), 0);
            lock(&self.events).push(event);
            let actions = match event.kind {
                DependencyEventKind::ChildResolved => {
                    self.child_entered.send(event).unwrap();
                    Some(&self.child_actions)
                }
                DependencyEventKind::MemberRequestCreated => {
                    self.request_entered.send(event).unwrap();
                    Some(&self.request_actions)
                }
                DependencyEventKind::MemberGranted => {
                    self.grant_entered.send(event).unwrap();
                    Some(&self.grant_actions)
                }
                _ => None,
            };
            if let Some(actions) = actions {
                lock(actions.get(&event.token.key.id).unwrap())
                    .recv()
                    .unwrap();
            }
        }
    }

    impl ScheduledResourceHooks for PausingScheduledResourceHooks {
        fn enter(&self, key: CellKey, token: DependencyLeaderToken, shape: ScheduledResourceShape) {
            assert_eq!(caller_thread_lock_depth(), 0);
            if shape == self.target {
                self.entered.send((key, token, shape)).unwrap();
                lock(&self.action).recv().unwrap();
            }
        }

        fn dropped(
            &self,
            key: CellKey,
            token: DependencyLeaderToken,
            shape: ScheduledResourceShape,
        ) {
            assert_eq!(caller_thread_lock_depth(), 0);
            if shape == self.target {
                let previous = self.drop_acknowledgements.fetch_add(1, Ordering::Relaxed);
                if previous == 0 {
                    self.dropped.send((key, token, shape)).unwrap();
                }
            }
        }
    }

    impl FailureDeliveryHooks for RecordingFailureDeliveryHooks {
        fn delivered(&self, key: CellKey, owner: usize) {
            assert_eq!(caller_thread_lock_depth(), 0);
            lock(&self.deliveries).push((key, owner));
        }
    }

    impl DependencyHooks for RecordingDependencyHooks {
        fn event(&self, event: DependencyEvent) {
            assert_eq!(caller_thread_lock_depth(), 0);
            lock(&self.events).push(event);
        }
    }

    struct SignallingDependencyHooks {
        target: DependencyEventKind,
        first_paused: AtomicBool,
        entered: mpsc::Sender<DependencyEvent>,
        first_action: Mutex<mpsc::Receiver<()>>,
        events: Mutex<Vec<DependencyEvent>>,
    }

    impl DependencyHooks for SignallingDependencyHooks {
        fn event(&self, event: DependencyEvent) {
            assert_eq!(caller_thread_lock_depth(), 0);
            lock(&self.events).push(event);
            if event.kind != self.target {
                return;
            }
            self.entered.send(event).unwrap();
            if self.first_paused.swap(false, Ordering::AcqRel) {
                lock(&self.first_action).recv().unwrap();
            }
        }
    }

    struct PanickingDependencyHooks;

    impl DependencyHooks for PanickingDependencyHooks {
        fn event(&self, event: DependencyEvent) {
            panic!("ready hit emitted dependency event: {event:?}")
        }
    }

    struct PausingDependencyWaitHooks {
        armed: AtomicBool,
        entered: mpsc::SyncSender<(CellKey, DependencyTransition)>,
        action: Mutex<mpsc::Receiver<()>>,
    }

    struct SignallingDependencyWaitHooks {
        entered: mpsc::Sender<(CellKey, DependencyTransition)>,
    }

    impl DependencyWaitHooks for SignallingDependencyWaitHooks {
        fn waiting(&self, key: CellKey, transition: DependencyTransition) {
            assert_eq!(caller_thread_lock_depth(), 0);
            self.entered.send((key, transition)).unwrap();
        }
    }

    struct PausingDependencyCleanupFailureHooks {
        armed: AtomicBool,
        entered: mpsc::SyncSender<(CellKey, DependencyLeaderToken)>,
        action: Mutex<mpsc::Receiver<()>>,
    }

    enum R9FollowerOutcome {
        Waited(DependencyTransition),
        Returned(Result<(), AccessError>),
    }

    struct ReportingDependencyWaitHooks {
        outcome: mpsc::Sender<R9FollowerOutcome>,
    }

    impl DependencyWaitHooks for ReportingDependencyWaitHooks {
        fn waiting(&self, _key: CellKey, transition: DependencyTransition) {
            assert_eq!(caller_thread_lock_depth(), 0);
            self.outcome
                .send(R9FollowerOutcome::Waited(transition))
                .unwrap();
        }
    }

    struct SignallingDependencyTerminalCleanupHooks {
        entered: mpsc::SyncSender<(CellKey, DependencyTerminalCleanupOwner)>,
    }

    struct PausingDependencyTerminalCleanupHooks {
        entered: mpsc::SyncSender<(CellKey, DependencyTerminalCleanupOwner)>,
        action: Mutex<mpsc::Receiver<()>>,
    }

    impl DependencyTerminalCleanupHooks for SignallingDependencyTerminalCleanupHooks {
        fn before_metadata(&self, key: CellKey, owner: DependencyTerminalCleanupOwner) {
            assert_eq!(caller_thread_lock_depth(), 0);
            self.entered.send((key, owner)).unwrap();
        }
    }

    impl DependencyTerminalCleanupHooks for PausingDependencyTerminalCleanupHooks {
        fn before_metadata(&self, key: CellKey, owner: DependencyTerminalCleanupOwner) {
            assert_eq!(caller_thread_lock_depth(), 0);
            self.entered.send((key, owner)).unwrap();
            lock(&self.action).recv().unwrap();
        }
    }

    struct PausingDependencyPreWaitHooks {
        armed: AtomicBool,
        entered: mpsc::SyncSender<(CellKey, DependencyTransition)>,
        action: Mutex<mpsc::Receiver<()>>,
    }

    impl DependencyPreWaitHooks for PausingDependencyPreWaitHooks {
        fn before_wait(&self, key: CellKey, transition: DependencyTransition) {
            assert!(caller_thread_lock_depth() > 0);
            if !self.armed.swap(false, Ordering::AcqRel) {
                return;
            }
            self.entered.send((key, transition)).unwrap();
            lock(&self.action).recv().unwrap();
        }
    }

    impl DependencyCleanupFailureHooks for PausingDependencyCleanupFailureHooks {
        fn failed(&self, key: CellKey, token: DependencyLeaderToken) {
            assert_eq!(caller_thread_lock_depth(), 0);
            if !self.armed.swap(false, Ordering::AcqRel) {
                return;
            }
            self.entered.send((key, token)).unwrap();
            lock(&self.action).recv().unwrap();
        }
    }

    impl DependencyWaitHooks for PausingDependencyWaitHooks {
        fn waiting(&self, key: CellKey, transition: DependencyTransition) {
            assert_eq!(caller_thread_lock_depth(), 0);
            if !self.armed.swap(false, Ordering::AcqRel) {
                return;
            }
            self.entered.send((key, transition)).unwrap();
            lock(&self.action).recv().unwrap();
        }
    }

    impl DependencyHooks for PausingDependencyHooks {
        fn event(&self, event: DependencyEvent) {
            assert_eq!(
                caller_thread_lock_depth(),
                0,
                "dependency hook caller retained an object-cell lock: {event:?}"
            );
            self.lock_probes.fetch_add(1, Ordering::Relaxed);
            lock(&self.events).push(event);
            if event.kind != self.target
                || self
                    .target_generation
                    .is_some_and(|generation| event.token.generation != generation)
                || !self.armed.swap(false, Ordering::AcqRel)
            {
                return;
            }
            self.entered.send(event).unwrap();
            match lock(&self.action).recv().unwrap() {
                PhaseAction::Continue => {}
                PhaseAction::Panic => panic!("injected dependency-phase panic at {:?}", event.kind),
            }
        }
    }

    fn dependency_hook(
        domain: &ObjectCellDomain,
        target: DependencyEventKind,
    ) -> (
        Arc<PausingDependencyHooks>,
        mpsc::Receiver<DependencyEvent>,
        mpsc::SyncSender<PhaseAction>,
    ) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (action_tx, action_rx) = mpsc::sync_channel(0);
        let hooks = Arc::new(PausingDependencyHooks {
            target,
            target_generation: None,
            armed: AtomicBool::new(true),
            entered: entered_tx,
            action: Mutex::new(action_rx),
            events: Mutex::new(Vec::new()),
            lock_probes: AtomicU64::new(0),
        });
        domain.set_dependency_hooks(hooks.clone());
        (hooks, entered_rx, action_tx)
    }

    fn scheduled_resource_hook(
        domain: &ObjectCellDomain,
        target: ScheduledResourceShape,
    ) -> (
        mpsc::Receiver<(CellKey, DependencyLeaderToken, ScheduledResourceShape)>,
        mpsc::SyncSender<()>,
        mpsc::Receiver<(CellKey, DependencyLeaderToken, ScheduledResourceShape)>,
        Arc<PausingScheduledResourceHooks>,
    ) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (action_tx, action_rx) = mpsc::sync_channel(0);
        let (dropped_tx, dropped_rx) = mpsc::sync_channel(0);
        let hooks = Arc::new(PausingScheduledResourceHooks {
            target,
            entered: entered_tx,
            action: Mutex::new(action_rx),
            dropped: dropped_tx,
            drop_acknowledgements: AtomicU64::new(0),
        });
        domain.set_scheduled_resource_hooks(hooks.clone());
        (entered_rx, action_tx, dropped_rx, hooks)
    }

    fn multi_dependency_hook(
        domain: &ObjectCellDomain,
        target: DependencyEventKind,
    ) -> (
        Arc<MultiPausingDependencyHooks>,
        mpsc::Receiver<DependencyEvent>,
        mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let hooks = Arc::new(MultiPausingDependencyHooks {
            target,
            entered: entered_tx,
            action: Mutex::new(action_rx),
            events: Mutex::new(Vec::new()),
        });
        domain.set_dependency_hooks(hooks.clone());
        (hooks, entered_rx, action_tx)
    }

    fn dependency_hook_for_generation(
        domain: &ObjectCellDomain,
        target: DependencyEventKind,
        generation: u64,
    ) -> (
        Arc<PausingDependencyHooks>,
        mpsc::Receiver<DependencyEvent>,
        mpsc::SyncSender<PhaseAction>,
    ) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (action_tx, action_rx) = mpsc::sync_channel(0);
        let hooks = Arc::new(PausingDependencyHooks {
            target,
            target_generation: Some(generation),
            armed: AtomicBool::new(true),
            entered: entered_tx,
            action: Mutex::new(action_rx),
            events: Mutex::new(Vec::new()),
            lock_probes: AtomicU64::new(0),
        });
        domain.set_dependency_hooks(hooks.clone());
        (hooks, entered_rx, action_tx)
    }

    fn dependency_wait_hook(
        domain: &ObjectCellDomain,
    ) -> (
        mpsc::Receiver<(CellKey, DependencyTransition)>,
        mpsc::SyncSender<()>,
    ) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (action_tx, action_rx) = mpsc::sync_channel(0);
        domain.set_dependency_wait_hooks(Arc::new(PausingDependencyWaitHooks {
            armed: AtomicBool::new(true),
            entered: entered_tx,
            action: Mutex::new(action_rx),
        }));
        (entered_rx, action_tx)
    }

    fn dependency_cleanup_failure_hook(
        domain: &ObjectCellDomain,
    ) -> (
        mpsc::Receiver<(CellKey, DependencyLeaderToken)>,
        mpsc::SyncSender<()>,
    ) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (action_tx, action_rx) = mpsc::sync_channel(0);
        domain.set_dependency_cleanup_failure_hooks(Arc::new(
            PausingDependencyCleanupFailureHooks {
                armed: AtomicBool::new(true),
                entered: entered_tx,
                action: Mutex::new(action_rx),
            },
        ));
        (entered_rx, action_tx)
    }

    fn dependency_terminal_cleanup_hook(
        domain: &ObjectCellDomain,
    ) -> mpsc::Receiver<(CellKey, DependencyTerminalCleanupOwner)> {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        domain.set_dependency_terminal_cleanup_hooks(Arc::new(
            SignallingDependencyTerminalCleanupHooks {
                entered: entered_tx,
            },
        ));
        entered_rx
    }

    fn pausing_dependency_terminal_cleanup_hook(
        domain: &ObjectCellDomain,
    ) -> (
        mpsc::Receiver<(CellKey, DependencyTerminalCleanupOwner)>,
        mpsc::SyncSender<()>,
    ) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (action_tx, action_rx) = mpsc::sync_channel(0);
        domain.set_dependency_terminal_cleanup_hooks(Arc::new(
            PausingDependencyTerminalCleanupHooks {
                entered: entered_tx,
                action: Mutex::new(action_rx),
            },
        ));
        (entered_rx, action_tx)
    }

    fn dependency_pre_wait_hook(
        domain: &ObjectCellDomain,
    ) -> (
        mpsc::Receiver<(CellKey, DependencyTransition)>,
        mpsc::SyncSender<()>,
    ) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (action_tx, action_rx) = mpsc::sync_channel(0);
        domain.set_dependency_pre_wait_hooks(Arc::new(PausingDependencyPreWaitHooks {
            armed: AtomicBool::new(true),
            entered: entered_tx,
            action: Mutex::new(action_rx),
        }));
        (entered_rx, action_tx)
    }

    fn prepared_dependency_fixture() -> (
        BudgetBroker,
        ObjectCellDomain,
        ObjectCellArena,
        ObjectId,
        ObjectId,
    ) {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let (reader, member, container, _) = object_stream_reader();
        let prepared = arena
            .resolve_object_stream(container, |permit| {
                reader
                    .prepare_object_stream_with_permit(container, permit)
                    .map_err(|error| {
                        CellLoadError::new(
                            AccessError::typed(container, AccessKind::Backend, error),
                            NegativeDisposition::FlightOnly,
                        )
                    })
            })
            .unwrap();
        drop(prepared);
        (broker, domain, arena, member, container)
    }

    #[derive(Debug, Default, Eq, PartialEq)]
    struct OperationSnapshotDelta {
        normal_retained_cap: i128,
        queued: i128,
        in_flight: i128,
        error_owners: i128,
        grants: i128,
        granted_bytes: i128,
        cache_bytes: i128,
        pin_bytes: i128,
        bypass_bytes: i128,
        self_pinned_bytes: i128,
    }

    fn operation_snapshot_delta(
        before: &crate::broker::OperationSnapshot,
        after: &crate::broker::OperationSnapshot,
    ) -> OperationSnapshotDelta {
        OperationSnapshotDelta {
            normal_retained_cap: i128::from(after.normal_retained_cap)
                - i128::from(before.normal_retained_cap),
            queued: after.queued as i128 - before.queued as i128,
            in_flight: after.in_flight as i128 - before.in_flight as i128,
            error_owners: after.error_owners as i128 - before.error_owners as i128,
            grants: i128::from(after.grants) - i128::from(before.grants),
            granted_bytes: i128::from(after.granted_bytes) - i128::from(before.granted_bytes),
            cache_bytes: i128::from(after.cache_bytes) - i128::from(before.cache_bytes),
            pin_bytes: i128::from(after.pin_bytes) - i128::from(before.pin_bytes),
            bypass_bytes: i128::from(after.bypass_bytes) - i128::from(before.bypass_bytes),
            self_pinned_bytes: i128::from(after.self_pinned_bytes)
                - i128::from(before.self_pinned_bytes),
        }
    }

    #[derive(Debug, Default, Eq, PartialEq)]
    struct BrokerSnapshotDelta {
        normal_limit_bytes: i128,
        normal_payload_bytes: i128,
        normal_in_flight_estimate_bytes: i128,
        metadata_bytes: i128,
        completion_reserve_bytes: i128,
        oversize_bytes: i128,
        aggregate_bytes: i128,
        peak_normal_bytes: i128,
        peak_completion_reserve_bytes: i128,
        peak_oversize_bytes: i128,
        peak_aggregate_bytes: i128,
        queued: i128,
        in_flight: i128,
        live_request_records: i128,
        error_metadata_bytes: i128,
        reservation_metadata_bytes: i128,
        active_operations: i128,
        grants: i128,
        denials: i128,
        cancellations: i128,
        reconciliations: i128,
        maximum_admissible_lag: i128,
        oversize_owners: i128,
        cache_bytes: i128,
        pin_bytes: i128,
        bypass_bytes: i128,
        peak_cache_bytes: i128,
        peak_pin_bytes: i128,
        peak_bypass_bytes: i128,
        operations: BTreeMap<u64, OperationSnapshotDelta>,
        invariant_failed_changed: bool,
        closed_changed: bool,
    }

    fn broker_snapshot_delta(
        before: &crate::broker::BrokerSnapshot,
        after: &crate::broker::BrokerSnapshot,
    ) -> BrokerSnapshotDelta {
        assert_eq!(
            before.operations.keys().collect::<Vec<_>>(),
            after.operations.keys().collect::<Vec<_>>(),
            "operation registry changed while computing scoped broker delta"
        );
        let operations = before
            .operations
            .iter()
            .map(|(epoch, before_operation)| {
                (
                    *epoch,
                    operation_snapshot_delta(before_operation, &after.operations[epoch]),
                )
            })
            .collect();
        BrokerSnapshotDelta {
            normal_limit_bytes: i128::from(after.normal_limit_bytes)
                - i128::from(before.normal_limit_bytes),
            normal_payload_bytes: i128::from(after.normal_payload_bytes)
                - i128::from(before.normal_payload_bytes),
            normal_in_flight_estimate_bytes: i128::from(after.normal_in_flight_estimate_bytes)
                - i128::from(before.normal_in_flight_estimate_bytes),
            metadata_bytes: i128::from(after.metadata_bytes) - i128::from(before.metadata_bytes),
            completion_reserve_bytes: i128::from(after.completion_reserve_bytes)
                - i128::from(before.completion_reserve_bytes),
            oversize_bytes: i128::from(after.oversize_bytes) - i128::from(before.oversize_bytes),
            aggregate_bytes: i128::from(after.aggregate_bytes) - i128::from(before.aggregate_bytes),
            peak_normal_bytes: i128::from(after.peak_normal_bytes)
                - i128::from(before.peak_normal_bytes),
            peak_completion_reserve_bytes: i128::from(after.peak_completion_reserve_bytes)
                - i128::from(before.peak_completion_reserve_bytes),
            peak_oversize_bytes: i128::from(after.peak_oversize_bytes)
                - i128::from(before.peak_oversize_bytes),
            peak_aggregate_bytes: i128::from(after.peak_aggregate_bytes)
                - i128::from(before.peak_aggregate_bytes),
            queued: after.queued as i128 - before.queued as i128,
            in_flight: after.in_flight as i128 - before.in_flight as i128,
            live_request_records: after.live_request_records as i128
                - before.live_request_records as i128,
            error_metadata_bytes: i128::from(after.error_metadata_bytes)
                - i128::from(before.error_metadata_bytes),
            reservation_metadata_bytes: i128::from(after.reservation_metadata_bytes)
                - i128::from(before.reservation_metadata_bytes),
            active_operations: after.active_operations as i128 - before.active_operations as i128,
            grants: i128::from(after.grants) - i128::from(before.grants),
            denials: i128::from(after.denials) - i128::from(before.denials),
            cancellations: i128::from(after.cancellations) - i128::from(before.cancellations),
            reconciliations: i128::from(after.reconciliations) - i128::from(before.reconciliations),
            maximum_admissible_lag: i128::from(after.maximum_admissible_lag)
                - i128::from(before.maximum_admissible_lag),
            oversize_owners: i128::from(after.oversize_owners) - i128::from(before.oversize_owners),
            cache_bytes: i128::from(after.cache_bytes) - i128::from(before.cache_bytes),
            pin_bytes: i128::from(after.pin_bytes) - i128::from(before.pin_bytes),
            bypass_bytes: i128::from(after.bypass_bytes) - i128::from(before.bypass_bytes),
            peak_cache_bytes: i128::from(after.peak_cache_bytes)
                - i128::from(before.peak_cache_bytes),
            peak_pin_bytes: i128::from(after.peak_pin_bytes) - i128::from(before.peak_pin_bytes),
            peak_bypass_bytes: i128::from(after.peak_bypass_bytes)
                - i128::from(before.peak_bypass_bytes),
            operations,
            invariant_failed_changed: before.invariant_failed != after.invariant_failed,
            closed_changed: before.closed != after.closed,
        }
    }

    fn assert_broker_snapshot_unchanged(
        before: &crate::broker::BrokerSnapshot,
        after: &crate::broker::BrokerSnapshot,
        context: &str,
    ) {
        let mut delta = broker_snapshot_delta(before, after);
        for (epoch, operation) in &delta.operations {
            assert_eq!(
                operation,
                &OperationSnapshotDelta::default(),
                "{context}: operation {epoch} changed"
            );
        }
        delta.operations.clear();
        assert_eq!(delta, BrokerSnapshotDelta::default(), "{context}");
    }

    fn zero_operation_deltas(
        snapshot: &crate::broker::BrokerSnapshot,
    ) -> BTreeMap<u64, OperationSnapshotDelta> {
        snapshot
            .operations
            .keys()
            .map(|epoch| (*epoch, OperationSnapshotDelta::default()))
            .collect()
    }

    fn expected_member_request_delta(
        before: &crate::broker::BrokerSnapshot,
        epoch: u64,
        queued: bool,
    ) -> BrokerSnapshotDelta {
        let estimate = Representation::DeclaredObjStmMember.loader_estimate();
        let metadata = crate::broker::QUEUE_METADATA_WEIGHT;
        let payload = if queued { 0 } else { estimate };
        let added = metadata + payload;
        let current_normal = before.normal_payload_bytes + before.metadata_bytes;
        let mut operations = zero_operation_deltas(before);
        let operation = operations.get_mut(&epoch).unwrap();
        operation.queued = i128::from(queued);
        operation.in_flight = i128::from(!queued);
        operation.grants = i128::from(!queued);
        operation.granted_bytes = i128::from(payload);
        BrokerSnapshotDelta {
            normal_payload_bytes: i128::from(payload),
            normal_in_flight_estimate_bytes: i128::from(payload),
            metadata_bytes: i128::from(metadata),
            aggregate_bytes: i128::from(added),
            peak_normal_bytes: i128::from(
                (current_normal + added).saturating_sub(before.peak_normal_bytes),
            ),
            peak_aggregate_bytes: i128::from(
                (before.aggregate_bytes + added).saturating_sub(before.peak_aggregate_bytes),
            ),
            queued: i128::from(queued),
            in_flight: i128::from(!queued),
            live_request_records: 1,
            reservation_metadata_bytes: i128::from(!queued) * i128::from(metadata),
            grants: i128::from(!queued),
            operations,
            ..BrokerSnapshotDelta::default()
        }
    }

    fn expected_bounded_flight_refusal_delta(
        before: &crate::broker::BrokerSnapshot,
        epoch: u64,
        denials: u64,
        cell_retained_as_bypass: bool,
    ) -> BrokerSnapshotDelta {
        let pin = before.operations[&epoch].pin_bytes;
        let cache_delta = i128::from(pin) - i128::from(CELL_METADATA_BYTES);
        let dropped_cell = if cell_retained_as_bypass {
            0
        } else {
            CELL_METADATA_BYTES
        };
        let bypass = if cell_retained_as_bypass {
            CELL_METADATA_BYTES
        } else {
            0
        };
        let mut operations = zero_operation_deltas(before);
        let operation = operations.get_mut(&epoch).unwrap();
        operation.cache_bytes = cache_delta;
        operation.pin_bytes = -i128::from(pin);
        operation.bypass_bytes = i128::from(bypass);
        operation.self_pinned_bytes = -i128::from(pin);
        let next_cache = (i128::from(before.cache_bytes) + cache_delta) as u64;
        let next_bypass = before.bypass_bytes + bypass;
        BrokerSnapshotDelta {
            normal_payload_bytes: -i128::from(dropped_cell),
            aggregate_bytes: -i128::from(dropped_cell),
            denials: i128::from(denials),
            cache_bytes: cache_delta,
            pin_bytes: -i128::from(pin),
            bypass_bytes: i128::from(bypass),
            peak_cache_bytes: i128::from(next_cache.saturating_sub(before.peak_cache_bytes)),
            peak_bypass_bytes: i128::from(next_bypass.saturating_sub(before.peak_bypass_bytes)),
            operations,
            ..BrokerSnapshotDelta::default()
        }
    }

    fn expected_member_completion_delta(
        before: &crate::broker::BrokerSnapshot,
        epoch: u64,
        outcome: Result<u64, u64>,
        peak_bypass_delta: u64,
    ) -> BrokerSnapshotDelta {
        let estimate = Representation::DeclaredObjStmMember.loader_estimate();
        let metadata = crate::broker::QUEUE_METADATA_WEIGHT;
        let (retained, dropped_cell) = match outcome {
            Ok(retained) => (retained, 0),
            Err(_) => (0, CELL_METADATA_BYTES),
        };
        let pin = before.operations[&epoch].pin_bytes;
        let cache_delta = i128::from(pin + retained) - i128::from(dropped_cell);
        let normal_delta = i128::from(retained) - i128::from(estimate) - i128::from(dropped_cell);
        let mut operations = zero_operation_deltas(before);
        let operation = operations.get_mut(&epoch).unwrap();
        operation.in_flight = -1;
        operation.cache_bytes = cache_delta;
        operation.pin_bytes = -i128::from(pin);
        operation.self_pinned_bytes = -i128::from(pin);
        let next_cache = (i128::from(before.cache_bytes) + cache_delta) as u64;
        BrokerSnapshotDelta {
            normal_payload_bytes: normal_delta,
            normal_in_flight_estimate_bytes: -i128::from(estimate),
            metadata_bytes: -i128::from(metadata),
            aggregate_bytes: normal_delta - i128::from(metadata),
            in_flight: -1,
            live_request_records: -1,
            reservation_metadata_bytes: -i128::from(metadata),
            reconciliations: 1,
            cache_bytes: cache_delta,
            pin_bytes: -i128::from(pin),
            peak_cache_bytes: i128::from(next_cache.saturating_sub(before.peak_cache_bytes)),
            peak_pin_bytes: i128::from(retained.saturating_sub(before.peak_pin_bytes)),
            peak_bypass_bytes: i128::from(peak_bypass_delta),
            operations,
            ..BrokerSnapshotDelta::default()
        }
    }

    #[test]
    fn c2_budget_constants_are_frozen_together() {
        assert_eq!(
            Representation::DeclaredObjStmMember.loader_estimate(),
            4_194_304,
            "M: scheduled member estimate"
        );
        assert_eq!(
            crate::broker::QUEUE_METADATA_WEIGHT,
            256,
            "Q: queue record metadata"
        );
        assert_eq!(
            crate::broker::OPERATION_METADATA_WEIGHT,
            2_048,
            "E: operation metadata"
        );
        assert_eq!(CELL_METADATA_BYTES, 5_120, "C: cell metadata envelope");
        assert_eq!(
            CELL_ERROR_ENVELOPE_BYTES, 512,
            "F: retained flight-error envelope"
        );
    }

    struct C2QueuedFixture {
        broker: BudgetBroker,
        blocker_operation: BrokerOperation,
        domain: ObjectCellDomain,
        arena: ObjectCellArena,
        member: ObjectId,
        container: ObjectId,
        sibling_pointer: usize,
        blocker: crate::broker::Reservation,
        cell: Arc<Cell>,
        token: DependencyLeaderToken,
        queued_action: mpsc::SyncSender<PhaseAction>,
        leader_cancel: CellCancellation,
        leader_join: Option<thread::JoinHandle<Result<ResolvedObjectPin, AccessError>>>,
        followers: Vec<ObjectCellRequest>,
        parses: Arc<AtomicU64>,
        queued_snapshot: crate::broker::BrokerSnapshot,
    }

    fn c2_queued_fixture(follower_count: usize) -> C2QueuedFixture {
        let broker = BudgetBroker::new(crate::broker::BrokerConfig {
            normal_limit: 128 * 1024 * 1024,
            oversize_limit: 64 * 1024 * 1024,
            completion_reserve_limit: 0,
            queue_metadata_weight: crate::broker::QUEUE_METADATA_WEIGHT,
            operation_metadata_weight: crate::broker::OPERATION_METADATA_WEIGHT,
            max_active_operations: 128,
            max_queued_requests: 128,
        })
        .unwrap();
        let blocker_operation = broker.register_operation().unwrap();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let (stream_reader, member, container, _) = object_stream_reader();
        let sibling_reader = reader(1_515);
        let sibling = arena
            .resolve((1, 0), |permit| load(&sibling_reader, (1, 0), permit))
            .unwrap();
        let sibling_pointer = sibling.pointer();
        drop(sibling);
        let (_pin_hooks, pin_entered, pin_action) =
            dependency_hook(&domain, DependencyEventKind::BeforeMemberAdmission);
        let leader = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let cell = Arc::clone(&leader.cell);
        let leader_cancel = leader.cancellation_handle();
        let followers = (0..follower_count)
            .map(|_| {
                arena
                    .inner
                    .request_representation(member, Representation::DeclaredObjStmMember)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let parses = Arc::new(AtomicU64::new(0));
        let parses_for_load = Arc::clone(&parses);
        let child_reader = Arc::clone(&stream_reader);
        let member_reader = Arc::clone(&stream_reader);
        let leader_join = thread::spawn(move || {
            leader.resolve_scheduled_member_synthetic(
                container,
                move |permit| {
                    child_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                },
                move |permit| {
                    parses_for_load.fetch_add(1, Ordering::Relaxed);
                    load(&member_reader, member, permit)
                },
            )
        });
        pin_entered.recv().unwrap();

        let before_probe = broker.snapshot();
        let probe = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1,
            )
            .unwrap();
        let probe_snapshot = broker.snapshot();
        let reservation_metadata = probe_snapshot.metadata_bytes - before_probe.metadata_bytes;
        drop(probe);
        let baseline = broker.snapshot();
        let estimate = Representation::DeclaredObjStmMember.loader_estimate();
        let self_pinned = baseline.operations[&arena.epoch()].self_pinned_bytes;
        let blocker_bytes = baseline
            .normal_limit_bytes
            .checked_add(1 + self_pinned)
            .and_then(|bytes| bytes.checked_sub(baseline.normal_payload_bytes))
            .and_then(|bytes| bytes.checked_sub(baseline.metadata_bytes))
            .and_then(|bytes| bytes.checked_sub(reservation_metadata))
            .and_then(|bytes| bytes.checked_sub(crate::broker::QUEUE_METADATA_WEIGHT))
            .and_then(|bytes| bytes.checked_sub(estimate))
            .unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                blocker_bytes,
            )
            .unwrap();
        let (_queued_hooks, queued_entered, queued_action) =
            dependency_hook(&domain, DependencyEventKind::MemberQueued);
        pin_action.send(PhaseAction::Continue).unwrap();
        let queued = queued_entered.recv().unwrap();
        let queued_snapshot = broker.snapshot();
        let member_operation = &queued_snapshot.operations[&arena.epoch()];
        assert_eq!(queued.kind, DependencyEventKind::MemberQueued);
        assert_eq!(queued.token.key, cell.key);
        assert_eq!(member_operation.queued, 1);
        assert_eq!(member_operation.in_flight, 0);
        assert_eq!(parses.load(Ordering::Relaxed), 0);
        assert_eq!(queued_snapshot.oversize_bytes, 0);
        assert_eq!(queued_snapshot.completion_reserve_bytes, 0);
        C2QueuedFixture {
            broker,
            blocker_operation,
            domain,
            arena,
            member,
            container,
            sibling_pointer,
            blocker,
            cell,
            token: queued.token,
            queued_action,
            leader_cancel,
            leader_join: Some(leader_join),
            followers,
            parses,
            queued_snapshot,
        }
    }

    impl C2QueuedFixture {
        fn join_leader(&mut self) -> thread::Result<Result<ResolvedObjectPin, AccessError>> {
            self.leader_join.take().unwrap().join()
        }

        fn assert_member_drained_without_grant(&self) {
            assert_eq!(self.parses.load(Ordering::Relaxed), 0);
            let cells = self.domain.snapshot();
            assert_eq!(cells.dependency_pins, 0);
            assert_eq!(cells.dependency_edges, 0);
            let broker = self.broker.snapshot();
            assert_eq!(broker.grants, self.queued_snapshot.grants);
            assert_eq!(broker.operations[&self.arena.epoch()].queued, 0);
            assert_eq!(broker.operations[&self.arena.epoch()].in_flight, 0);
        }

        fn finish_open(self, context: &str) {
            assert!(self.leader_join.is_none());
            self.assert_member_drained_without_grant();
            drop(self.blocker);
            self.blocker_operation.close();
            let sibling = self
                .arena
                .resolve((1, 0), |_| panic!("{context}: unrelated sibling reloaded"))
                .unwrap();
            assert_eq!(sibling.pointer(), self.sibling_pointer, "{context}");
            drop(sibling);
            assert!(self.arena.has_container_cell(self.container), "{context}");
            drop(self.cell);
            self.arena.close();
            drop(self.arena);
            assert_eq!(self.broker.snapshot().aggregate_bytes, 0, "{context}");
        }

        fn finish_closed(self, context: &str) {
            assert!(self.leader_join.is_none());
            assert_eq!(self.parses.load(Ordering::Relaxed), 0, "{context}");
            assert_eq!(self.broker.snapshot().grants, self.queued_snapshot.grants);
            drop(self.blocker);
            self.blocker_operation.close();
            drop(self.cell);
            drop(self.arena);
            assert_eq!(self.broker.snapshot().aggregate_bytes, 0, "{context}");
        }
    }

    #[test]
    fn c2_queued_follower_cancel_preserves_blocked_leader_and_exact_broker_state() {
        let mut fixture = c2_queued_fixture(1);
        let follower = fixture.followers.pop().unwrap();
        let follower_cancel = follower.cancellation_handle();
        let container = fixture.container;
        let follower_join = thread::spawn(move || {
            follower.resolve_scheduled_member_synthetic(
                container,
                |_| panic!("queued follower cannot resolve child"),
                |_| panic!("queued follower cannot parse"),
            )
        });
        follower_cancel.cancel();
        assert!(follower_join.join().unwrap().is_err());
        assert_broker_snapshot_unchanged(
            &fixture.queued_snapshot,
            &fixture.broker.snapshot(),
            "queued follower cancellation",
        );
        assert_eq!(fixture.parses.load(Ordering::Relaxed), 0);
        let state = lock(&fixture.cell.state);
        let CellPhase::Loading(loading) = &state.phase else {
            panic!("queued follower cancellation replaced Loading")
        };
        assert_eq!(loading.generation, fixture.token.generation);
        assert_eq!(loading.leader_slot, fixture.token.leader_slot);
        assert_eq!(
            loading.dependency.as_ref().unwrap().token.key.id,
            fixture.member
        );
        assert!(loading.broker_cancellation.is_some());
        assert!(loading.permit.is_none());
        drop(state);

        let (wait_entered, wait_action) = dependency_wait_hook(&fixture.domain);
        let cancel = fixture.leader_cancel.clone();
        let cancel_join = thread::spawn(move || cancel.cancel());
        let (waited_key, transition) = wait_entered.recv().unwrap();
        assert_eq!(waited_key, fixture.cell.key);
        assert_eq!(
            transition,
            DependencyTransition::Announcing {
                token: fixture.token,
                kind: DependencyEventKind::MemberQueued,
            }
        );
        fixture.queued_action.send(PhaseAction::Continue).unwrap();
        wait_action.send(()).unwrap();
        cancel_join.join().unwrap();
        assert!(fixture.join_leader().unwrap().is_err());
        assert_eq!(fixture.parses.load(Ordering::Relaxed), 0);
        let terminal = fixture.domain.snapshot();
        assert_eq!(terminal.dependency_pins, 0);
        assert_eq!(terminal.dependency_edges, 0);
        assert_eq!(terminal.dependency_cleanup_acks, 1);
        let blocked = fixture.broker.snapshot();
        assert_eq!(blocked.grants, fixture.queued_snapshot.grants);
        assert_eq!(blocked.operations[&fixture.arena.epoch()].queued, 0);
        assert_eq!(blocked.operations[&fixture.arena.epoch()].in_flight, 0);
        fixture.finish_open("queued follower cancellation");
    }

    #[test]
    fn c2_queued_hook_panic_drops_request_pin_and_cancellation_without_granting() {
        let mut fixture = c2_queued_fixture(0);
        fixture.queued_action.send(PhaseAction::Panic).unwrap();
        assert!(fixture.join_leader().is_err());
        assert_eq!(fixture.parses.load(Ordering::Relaxed), 0);
        let terminal = fixture.domain.snapshot();
        assert_eq!(terminal.dependency_pins, 0);
        assert_eq!(terminal.dependency_edges, 0);
        assert_eq!(terminal.dependency_cleanup_acks, 1);
        let blocked = fixture.broker.snapshot();
        assert_eq!(blocked.grants, fixture.queued_snapshot.grants);
        assert_eq!(blocked.operations[&fixture.arena.epoch()].queued, 0);
        assert_eq!(blocked.operations[&fixture.arena.epoch()].in_flight, 0);
        fixture.finish_open("queued hook panic");
    }

    #[test]
    fn c2_queued_arena_close_waits_for_ack_then_cancels_without_granting() {
        let mut fixture = c2_queued_fixture(0);
        let terminal_cleanup = dependency_terminal_cleanup_hook(&fixture.domain);
        let (wait_entered, wait_action) = dependency_wait_hook(&fixture.domain);
        let closing = fixture.arena.clone();
        let close_join = thread::spawn(move || closing.close());
        let (waited_key, transition) = wait_entered.recv().unwrap();
        assert_eq!(waited_key, fixture.cell.key);
        assert_eq!(
            transition,
            DependencyTransition::Announcing {
                token: fixture.token,
                kind: DependencyEventKind::MemberQueued,
            }
        );
        fixture.queued_action.send(PhaseAction::Continue).unwrap();
        wait_action.send(()).unwrap();
        let (cleanup_key, cleanup_owner) = terminal_cleanup.recv().unwrap();
        assert_eq!(cleanup_key, fixture.cell.key);
        assert!(matches!(
            cleanup_owner,
            DependencyTerminalCleanupOwner::ArenaClose
                | DependencyTerminalCleanupOwner::LeaderTerminal
        ));
        let leader_error = fixture.join_leader().unwrap().unwrap_err();
        assert_eq!(leader_error.detail, CellControlTag::ArenaClosed.detail());
        close_join.join().unwrap();
        assert_eq!(fixture.parses.load(Ordering::Relaxed), 0);
        let terminal = fixture.domain.snapshot();
        assert_eq!(terminal.cells, 0);
        assert_eq!(terminal.dependency_pins, 0);
        assert_eq!(terminal.dependency_edges, 0);
        let blocked = fixture.broker.snapshot();
        assert_eq!(blocked.grants, fixture.queued_snapshot.grants);
        fixture.finish_closed("queued arena close");
    }

    #[test]
    fn c2_close_acknowledges_only_after_external_cleanup_and_operation_close() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let object_reader = reader(1_516);
        let pin = arena
            .resolve((1, 0), |permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        drop(pin);
        let hooks = Arc::new(PausingCloseHooks {
            entered: Barrier::new(2),
            release: Barrier::new(2),
        });
        domain.set_close_hooks(hooks.clone());
        let closing = arena.clone();
        let close_join = thread::spawn(move || closing.close());
        hooks.entered.wait();
        let paused = domain.snapshot();
        assert_eq!(paused.closes, 1);
        assert_eq!(paused.close_acknowledgements, 0);
        assert_eq!(broker.snapshot().active_operations, 1);
        hooks.release.wait();
        close_join.join().unwrap();
        let acknowledged = domain.snapshot();
        assert_eq!(acknowledged.closes, 1);
        assert_eq!(acknowledged.close_acknowledgements, 1);
        assert_eq!(broker.snapshot().active_operations, 0);
        drop(arena);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn c2_queued_cancelled_leader_acks_before_oldest_successor_queues_without_grant() {
        let mut fixture = c2_queued_fixture(2);
        let oldest = fixture.followers.remove(0);
        let oldest_slot = oldest.slot;
        let oldest_cancel = oldest.cancellation_handle();
        let later = fixture.followers.remove(0);
        let successor_generation = fixture.token.generation + 1;
        let (successor_hooks, successor_entered, successor_action) = dependency_hook_for_generation(
            &fixture.domain,
            DependencyEventKind::MemberQueued,
            successor_generation,
        );
        let (wait_entered, wait_action) = dependency_wait_hook(&fixture.domain);
        let cancel = fixture.leader_cancel.clone();
        let cancel_join = thread::spawn(move || cancel.cancel());
        wait_entered.recv().unwrap();
        fixture.queued_action.send(PhaseAction::Continue).unwrap();
        wait_action.send(()).unwrap();
        cancel_join.join().unwrap();
        let container = fixture.container;
        let oldest_join = thread::spawn(move || {
            oldest.resolve_scheduled_member_synthetic(
                container,
                |_| panic!("cached container must hit for queued successor"),
                |_| panic!("blocked oldest successor cannot parse"),
            )
        });
        let successor = successor_entered.recv().unwrap();
        assert_eq!(successor.token.generation, successor_generation);
        assert_eq!(successor.token.leader_slot, oldest_slot);
        let kinds = lock(&successor_hooks.events)
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        let cleanup = kinds
            .iter()
            .position(|kind| *kind == DependencyEventKind::CleanupAcknowledged)
            .unwrap();
        let queued = kinds
            .iter()
            .rposition(|kind| *kind == DependencyEventKind::MemberQueued)
            .unwrap();
        assert!(cleanup < queued);
        let successor_snapshot = fixture.broker.snapshot();
        assert_eq!(successor_snapshot.grants, fixture.queued_snapshot.grants);
        assert_eq!(
            successor_snapshot.operations[&fixture.arena.epoch()].queued,
            1
        );
        assert_eq!(
            successor_snapshot.operations[&fixture.arena.epoch()].in_flight,
            0
        );
        assert_eq!(fixture.parses.load(Ordering::Relaxed), 0);

        let (cancel_wait_entered, cancel_wait_action) = dependency_wait_hook(&fixture.domain);
        let successor_cancel_join = thread::spawn(move || oldest_cancel.cancel());
        cancel_wait_entered.recv().unwrap();
        successor_action.send(PhaseAction::Continue).unwrap();
        cancel_wait_action.send(()).unwrap();
        successor_cancel_join.join().unwrap();
        assert!(fixture.join_leader().unwrap().is_err());
        assert!(oldest_join.join().unwrap().is_err());
        drop(later);
        assert_eq!(fixture.parses.load(Ordering::Relaxed), 0);
        assert_eq!(
            fixture.broker.snapshot().grants,
            fixture.queued_snapshot.grants
        );
        fixture.finish_open("queued FIFO successor cancellation");
    }

    #[test]
    fn c2_queued_stale_install_take_and_publication_wait_without_mutating_exact_token() {
        let mut fixture = c2_queued_fixture(0);
        let stale = DependencyLeaderToken {
            generation: fixture.token.generation + 1,
            ..fixture.token
        };
        let external_cancellation = {
            let state = lock(&fixture.cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("queued stale cancellation requires Loading")
            };
            loading.broker_cancellation.clone().unwrap()
        };
        let (external_cancellation, stale_install) = fixture
            .arena
            .inner
            .install_member_broker_cancellation(&fixture.cell, stale, external_cancellation)
            .expect_err("stale queued cancellation replaced exact owner");
        assert_eq!(
            stale_install.detail,
            CellControlDetail::Static(CellControlTag::DependencyRejectedStale)
        );
        drop(external_cancellation);
        let stale_permit = ScalarResolutionPermit::new(4_194_304);
        let (stale_permit, stale_permit_error) = fixture
            .arena
            .inner
            .install_member_permit(&fixture.cell, stale, stale_permit)
            .expect_err("stale queued permit replaced exact owner");
        assert_eq!(
            stale_permit_error.detail,
            CellControlDetail::Static(CellControlTag::DependencyRejectedStale)
        );
        drop(stale_permit);
        assert!(fixture
            .arena
            .inner
            .take_member_permit_after_attempt(&fixture.cell, stale)
            .unwrap()
            .is_none());

        let attack_reader = reader(1_616);
        let attack_permit = ScalarResolutionPermit::new(4_194_304);
        let attack_object = load(&attack_reader, (1, 0), &attack_permit).unwrap();
        let attack_owner = Arc::new(ResolvedObjectOwner {
            payload: CellPayload::Object(attack_object),
            permit: test_clone_permit(&attack_permit),
            transition_gate: Mutex::new(()),
            cache_backed: AtomicBool::new(false),
            charge: Mutex::new(None),
            self_pin: Mutex::new(None),
        });
        let (wait_entered, wait_action) = dependency_wait_hook(&fixture.domain);
        let attack_inner = Arc::clone(&fixture.arena.inner);
        let attack_cell = Arc::clone(&fixture.cell);
        let attack_join = thread::spawn(move || {
            attack_inner.publish_ready(
                &attack_cell,
                stale.leader_slot,
                stale.generation,
                attack_owner,
                true,
            );
        });
        let (waited_key, transition) = wait_entered.recv().unwrap();
        assert_eq!(waited_key, fixture.cell.key);
        assert_eq!(
            transition,
            DependencyTransition::Announcing {
                token: fixture.token,
                kind: DependencyEventKind::MemberQueued,
            }
        );
        {
            let state = lock(&fixture.cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("stale queued attack replaced Loading")
            };
            assert_eq!(loading.generation, fixture.token.generation);
            assert_eq!(loading.leader_slot, fixture.token.leader_slot);
            assert!(loading.broker_cancellation.is_some());
            assert!(loading.permit.is_none());
            assert_eq!(loading.dependency.as_ref().unwrap().token, fixture.token);
        }
        fixture.queued_action.send(PhaseAction::Continue).unwrap();
        wait_action.send(()).unwrap();
        attack_join.join().unwrap();
        fixture.leader_cancel.cancel();
        assert!(fixture.join_leader().unwrap().is_err());
        fixture.finish_open("queued stale attacks");
    }

    #[test]
    fn c2_queued_duplicate_cancellation_invariant_exact_teardown_is_sibling_safe() {
        let mut fixture = c2_queued_fixture(0);
        fixture.queued_action.send(PhaseAction::Continue).unwrap();
        fixture
            .arena
            .inner
            .wait_for_dependency_transition(&fixture.cell);
        let duplicate = {
            let state = lock(&fixture.cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("queued duplicate cancellation requires Loading")
            };
            loading.broker_cancellation.clone().unwrap()
        };
        let (duplicate, invariant) = fixture
            .arena
            .inner
            .install_member_broker_cancellation(&fixture.cell, fixture.token, duplicate)
            .expect_err("duplicate exact queued cancellation did not trip invariant");
        assert_eq!(
            invariant.detail,
            CellControlDetail::Static(CellControlTag::DependencyStateInvariant)
        );
        drop(duplicate);

        let terminal_cleanup = dependency_terminal_cleanup_hook(&fixture.domain);
        let teardown_inner = Arc::clone(&fixture.arena.inner);
        let teardown_cell = Arc::clone(&fixture.cell);
        let token = fixture.token;
        let teardown = thread::spawn(move || {
            teardown_inner.close_exact_control(
                &teardown_cell,
                token.leader_slot,
                token.generation,
                invariant,
                true,
            )
        });
        assert_eq!(terminal_cleanup.recv().unwrap().0, fixture.cell.key);
        assert!(teardown.join().unwrap());
        assert!(fixture.join_leader().unwrap().is_err());
        assert!(fixture.domain.snapshot().invariant_failed);
        fixture.finish_open("queued duplicate-cancellation invariant");
    }

    fn prepared_member_dependency(
        arena: &ObjectCellArena,
        member: ObjectId,
        container: ObjectId,
    ) -> (ObjectCellRequest, Arc<Cell>, DependencyLeaderToken) {
        let request = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let cell = Arc::clone(&request.cell);
        let token = {
            let mut state = lock(&cell.state);
            let CellPhase::Loading(loading) = &mut state.phase else {
                panic!("prepared member dependency requires a loading cell")
            };
            loading.leader_running = true;
            DependencyLeaderToken {
                key: cell.key,
                generation: loading.generation,
                leader_slot: loading.leader_slot,
            }
        };
        let child = arena
            .inner
            .request_representation(container, Representation::DeclaredObjStmContainer)
            .unwrap();
        let child_cancellation = child.cancellation_handle();
        match arena.inner.install_dependency_cancellation(
            &cell,
            token,
            container,
            child_cancellation,
        ) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("dependency preparation unexpectedly captured a hook panic"),
            Err((_cancellation, control)) => {
                panic!("dependency cancellation installation failed: {control:?}")
            }
        }
        let pin = child
            .resolve_object_stream(|_| panic!("prepared dependency container reloaded"))
            .unwrap();
        let cancellation = arena
            .inner
            .take_dependency_cancellation(&cell, token)
            .unwrap()
            .expect("prepared dependency cancellation must remain installed");
        drop(cancellation);
        match arena.inner.install_dependency_pin(&cell, token, pin) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("dependency preparation unexpectedly captured a hook panic"),
            Err((_pin, control)) => panic!("dependency pin installation failed: {control:?}"),
        }
        {
            let mut state = lock(&cell.state);
            let CellPhase::Loading(loading) = &mut state.phase else {
                panic!("prepared dependency lost its loading phase")
            };
            loading.leader_running = false;
        }
        let snapshot = arena.inner.domain.snapshot();
        assert_eq!(snapshot.dependency_cancellations, 0);
        assert_eq!(snapshot.dependency_pins, 1);
        assert_eq!(snapshot.dependency_edges, 1);
        assert_eq!(snapshot.dependency_leaders, 1);
        (request, cell, token)
    }

    fn assert_dependency_cleanup_failure_closed(cell: &Arc<Cell>) {
        let state = lock(&cell.state);
        let CellPhase::Closed(control) = &state.phase else {
            panic!("cleanup invariant must close the exact owner")
        };
        assert_eq!(
            control.detail,
            CellControlDetail::Static(CellControlTag::DependencyStateInvariant)
        );
        assert!(state.dependency_transition.is_idle());
        assert!(!state.cached);
    }

    fn assert_dependency_invariant_access(error: &AccessError) {
        assert_eq!(error.kind, AccessKind::Backend);
        assert_eq!(error.detail, "member dependency state invariant");
    }

    fn assert_closed_transition(
        cell: &Arc<Cell>,
        transition: DependencyTransition,
        tag: CellControlTag,
    ) {
        let state = lock(&cell.state);
        let CellPhase::Closed(control) = &state.phase else {
            panic!("R7 authority requires a provisional Closed phase")
        };
        assert_eq!(control.detail, CellControlDetail::Static(tag));
        assert_eq!(state.dependency_transition, transition);
    }

    fn attach_waiting_member_follower(
        domain: &ObjectCellDomain,
        arena: &ObjectCellArena,
        member: ObjectId,
    ) -> thread::JoinHandle<Result<ResolvedObjectPin, AccessError>> {
        let follower = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let (wait_tx, wait_rx) = mpsc::channel();
        domain.set_wait_hooks(Arc::new(SignallingWaitHooks {
            adds: AtomicU64::new(0),
            removes: AtomicU64::new(0),
            entered: wait_tx,
        }));
        let join = thread::spawn(move || {
            follower.resolve(|_| panic!("R7 follower must never become loader"))
        });
        wait_rx.recv().unwrap();
        join
    }

    fn attach_r9_outcome_follower(
        domain: &ObjectCellDomain,
        arena: &ObjectCellArena,
        member: ObjectId,
        outcome: mpsc::Sender<R9FollowerOutcome>,
    ) -> thread::JoinHandle<()> {
        let follower = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let (wait_tx, wait_rx) = mpsc::channel();
        domain.set_wait_hooks(Arc::new(SignallingWaitHooks {
            adds: AtomicU64::new(0),
            removes: AtomicU64::new(0),
            entered: wait_tx,
        }));
        let join = thread::spawn(move || {
            let result = follower
                .resolve(|_| panic!("R9 follower must never become loader"))
                .map(drop);
            outcome.send(R9FollowerOutcome::Returned(result)).unwrap();
        });
        wait_rx.recv().unwrap();
        join
    }

    fn rearm_r9_wait_observer(domain: &ObjectCellDomain, outcome: mpsc::Sender<R9FollowerOutcome>) {
        domain.set_dependency_wait_hooks(Arc::new(ReportingDependencyWaitHooks { outcome }));
    }

    fn expect_r9_waited(
        outcome: &mpsc::Receiver<R9FollowerOutcome>,
        expected: DependencyTransition,
    ) {
        match outcome.recv().unwrap() {
            R9FollowerOutcome::Waited(transition) => assert_eq!(transition, expected),
            R9FollowerOutcome::Returned(_) => {
                panic!("follower returned before caller-owned terminal cleanup")
            }
        }
    }

    fn expect_r9_returned(outcome: &mpsc::Receiver<R9FollowerOutcome>) -> Result<(), AccessError> {
        match outcome.recv().unwrap() {
            R9FollowerOutcome::Returned(result) => result,
            R9FollowerOutcome::Waited(transition) => {
                panic!("unexpected additional transition wait: {transition:?}")
            }
        }
    }

    fn force_dependency_follower_wait(
        domain: &ObjectCellDomain,
        cell: &Arc<Cell>,
        expected: DependencyTransition,
    ) {
        let (wait_entered, wait_action) = dependency_wait_hook(domain);
        cell.ready.notify_all();
        let (key, transition) = wait_entered.recv().unwrap();
        assert_eq!(key, cell.key);
        assert_eq!(transition, expected);
        wait_action.send(()).unwrap();
    }

    fn establish_r10_condvar_wait(
        domain: &ObjectCellDomain,
        cell: &Arc<Cell>,
        transition: DependencyTransition,
    ) {
        let (pre_wait_entered, pre_wait_action) = dependency_pre_wait_hook(domain);
        force_dependency_follower_wait(domain, cell, transition);
        let (key, observed) = pre_wait_entered.recv().unwrap();
        assert_eq!(key, cell.key);
        assert_eq!(observed, transition);
        pre_wait_action.send(()).unwrap();
        let state = lock(&cell.state);
        assert_eq!(state.dependency_transition, transition);
        drop(state);
    }

    impl LeaderPhaseHooks for PausingLeaderPhaseHooks {
        fn enter(&self, phase: LeaderPhase) {
            if phase != self.target || !self.armed.swap(false, Ordering::AcqRel) {
                return;
            }
            self.entered.send(()).unwrap();
            match lock(&self.action).recv().unwrap() {
                PhaseAction::Continue => {}
                PhaseAction::Panic => panic!("injected leader-phase panic at {phase:?}"),
            }
        }
    }

    impl CloseEdgeHooks for PausingCloseHooks {
        fn after_phase_replacement(&self) {
            self.entered.wait();
            self.release.wait();
        }
    }

    impl WaitEdgeHooks for CountingHooks {
        fn add(&self, _epoch: u64, _id: ObjectId, generation: u64, ordinal: u64) {
            self.adds.fetch_add(1, Ordering::Relaxed);
            lock(&self.seen).push((generation, ordinal));
            if let Some(sender) = lock(&self.entered).take() {
                sender.send(()).unwrap();
            }
        }

        fn remove(&self, _epoch: u64, _id: ObjectId, _generation: u64, _ordinal: u64) {
            self.removes.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl WaitEdgeHooks for SignallingWaitHooks {
        fn add(&self, _epoch: u64, _id: ObjectId, _generation: u64, _ordinal: u64) {
            self.adds.fetch_add(1, Ordering::Relaxed);
            self.entered.send(()).unwrap();
        }

        fn remove(&self, _epoch: u64, _id: ObjectId, _generation: u64, _ordinal: u64) {
            self.removes.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn exact_mixed_lru_is_global_across_representation_and_epoch() {
        let raw_reader = reader(241);
        let raw_permit = ScalarResolutionPermit::new(LOADER_ESTIMATE_BYTES);
        let raw_weight = load(&raw_reader, (1, 0), &raw_permit)
            .unwrap()
            .retained_bytes();
        assert_eq!(raw_permit.stats().current_bytes, 0);
        let (stream_reader, _, container, _) = object_stream_reader();
        let stream_permit = ScalarResolutionPermit::new(LOADER_ESTIMATE_BYTES);
        let stream_weight = stream_reader
            .prepare_object_stream_with_permit(container, &stream_permit)
            .unwrap()
            .retained_bytes();
        assert_eq!(stream_permit.stats().current_bytes, 0);
        let target = CELL_METADATA_BYTES + raw_weight.max(stream_weight);

        {
            let broker = broker();
            let domain = ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(target));
            let raw_arena = domain.open_arena().unwrap();
            let container_arena = domain.open_arena().unwrap();
            let raw_loads = AtomicU64::new(0);
            let first = raw_arena
                .resolve((1, 0), |permit| {
                    raw_loads.fetch_add(1, Ordering::Relaxed);
                    load(&raw_reader, (1, 0), permit)
                })
                .unwrap();
            let first_pointer = first.pointer();
            let first_owner = Arc::downgrade(first.owner());
            drop(first);
            let container_pin = container_arena
                .resolve_object_stream(container, |permit| {
                    stream_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                })
                .unwrap();
            drop(container_pin);
            let reloaded = raw_arena
                .resolve((1, 0), |permit| {
                    raw_loads.fetch_add(1, Ordering::Relaxed);
                    load(&raw_reader, (1, 0), permit)
                })
                .unwrap();
            assert_ne!(reloaded.pointer(), first_pointer);
            assert!(first_owner.upgrade().is_none());
            assert_eq!(reloaded.owner().as_object().as_i64().unwrap(), 241);
            assert_eq!(raw_loads.load(Ordering::Relaxed), 2);
            drop(reloaded);
            let snapshot = domain.snapshot();
            assert_eq!(
                snapshot.raw,
                RepresentationSnapshot {
                    calls: 2,
                    loads: 2,
                    evictions: 1,
                    ..RepresentationSnapshot::default()
                }
            );
            assert_eq!(
                snapshot.containers,
                RepresentationSnapshot {
                    calls: 1,
                    loads: 1,
                    evictions: 1,
                    ..RepresentationSnapshot::default()
                }
            );
            assert_eq!(snapshot.members, RepresentationSnapshot::default());
            assert_eq!(snapshot.cache_bytes, CELL_METADATA_BYTES + raw_weight);
            assert_representation_counter_sums(&snapshot);
            let ownership = broker.snapshot();
            assert_eq!(
                ownership.operations[&raw_arena.epoch()].cache_bytes,
                ARENA_METADATA_BYTES + CELL_METADATA_BYTES + raw_weight
            );
            assert_eq!(
                ownership.operations[&container_arena.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            raw_arena.close();
            container_arena.close();
            drop(raw_arena);
            drop(container_arena);
            assert_gate4_terminal(&domain, &broker, 2);
        }

        {
            let broker = broker();
            let domain = ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(target));
            let container_arena = domain.open_arena().unwrap();
            let raw_arena = domain.open_arena().unwrap();
            let first = container_arena
                .resolve_object_stream(container, |permit| {
                    stream_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                })
                .unwrap();
            let first_pointer = first.pointer();
            let first_owner = Arc::downgrade(&first.inner.owner);
            drop(first);
            let raw = raw_arena
                .resolve((1, 0), |permit| load(&raw_reader, (1, 0), permit))
                .unwrap();
            drop(raw);
            let reloaded = container_arena
                .resolve_object_stream(container, |permit| {
                    stream_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                })
                .unwrap();
            assert_ne!(reloaded.pointer(), first_pointer);
            assert!(first_owner.upgrade().is_none());
            assert_eq!(reloaded.as_object_stream().container_id(), container);
            drop(reloaded);
            let snapshot = domain.snapshot();
            assert_eq!(
                snapshot.containers,
                RepresentationSnapshot {
                    calls: 2,
                    loads: 2,
                    evictions: 1,
                    ..RepresentationSnapshot::default()
                }
            );
            assert_eq!(
                snapshot.raw,
                RepresentationSnapshot {
                    calls: 1,
                    loads: 1,
                    evictions: 1,
                    ..RepresentationSnapshot::default()
                }
            );
            assert_eq!(snapshot.members, RepresentationSnapshot::default());
            assert_eq!(snapshot.cache_bytes, CELL_METADATA_BYTES + stream_weight);
            assert_representation_counter_sums(&snapshot);
            let ownership = broker.snapshot();
            assert_eq!(
                ownership.operations[&container_arena.epoch()].cache_bytes,
                ARENA_METADATA_BYTES + CELL_METADATA_BYTES + stream_weight
            );
            assert_eq!(
                ownership.operations[&raw_arena.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            container_arena.close();
            raw_arena.close();
            drop(container_arena);
            drop(raw_arena);
            assert_gate4_terminal(&domain, &broker, 2);
        }
    }

    #[test]
    fn pinned_resident_forces_one_shared_bypass_then_container_reloads_and_hits() {
        let raw_reader = reader(251);
        let raw_permit = ScalarResolutionPermit::new(LOADER_ESTIMATE_BYTES);
        let raw_weight = load(&raw_reader, (1, 0), &raw_permit)
            .unwrap()
            .retained_bytes();
        let raw_peak = raw_permit.stats().peak_bytes;
        assert_eq!(raw_permit.stats().current_bytes, 0);
        let (stream_reader, _, container, _) = object_stream_reader();
        let stream_permit = ScalarResolutionPermit::new(LOADER_ESTIMATE_BYTES);
        let stream_weight = stream_reader
            .prepare_object_stream_with_permit(container, &stream_permit)
            .unwrap()
            .retained_bytes();
        let stream_peak = stream_permit.stats().peak_bytes;
        assert_eq!(stream_permit.stats().current_bytes, 0);
        let target = CELL_METADATA_BYTES + raw_weight.max(stream_weight);
        let broker = broker();
        let domain = ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(target));
        let arena = domain.open_arena().unwrap();
        let pinned_raw = arena
            .resolve((1, 0), |permit| load(&raw_reader, (1, 0), permit))
            .unwrap();
        let raw_held_permit = pinned_raw.inner.owner.permit.clone();
        assert_eq!(raw_held_permit.stats().current_bytes, raw_weight);
        assert_eq!(raw_held_permit.stats().peak_bytes, raw_peak);
        let active_preparations = Arc::new(AtomicUsize::new(0));
        let peak_preparations = Arc::new(AtomicUsize::new(0));

        let leader = arena
            .inner
            .request_representation(container, Representation::DeclaredObjStmContainer)
            .unwrap();
        let followers: Vec<_> = (0..3)
            .map(|_| {
                arena
                    .inner
                    .request_representation(container, Representation::DeclaredObjStmContainer)
                    .unwrap()
            })
            .collect();
        let (wait_tx, wait_rx) = mpsc::channel();
        let wait_hooks = Arc::new(SignallingWaitHooks {
            adds: AtomicU64::new(0),
            removes: AtomicU64::new(0),
            entered: wait_tx,
        });
        domain.set_wait_hooks(wait_hooks.clone());
        let (source_tx, source_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let leader_reader = Arc::clone(&stream_reader);
        let leader_active = Arc::clone(&active_preparations);
        let leader_peak = Arc::clone(&peak_preparations);
        let leader_join = thread::spawn(move || {
            leader.resolve_object_stream(|permit| {
                source_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                let active = leader_active.fetch_add(1, Ordering::AcqRel) + 1;
                leader_peak.fetch_max(active, Ordering::AcqRel);
                let result = leader_reader
                    .prepare_object_stream_with_permit(container, permit)
                    .map_err(|error| {
                        CellLoadError::objstm(crate::objstm_failures::classify(container, error))
                    });
                leader_active.fetch_sub(1, Ordering::AcqRel);
                result
            })
        });
        source_rx.recv().unwrap();
        let follower_joins: Vec<_> = followers
            .into_iter()
            .map(|follower| {
                thread::spawn(move || {
                    follower.resolve_object_stream(|_| panic!("attached follower cannot load"))
                })
            })
            .collect();
        for _ in 0..3 {
            wait_rx.recv().unwrap();
        }
        release_tx.send(()).unwrap();
        let leader_pin = leader_join.join().unwrap().unwrap();
        let mut pins = vec![leader_pin];
        for join in follower_joins {
            pins.push(join.join().unwrap().unwrap());
        }
        assert!(pins.iter().all(|pin| pin.pointer() == pins[0].pointer()));
        let (_, bypass_permit, bypass_charge) = pins[0].retained_evidence();
        assert_eq!(bypass_charge, stream_weight);
        assert_eq!(bypass_permit.stats().current_bytes, stream_weight);
        assert_eq!(bypass_permit.stats().peak_bytes, stream_peak);
        assert_eq!(pinned_raw.owner().as_object().as_i64().unwrap(), 251);
        let bypass = broker.snapshot().operations[&arena.epoch()].clone();
        assert_eq!(
            bypass.cache_bytes,
            ARENA_METADATA_BYTES + CELL_METADATA_BYTES
        );
        assert_eq!(bypass.pin_bytes, raw_weight);
        assert_eq!(bypass.bypass_bytes, stream_weight);
        assert_eq!(bypass.self_pinned_bytes, raw_weight + stream_weight);
        assert_eq!(wait_hooks.adds.load(Ordering::Relaxed), 3);
        assert_eq!(wait_hooks.removes.load(Ordering::Relaxed), 3);
        drop(pins);
        assert_eq!(bypass_permit.stats().current_bytes, 0);
        let after_bypass = broker.snapshot().operations[&arena.epoch()].clone();
        assert_eq!(
            after_bypass.cache_bytes,
            ARENA_METADATA_BYTES + CELL_METADATA_BYTES
        );
        assert_eq!(after_bypass.pin_bytes, raw_weight);
        assert_eq!(after_bypass.bypass_bytes, 0);
        assert_eq!(after_bypass.self_pinned_bytes, raw_weight);
        drop(pinned_raw);
        let raw_cached = broker.snapshot().operations[&arena.epoch()].clone();
        assert_eq!(
            raw_cached.cache_bytes,
            ARENA_METADATA_BYTES + CELL_METADATA_BYTES + raw_weight
        );
        assert_eq!(raw_cached.pin_bytes, 0);
        assert_eq!(raw_cached.bypass_bytes, 0);
        assert_eq!(raw_cached.self_pinned_bytes, 0);
        assert_eq!(raw_held_permit.stats().current_bytes, raw_weight);

        let reloaded = arena
            .resolve_object_stream(container, |permit| {
                let active = active_preparations.fetch_add(1, Ordering::AcqRel) + 1;
                peak_preparations.fetch_max(active, Ordering::AcqRel);
                let result = stream_reader
                    .prepare_object_stream_with_permit(container, permit)
                    .map_err(|error| {
                        CellLoadError::objstm(crate::objstm_failures::classify(container, error))
                    });
                active_preparations.fetch_sub(1, Ordering::AcqRel);
                result
            })
            .unwrap();
        let reload_pointer = reloaded.pointer();
        let (_, reload_permit, reload_charge) = reloaded.retained_evidence();
        assert_eq!(reload_charge, stream_weight);
        assert_eq!(reload_permit.stats().current_bytes, stream_weight);
        assert_eq!(reload_permit.stats().peak_bytes, stream_peak);
        assert_eq!(raw_held_permit.stats().current_bytes, 0);
        let reload_pin = broker.snapshot().operations[&arena.epoch()].clone();
        assert_eq!(
            reload_pin.cache_bytes,
            ARENA_METADATA_BYTES + CELL_METADATA_BYTES
        );
        assert_eq!(reload_pin.pin_bytes, stream_weight);
        assert_eq!(reload_pin.bypass_bytes, 0);
        assert_eq!(reload_pin.self_pinned_bytes, stream_weight);
        drop(reloaded);
        assert_eq!(reload_permit.stats().current_bytes, stream_weight);
        let hit = arena
            .resolve_object_stream(container, |_| panic!("reloaded container must hit"))
            .unwrap();
        assert_eq!(hit.pointer(), reload_pointer);
        let hit_ownership = broker.snapshot().operations[&arena.epoch()].clone();
        assert_eq!(
            hit_ownership.cache_bytes,
            ARENA_METADATA_BYTES + CELL_METADATA_BYTES
        );
        assert_eq!(hit_ownership.pin_bytes, stream_weight);
        assert_eq!(hit_ownership.bypass_bytes, 0);
        assert_eq!(hit_ownership.self_pinned_bytes, stream_weight);
        drop(hit);
        let snapshot = domain.snapshot();
        assert_eq!(
            snapshot.raw,
            RepresentationSnapshot {
                calls: 1,
                loads: 1,
                evictions: 1,
                ..RepresentationSnapshot::default()
            }
        );
        assert_eq!(
            snapshot.containers,
            RepresentationSnapshot {
                calls: 6,
                loads: 2,
                hits: 1,
                waits: 3,
                bypasses: 1,
                ..RepresentationSnapshot::default()
            }
        );
        assert_eq!(snapshot.members, RepresentationSnapshot::default());
        assert_eq!(snapshot.cache_bytes, CELL_METADATA_BYTES + stream_weight);
        assert_representation_counter_sums(&snapshot);
        let ownership = broker.snapshot().operations[&arena.epoch()].clone();
        assert_eq!(
            ownership.cache_bytes,
            ARENA_METADATA_BYTES + CELL_METADATA_BYTES + stream_weight
        );
        assert_eq!(ownership.pin_bytes, 0);
        assert_eq!(ownership.bypass_bytes, 0);
        assert_eq!(ownership.self_pinned_bytes, 0);
        assert_eq!(reload_permit.stats().current_bytes, stream_weight);
        assert_eq!(peak_preparations.load(Ordering::Acquire), 1);
        arena.close();
        drop(arena);
        assert_eq!(reload_permit.stats().current_bytes, 0);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn stale_ready_publication_after_close_cannot_evict_sibling_epoch() {
        let oracle_reader = reader(201);
        let oracle_permit = ScalarResolutionPermit::new(LOADER_ESTIMATE_BYTES);
        let retained = load(&oracle_reader, (1, 0), &oracle_permit)
            .unwrap()
            .retained_bytes();
        let target = CELL_METADATA_BYTES.checked_add(retained).unwrap();
        let broker = broker();
        let domain = ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(target));
        let stale_arena = domain.open_arena().unwrap();
        let sibling_arena = domain.open_arena().unwrap();

        let sibling_reader = reader(211);
        let sibling_loads = Arc::new(AtomicU64::new(0));
        let sibling_loads_first = Arc::clone(&sibling_loads);
        let sibling = sibling_arena
            .resolve((1, 0), |permit| {
                sibling_loads_first.fetch_add(1, Ordering::Relaxed);
                load(&sibling_reader, (1, 0), permit)
            })
            .unwrap();
        let sibling_pointer = sibling.pointer();
        let sibling_owner = Arc::downgrade(sibling.owner());
        drop(sibling);

        let (phase_tx, phase_rx) = mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);
        domain.set_leader_phase_hooks(Arc::new(PausingLeaderPhaseHooks {
            target: LeaderPhase::ReconciledBeforePublication,
            armed: AtomicBool::new(true),
            entered: phase_tx,
            action: Mutex::new(resume_rx),
        }));
        let stale = stale_arena.request((2, 0)).unwrap();
        let stale_reader = reader(223);
        let stale_join =
            thread::spawn(move || stale.resolve(|permit| load(&stale_reader, (1, 0), permit)));
        phase_rx.recv().unwrap();
        let before = domain.snapshot();
        let sibling_cache_before = broker.snapshot().operations[&sibling_arena.epoch()].cache_bytes;
        stale_arena.close();
        resume_tx.send(PhaseAction::Continue).unwrap();
        assert!(stale_join.join().unwrap().is_err());

        let sibling_reader = reader(211);
        let sibling_loads_retry = Arc::clone(&sibling_loads);
        let hit = sibling_arena
            .resolve((1, 0), |permit| {
                sibling_loads_retry.fetch_add(1, Ordering::Relaxed);
                load(&sibling_reader, (1, 0), permit)
            })
            .unwrap();
        assert_eq!(hit.pointer(), sibling_pointer);
        assert!(sibling_owner
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, hit.owner())));
        assert_eq!(sibling_loads.load(Ordering::Relaxed), 1);
        drop(hit);
        let after = domain.snapshot();
        assert_eq!(after.evictions, before.evictions);
        assert_eq!(after.raw.evictions, before.raw.evictions);
        assert_eq!(
            broker.snapshot().operations[&sibling_arena.epoch()].cache_bytes,
            sibling_cache_before
        );
        assert_literal_counter_vectors(
            &after,
            RepresentationSnapshot {
                calls: 3,
                loads: 2,
                hits: 1,
                ..RepresentationSnapshot::default()
            },
            RepresentationSnapshot {
                calls: 3,
                loads: 2,
                hits: 1,
                ..RepresentationSnapshot::default()
            },
            RepresentationSnapshot::default(),
            RepresentationSnapshot::default(),
        );
        sibling_arena.close();
        drop(stale_arena);
        drop(sibling_arena);
        assert_gate4_terminal(&domain, &broker, 2);
    }

    #[test]
    fn stale_persistent_publication_after_close_cannot_evict_sibling_epoch() {
        let sibling_id = (31, 0);
        let sibling_failure = container_persistent(sibling_id);
        let sibling_weight = sibling_failure.payload.retained_weight().unwrap();
        let target = CELL_METADATA_BYTES.checked_add(sibling_weight).unwrap();
        let broker = broker();
        let domain = ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(target));
        let stale_arena = domain.open_arena().unwrap();
        let sibling_arena = domain.open_arena().unwrap();

        let sibling_loads = Arc::new(AtomicU64::new(0));
        let sibling_loads_first = Arc::clone(&sibling_loads);
        let sibling = sibling_arena
            .resolve_object_stream(sibling_id, |_| {
                sibling_loads_first.fetch_add(1, Ordering::Relaxed);
                Err(sibling_failure)
            })
            .unwrap_err();
        let sibling_pointer = sibling.shared_pointer().unwrap();
        let sibling_owner = match &sibling {
            ContainerCellError::Shared(owner) => Arc::downgrade(owner),
            ContainerCellError::Control(_) => panic!("persistent sentinel must be shared"),
        };
        drop(sibling);

        let (phase_tx, phase_rx) = mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);
        domain.set_leader_phase_hooks(Arc::new(PausingLeaderPhaseHooks {
            target: LeaderPhase::ReconciledBeforePublication,
            armed: AtomicBool::new(true),
            entered: phase_tx,
            action: Mutex::new(resume_rx),
        }));
        let stale = stale_arena.request((32, 0)).unwrap();
        let stale_join = thread::spawn(move || {
            stale.resolve(|_| {
                Err(CellLoadError::new(
                    AccessError::typed((32, 0), AccessKind::Backend, "stale persistent"),
                    NegativeDisposition::Persistent,
                ))
            })
        });
        phase_rx.recv().unwrap();
        let before = domain.snapshot();
        let sibling_cache_before = broker.snapshot().operations[&sibling_arena.epoch()].cache_bytes;
        stale_arena.close();
        resume_tx.send(PhaseAction::Continue).unwrap();
        assert!(stale_join.join().unwrap().is_err());

        let sibling_loads_retry = Arc::clone(&sibling_loads);
        let hit = sibling_arena
            .resolve_object_stream(sibling_id, |_| {
                sibling_loads_retry.fetch_add(1, Ordering::Relaxed);
                Err(container_persistent(sibling_id))
            })
            .unwrap_err();
        assert_eq!(hit.shared_pointer(), Some(sibling_pointer));
        let ContainerCellError::Shared(hit_owner) = &hit else {
            panic!("persistent sentinel hit must be shared")
        };
        assert!(sibling_owner
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, hit_owner)));
        assert_eq!(sibling_loads.load(Ordering::Relaxed), 1);
        let after = domain.snapshot();
        assert_eq!(after.evictions, before.evictions);
        assert_eq!(after.containers.evictions, before.containers.evictions);
        assert_eq!(
            broker.snapshot().operations[&sibling_arena.epoch()].cache_bytes,
            sibling_cache_before
        );
        assert_literal_counter_vectors(
            &after,
            RepresentationSnapshot {
                calls: 3,
                loads: 2,
                negative_hits: 1,
                ..RepresentationSnapshot::default()
            },
            RepresentationSnapshot {
                calls: 1,
                loads: 1,
                ..RepresentationSnapshot::default()
            },
            RepresentationSnapshot {
                calls: 2,
                loads: 1,
                negative_hits: 1,
                ..RepresentationSnapshot::default()
            },
            RepresentationSnapshot::default(),
        );
        drop(hit);
        sibling_arena.close();
        drop(stale_arena);
        drop(sibling_arena);
        assert_gate4_terminal(&domain, &broker, 2);
    }

    #[test]
    fn close_loading_matrix_is_hook_driven_and_exact_at_every_boundary() {
        #[derive(Clone, Copy, Debug)]
        enum Row {
            NotStarted,
            Phase(LeaderPhase),
            SourceRead,
        }
        let rows = [
            Row::NotStarted,
            Row::Phase(LeaderPhase::QueuedBeforeWait),
            Row::Phase(LeaderPhase::Granted),
            Row::Phase(LeaderPhase::BeforeLoader),
            Row::SourceRead,
            Row::Phase(LeaderPhase::AfterLoaderResult),
            Row::Phase(LeaderPhase::ReconciledBeforePublication),
        ];
        for row in rows {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let (stream_raw, _, container, _) = object_stream_fixture();
            let stream_reader =
                Arc::new(IndexedReader::open(BytesSource::from(Arc::clone(&stream_raw))).unwrap());
            let stream_oracle_permit = ScalarResolutionPermit::new(LOADER_ESTIMATE_BYTES);
            let stream_oracle = stream_reader
                .prepare_object_stream_with_permit(container, &stream_oracle_permit)
                .unwrap();
            let stream_retained = stream_oracle.retained_bytes();
            let stream_peak = stream_oracle_permit.stats().peak_bytes;
            drop(stream_oracle);
            assert_eq!(stream_oracle_permit.stats().current_bytes, 0);

            let (source_control, source_reader) = if matches!(row, Row::SourceRead) {
                let (source, entered, resume) = BlockingReadSource::new(Arc::clone(&stream_raw));
                let shared: Arc<dyn RandomAccessSource> = source.clone();
                let reader = Arc::new(
                    IndexedReader::open_shared(shared, IndexedReaderOptions::default()).unwrap(),
                );
                source.arm();
                (Some((entered, resume)), Some(reader))
            } else {
                (None, None)
            };
            let representation = Representation::DeclaredObjStmContainer;
            let mut leader = Some(
                arena
                    .inner
                    .request_representation(container, representation)
                    .unwrap(),
            );
            let leader_cell = Arc::clone(&leader.as_ref().unwrap().cell);
            let needs_follower = matches!(
                row,
                Row::Phase(LeaderPhase::QueuedBeforeWait)
                    | Row::SourceRead
                    | Row::Phase(LeaderPhase::AfterLoaderResult)
                    | Row::Phase(LeaderPhase::ReconciledBeforePublication)
            );
            let follower = needs_follower.then(|| {
                arena
                    .inner
                    .request_representation(container, representation)
                    .unwrap()
            });
            let blocker = if matches!(row, Row::Phase(LeaderPhase::QueuedBeforeWait)) {
                let operation = broker.register_operation().unwrap();
                let pending = operation
                    .request(
                        Lane::Normal {
                            completion_reserve: 0,
                        },
                        LOADER_ESTIMATE_BYTES,
                    )
                    .unwrap();
                Some((operation, pending.wait().unwrap()))
            } else {
                None
            };

            let (phase_tx, phase_rx) = mpsc::sync_channel(0);
            let (phase_resume_tx, phase_resume_rx) = mpsc::sync_channel(0);
            if let Row::Phase(phase) = row {
                domain.set_leader_phase_hooks(Arc::new(PausingLeaderPhaseHooks {
                    target: phase,
                    armed: AtomicBool::new(true),
                    entered: phase_tx,
                    action: Mutex::new(phase_resume_rx),
                }));
            }
            let evidence = Arc::new(Mutex::new(None));
            let source_reader_for_assert = source_reader.clone();
            let leader_join = if matches!(row, Row::NotStarted) {
                None
            } else {
                let evidence_for_load = Arc::clone(&evidence);
                let leader_reader = Arc::clone(&stream_reader);
                let leader = leader.take().unwrap();
                Some(thread::spawn(move || match representation {
                    Representation::RawNormalObject => unreachable!(),
                    Representation::DeclaredObjStmContainer => leader
                        .resolve_object_stream(|permit| {
                            let selected_reader = source_reader.as_ref().unwrap_or(&leader_reader);
                            let result = selected_reader
                                .prepare_object_stream_with_permit(container, permit)
                                .map_err(|error| {
                                    CellLoadError::objstm(crate::objstm_failures::classify(
                                        container, error,
                                    ))
                                });
                            if let Ok(owner) = &result {
                                *lock(&evidence_for_load) =
                                    Some((permit.clone(), owner.retained_bytes()));
                            }
                            result
                        })
                        .is_err(),
                    Representation::DeclaredObjStmMember => unreachable!(),
                }))
            };
            match row {
                Row::NotStarted => {}
                Row::Phase(_) => phase_rx.recv().unwrap(),
                Row::SourceRead => source_control.as_ref().unwrap().0.recv().unwrap(),
            }

            let (wait_tx, wait_rx) = mpsc::channel();
            let wait_hooks = Arc::new(SignallingWaitHooks {
                adds: AtomicU64::new(0),
                removes: AtomicU64::new(0),
                entered: wait_tx,
            });
            domain.set_wait_hooks(wait_hooks.clone());
            let follower_join = follower.map(|follower| {
                thread::spawn(move || match representation {
                    Representation::RawNormalObject => unreachable!(),
                    Representation::DeclaredObjStmContainer => follower
                        .resolve_object_stream(|_| panic!("closed follower cannot load"))
                        .is_err(),
                    Representation::DeclaredObjStmMember => unreachable!(),
                })
            });
            if needs_follower {
                wait_rx.recv().unwrap();
            }

            let blocked_cells = domain.snapshot();
            let blocked_global = broker.snapshot();
            let blocked_operation = blocked_global.operations[&arena.epoch()].clone();
            assert_eq!(blocked_cells.arenas, 1, "{row:?}");
            assert_eq!(blocked_cells.cells, 1, "{row:?}");
            assert_eq!(blocked_cells.loading, 1, "{row:?}");
            assert_eq!(blocked_cells.ready, 0, "{row:?}");
            assert_eq!(blocked_cells.negative, 0, "{row:?}");
            assert_eq!(blocked_cells.external_pins, 0, "{row:?}");
            assert_eq!(blocked_cells.cache_bytes, CELL_METADATA_BYTES, "{row:?}");
            assert_eq!(
                lock(&domain.inner.state).loading_metadata_bytes,
                CELL_METADATA_BYTES,
                "{row:?}"
            );
            assert_eq!(
                blocked_cells.live_interests,
                if needs_follower { 2 } else { 1 },
                "{row:?}"
            );
            let blocked_cache_bytes = ARENA_METADATA_BYTES + CELL_METADATA_BYTES;
            let blocked_bypass_bytes =
                if matches!(row, Row::Phase(LeaderPhase::ReconciledBeforePublication)) {
                    stream_retained
                } else {
                    0
                };
            assert_eq!(blocked_operation.cache_bytes, blocked_cache_bytes);
            assert_eq!(blocked_operation.pin_bytes, 0);
            assert_eq!(blocked_operation.bypass_bytes, blocked_bypass_bytes);
            assert_eq!(blocked_operation.self_pinned_bytes, 0);
            assert_eq!(blocked_global.cache_bytes, blocked_cache_bytes);
            assert_eq!(blocked_global.pin_bytes, 0);
            assert_eq!(blocked_global.bypass_bytes, blocked_bypass_bytes);
            assert_eq!(blocked_global.completion_reserve_bytes, 0);
            assert_eq!(blocked_global.oversize_bytes, 0);
            assert_eq!(blocked_global.oversize_owners, 0);
            assert_eq!(blocked_global.error_metadata_bytes, 0);
            let (cell_permit, leader_running, phase_interests) = {
                let state = lock(&leader_cell.state);
                match &state.phase {
                    CellPhase::Loading(loading) => (
                        loading.permit.as_ref().map(|p| p.stats()),
                        loading.leader_running,
                        state.live_interests,
                    ),
                    _ => panic!("{row:?}: blocked cell left Loading"),
                }
            };
            assert_eq!(phase_interests, if needs_follower { 2 } else { 1 });
            assert_eq!(leader_running, !matches!(row, Row::NotStarted));
            match row {
                Row::NotStarted => {
                    assert_eq!(
                        blocked_global.normal_payload_bytes,
                        ARENA_METADATA_BYTES + CELL_METADATA_BYTES
                    );
                    assert_eq!(blocked_global.normal_in_flight_estimate_bytes, 0);
                    assert_eq!(
                        blocked_global.metadata_bytes,
                        crate::broker::OPERATION_METADATA_WEIGHT
                    );
                    assert_eq!(
                        blocked_global.aggregate_bytes,
                        ARENA_METADATA_BYTES
                            + CELL_METADATA_BYTES
                            + crate::broker::OPERATION_METADATA_WEIGHT
                    );
                    assert_eq!(blocked_global.queued, 0);
                    assert_eq!(blocked_global.in_flight, 0);
                    assert_eq!(blocked_global.live_request_records, 0);
                    assert_eq!(blocked_global.reservation_metadata_bytes, 0);
                    assert_eq!(blocked_global.active_operations, 1);
                    assert_eq!(blocked_operation.queued, 0);
                    assert_eq!(blocked_operation.in_flight, 0);
                    assert!(cell_permit.is_none());
                    let state = lock(&leader_cell.state);
                    assert!(
                        matches!(&state.phase, CellPhase::Loading(loading) if !loading.leader_running)
                    );
                }
                Row::Phase(LeaderPhase::QueuedBeforeWait) => {
                    assert_eq!(
                        blocked_global.normal_payload_bytes,
                        ARENA_METADATA_BYTES + CELL_METADATA_BYTES + LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.normal_in_flight_estimate_bytes,
                        LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.metadata_bytes,
                        2 * crate::broker::OPERATION_METADATA_WEIGHT
                            + 2 * crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(
                        blocked_global.aggregate_bytes,
                        blocked_global.normal_payload_bytes + blocked_global.metadata_bytes
                    );
                    assert_eq!(blocked_global.queued, 1);
                    assert_eq!(blocked_global.in_flight, 1);
                    assert_eq!(blocked_global.live_request_records, 2);
                    assert_eq!(
                        blocked_global.reservation_metadata_bytes,
                        crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(blocked_global.active_operations, 2);
                    assert_eq!(blocked_operation.queued, 1);
                    assert_eq!(blocked_operation.in_flight, 0);
                    assert!(cell_permit.is_none());
                }
                Row::Phase(LeaderPhase::Granted) => {
                    assert_eq!(
                        blocked_global.normal_payload_bytes,
                        ARENA_METADATA_BYTES + CELL_METADATA_BYTES + LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.normal_in_flight_estimate_bytes,
                        LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.metadata_bytes,
                        crate::broker::OPERATION_METADATA_WEIGHT
                            + crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(
                        blocked_global.aggregate_bytes,
                        blocked_global.normal_payload_bytes + blocked_global.metadata_bytes
                    );
                    assert_eq!(blocked_global.queued, 0);
                    assert_eq!(blocked_global.in_flight, 1);
                    assert_eq!(blocked_global.live_request_records, 1);
                    assert_eq!(
                        blocked_global.reservation_metadata_bytes,
                        crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(blocked_global.active_operations, 1);
                    assert_eq!(blocked_operation.queued, 0);
                    assert_eq!(blocked_operation.in_flight, 1);
                    assert!(cell_permit.is_none());
                }
                Row::Phase(LeaderPhase::BeforeLoader) => {
                    assert_eq!(
                        blocked_global.normal_payload_bytes,
                        ARENA_METADATA_BYTES + CELL_METADATA_BYTES + LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.normal_in_flight_estimate_bytes,
                        LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.metadata_bytes,
                        crate::broker::OPERATION_METADATA_WEIGHT
                            + crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(
                        blocked_global.aggregate_bytes,
                        blocked_global.normal_payload_bytes + blocked_global.metadata_bytes
                    );
                    assert_eq!(blocked_global.queued, 0);
                    assert_eq!(blocked_global.in_flight, 1);
                    assert_eq!(blocked_global.live_request_records, 1);
                    assert_eq!(
                        blocked_global.reservation_metadata_bytes,
                        crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(blocked_global.active_operations, 1);
                    assert_eq!(blocked_operation.queued, 0);
                    assert_eq!(blocked_operation.in_flight, 1);
                    let stats = cell_permit.expect("active loader permit");
                    assert_eq!(stats.current_bytes, 0);
                    assert_eq!(stats.peak_bytes, 0);
                }
                Row::SourceRead => {
                    const CONTAINER_READ_BLOCKED_BYTES: u64 = 256;
                    assert_eq!(
                        blocked_global.normal_payload_bytes,
                        ARENA_METADATA_BYTES + CELL_METADATA_BYTES + LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.normal_in_flight_estimate_bytes,
                        LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.metadata_bytes,
                        crate::broker::OPERATION_METADATA_WEIGHT
                            + crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(
                        blocked_global.aggregate_bytes,
                        blocked_global.normal_payload_bytes + blocked_global.metadata_bytes
                    );
                    assert_eq!(blocked_global.queued, 0);
                    assert_eq!(blocked_global.in_flight, 1);
                    assert_eq!(blocked_global.live_request_records, 1);
                    assert_eq!(
                        blocked_global.reservation_metadata_bytes,
                        crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(blocked_global.active_operations, 1);
                    assert_eq!(blocked_operation.queued, 0);
                    assert_eq!(blocked_operation.in_flight, 1);
                    let stats = cell_permit.expect("source-read permit");
                    assert_eq!(stats.current_bytes, CONTAINER_READ_BLOCKED_BYTES);
                    assert_eq!(stats.peak_bytes, CONTAINER_READ_BLOCKED_BYTES);
                }
                Row::Phase(LeaderPhase::AfterLoaderResult) => {
                    assert_eq!(
                        blocked_global.normal_payload_bytes,
                        ARENA_METADATA_BYTES + CELL_METADATA_BYTES + LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.normal_in_flight_estimate_bytes,
                        LOADER_ESTIMATE_BYTES
                    );
                    assert_eq!(
                        blocked_global.metadata_bytes,
                        crate::broker::OPERATION_METADATA_WEIGHT
                            + crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(
                        blocked_global.aggregate_bytes,
                        blocked_global.normal_payload_bytes + blocked_global.metadata_bytes
                    );
                    assert_eq!(blocked_global.queued, 0);
                    assert_eq!(blocked_global.in_flight, 1);
                    assert_eq!(blocked_global.live_request_records, 1);
                    assert_eq!(
                        blocked_global.reservation_metadata_bytes,
                        crate::broker::QUEUE_METADATA_WEIGHT
                    );
                    assert_eq!(blocked_global.active_operations, 1);
                    assert_eq!(blocked_operation.queued, 0);
                    assert_eq!(blocked_operation.in_flight, 1);
                    let stats = cell_permit.expect("prepared loader permit");
                    assert_eq!(stats.current_bytes, stream_retained);
                    assert_eq!(stats.peak_bytes, stream_peak);
                }
                Row::Phase(LeaderPhase::ReconciledBeforePublication) => {
                    assert_eq!(
                        blocked_global.normal_payload_bytes,
                        ARENA_METADATA_BYTES + CELL_METADATA_BYTES + stream_retained
                    );
                    assert_eq!(blocked_global.normal_in_flight_estimate_bytes, 0);
                    assert_eq!(
                        blocked_global.metadata_bytes,
                        crate::broker::OPERATION_METADATA_WEIGHT
                    );
                    assert_eq!(
                        blocked_global.aggregate_bytes,
                        ARENA_METADATA_BYTES
                            + CELL_METADATA_BYTES
                            + stream_retained
                            + crate::broker::OPERATION_METADATA_WEIGHT
                    );
                    assert_eq!(blocked_global.queued, 0);
                    assert_eq!(blocked_global.in_flight, 0);
                    assert_eq!(blocked_global.live_request_records, 0);
                    assert_eq!(blocked_global.reservation_metadata_bytes, 0);
                    assert_eq!(blocked_global.active_operations, 1);
                    assert_eq!(blocked_operation.queued, 0);
                    assert_eq!(blocked_operation.in_flight, 0);
                    assert!(cell_permit.is_none());
                    let held = lock(&evidence);
                    let (permit, retained) = held.as_ref().expect("reconciled evidence");
                    assert_eq!(*retained, stream_retained);
                    assert_eq!(permit.stats().current_bytes, stream_retained);
                    assert_eq!(permit.stats().peak_bytes, stream_peak);
                }
                Row::Phase(LeaderPhase::BeforeRequest) => unreachable!(),
            }
            if let Some(reader) = &source_reader_for_assert {
                assert_eq!(reader.cache_stats(), Default::default());
                assert_eq!(reader.object_cache_stats(), Default::default());
                assert_eq!(reader.object_stream_cache_stats(), Default::default());
            }

            arena.close();
            match row {
                Row::NotStarted => {}
                Row::Phase(_) => phase_resume_tx.send(PhaseAction::Continue).unwrap(),
                Row::SourceRead => source_control.as_ref().unwrap().1.send(()).unwrap(),
            }
            if let Some(join) = leader_join {
                assert!(join.join().unwrap(), "{row:?}");
            } else {
                assert!(leader
                    .take()
                    .unwrap()
                    .resolve_object_stream(|_| panic!("closed unstarted row cannot load"))
                    .is_err());
            }
            if let Some(join) = follower_join {
                assert!(join.join().unwrap(), "{row:?}");
            }
            drop(blocker);
            if let Some((permit, _)) = lock(&evidence).as_ref() {
                assert_eq!(permit.stats().current_bytes, 0, "{row:?}");
            }
            if let Some(reader) = &source_reader_for_assert {
                assert_eq!(reader.cache_stats(), Default::default());
                assert_eq!(reader.object_cache_stats(), Default::default());
                assert_eq!(reader.object_stream_cache_stats(), Default::default());
            }

            let snapshot = domain.snapshot();
            assert_eq!(snapshot.arenas, 0, "{row:?}");
            assert_eq!(snapshot.cells, 0, "{row:?}");
            assert_eq!(snapshot.loading, 0, "{row:?}");
            assert_eq!(snapshot.ready, 0, "{row:?}");
            assert_eq!(snapshot.negative, 0, "{row:?}");
            assert_eq!(snapshot.live_interests, 0, "{row:?}");
            assert_eq!(snapshot.external_pins, 0, "{row:?}");
            assert_eq!(snapshot.cache_bytes, 0, "{row:?}");
            assert_eq!(snapshot.closes, 1, "{row:?}");
            match row {
                Row::NotStarted => assert_literal_counter_vectors(
                    &snapshot,
                    RepresentationSnapshot {
                        calls: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                    RepresentationSnapshot {
                        calls: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                ),
                Row::Phase(LeaderPhase::QueuedBeforeWait) => assert_literal_counter_vectors(
                    &snapshot,
                    RepresentationSnapshot {
                        calls: 2,
                        waits: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                    RepresentationSnapshot {
                        calls: 2,
                        waits: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                ),
                Row::Phase(LeaderPhase::Granted) => assert_literal_counter_vectors(
                    &snapshot,
                    RepresentationSnapshot {
                        calls: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                    RepresentationSnapshot {
                        calls: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                ),
                Row::Phase(LeaderPhase::BeforeLoader) => assert_literal_counter_vectors(
                    &snapshot,
                    RepresentationSnapshot {
                        calls: 1,
                        loads: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                    RepresentationSnapshot {
                        calls: 1,
                        loads: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                ),
                Row::SourceRead => assert_literal_counter_vectors(
                    &snapshot,
                    RepresentationSnapshot {
                        calls: 2,
                        loads: 1,
                        waits: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                    RepresentationSnapshot {
                        calls: 2,
                        loads: 1,
                        waits: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                ),
                Row::Phase(LeaderPhase::AfterLoaderResult) => assert_literal_counter_vectors(
                    &snapshot,
                    RepresentationSnapshot {
                        calls: 2,
                        loads: 1,
                        waits: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                    RepresentationSnapshot {
                        calls: 2,
                        loads: 1,
                        waits: 1,
                        ..RepresentationSnapshot::default()
                    },
                    RepresentationSnapshot::default(),
                ),
                Row::Phase(LeaderPhase::ReconciledBeforePublication) => {
                    assert_literal_counter_vectors(
                        &snapshot,
                        RepresentationSnapshot {
                            calls: 2,
                            loads: 1,
                            waits: 1,
                            ..RepresentationSnapshot::default()
                        },
                        RepresentationSnapshot::default(),
                        RepresentationSnapshot {
                            calls: 2,
                            loads: 1,
                            waits: 1,
                            ..RepresentationSnapshot::default()
                        },
                        RepresentationSnapshot::default(),
                    )
                }
                Row::Phase(LeaderPhase::BeforeRequest) => unreachable!(),
            }
            assert_eq!(stream_reader.cache_stats(), Default::default());
            assert_eq!(stream_reader.object_cache_stats(), Default::default());
            assert_eq!(
                stream_reader.object_stream_cache_stats(),
                Default::default()
            );
            assert_eq!(
                wait_hooks.adds.load(Ordering::Relaxed),
                u64::from(needs_follower)
            );
            assert_eq!(
                wait_hooks.removes.load(Ordering::Relaxed),
                u64::from(needs_follower)
            );
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }
    }

    #[test]
    fn close_completed_flight_and_bypass_states_preserve_held_owners_then_drain() {
        {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let object_reader = reader(261);
            let pin = arena
                .resolve((1, 0), |permit| load(&object_reader, (1, 0), permit))
                .unwrap();
            let pointer = pin.pointer();
            let retained = pin.owner().retained_bytes();
            let retained_permit = pin.inner.owner.permit.clone();
            let epoch = arena.epoch();
            arena.close();
            assert_eq!(pin.pointer(), pointer);
            assert_eq!(pin.owner().as_object().as_i64().unwrap(), 261);
            let snapshot = domain.snapshot();
            assert_eq!(snapshot.cells, 0);
            assert_eq!(snapshot.closes, 1);
            assert_literal_counter_vectors(
                &snapshot,
                RepresentationSnapshot {
                    calls: 1,
                    loads: 1,
                    ..RepresentationSnapshot::default()
                },
                RepresentationSnapshot {
                    calls: 1,
                    loads: 1,
                    ..RepresentationSnapshot::default()
                },
                RepresentationSnapshot::default(),
                RepresentationSnapshot::default(),
            );
            assert_eq!(retained_permit.stats().current_bytes, retained);
            assert_gate4_held_after_close(&domain, &broker, epoch, retained, retained, 0, retained);
            drop(pin);
            assert_eq!(retained_permit.stats().current_bytes, 0);
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }

        {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let id = (262, 0);
            let error = arena
                .resolve_object_stream(id, |_| Err(container_persistent(id)))
                .unwrap_err();
            let pointer = error.shared_pointer().unwrap();
            let ContainerCellError::Shared(error_owner) = &error else {
                panic!("persistent close owner")
            };
            let retained = error_owner.retained_weight();
            let epoch = arena.epoch();
            arena.close();
            assert_eq!(error.shared_pointer(), Some(pointer));
            let snapshot = domain.snapshot();
            assert_eq!(snapshot.cells, 0);
            assert_eq!(snapshot.closes, 1);
            assert_literal_counter_vectors(
                &snapshot,
                RepresentationSnapshot {
                    calls: 1,
                    loads: 1,
                    ..RepresentationSnapshot::default()
                },
                RepresentationSnapshot::default(),
                RepresentationSnapshot {
                    calls: 1,
                    loads: 1,
                    ..RepresentationSnapshot::default()
                },
                RepresentationSnapshot::default(),
            );
            assert_gate4_held_after_close(&domain, &broker, epoch, retained, 0, retained, 0);
            drop(error);
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }

        {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let id = (263, 0);
            let leader = arena
                .inner
                .request_representation(id, Representation::DeclaredObjStmContainer)
                .unwrap();
            let follower = arena
                .inner
                .request_representation(id, Representation::DeclaredObjStmContainer)
                .unwrap();
            let first = leader
                .resolve_object_stream(|_| Err(container_transient(id)))
                .unwrap_err();
            let second = follower
                .resolve_object_stream(|_| panic!("attached flight follower cannot load"))
                .unwrap_err();
            assert_eq!(first.shared_pointer(), second.shared_pointer());
            let ContainerCellError::Shared(first_owner) = &first else {
                panic!("flight close owner")
            };
            let retained = first_owner.retained_weight();
            let epoch = arena.epoch();
            arena.close();
            assert_eq!(first.shared_pointer(), second.shared_pointer());
            let snapshot = domain.snapshot();
            assert_eq!(snapshot.cells, 0);
            assert_eq!(snapshot.closes, 1);
            assert_literal_counter_vectors(
                &snapshot,
                RepresentationSnapshot {
                    calls: 2,
                    loads: 1,
                    waits: 1,
                    transient_shares: 1,
                    ..RepresentationSnapshot::default()
                },
                RepresentationSnapshot::default(),
                RepresentationSnapshot {
                    calls: 2,
                    loads: 1,
                    waits: 1,
                    transient_shares: 1,
                    ..RepresentationSnapshot::default()
                },
                RepresentationSnapshot::default(),
            );
            assert_gate4_held_after_close(&domain, &broker, epoch, retained, 0, retained, 0);
            drop(second);
            drop(first);
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }

        {
            let broker = broker();
            let domain = ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(0));
            let arena = domain.open_arena().unwrap();
            let object_reader = reader(271);
            let pin = arena
                .resolve((1, 0), |permit| load(&object_reader, (1, 0), permit))
                .unwrap();
            let pointer = pin.pointer();
            let retained = pin.owner().retained_bytes();
            let retained_permit = pin.inner.owner.permit.clone();
            let epoch = arena.epoch();
            arena.close();
            assert_eq!(pin.pointer(), pointer);
            assert_eq!(pin.owner().as_object().as_i64().unwrap(), 271);
            let snapshot = domain.snapshot();
            assert_eq!(snapshot.cells, 0);
            assert_eq!(snapshot.closes, 1);
            assert_literal_counter_vectors(
                &snapshot,
                RepresentationSnapshot {
                    calls: 1,
                    loads: 1,
                    bypasses: 1,
                    ..RepresentationSnapshot::default()
                },
                RepresentationSnapshot {
                    calls: 1,
                    loads: 1,
                    bypasses: 1,
                    ..RepresentationSnapshot::default()
                },
                RepresentationSnapshot::default(),
                RepresentationSnapshot::default(),
            );
            assert_eq!(retained_permit.stats().current_bytes, retained);
            assert_gate4_held_after_close(&domain, &broker, epoch, retained, 0, retained, retained);
            drop(pin);
            assert_eq!(retained_permit.stats().current_bytes, 0);
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }
    }

    #[test]
    fn generic_epoch_states_and_charges_never_alias_or_cross_close_boundaries() {
        fn dynamic_container_failure(id: ObjectId, detail: String) -> CellLoadError {
            CellLoadError::objstm(crate::objstm_failures::classify(
                id,
                lopdf::IndexedReaderError::ObjectStreamMember {
                    id,
                    container: id,
                    index: 0,
                    source: lopdf::Error::InvalidObjectStream(detail),
                },
            ))
        }

        {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena_a = domain.open_arena().unwrap();
            let arena_b = domain.open_arena().unwrap();
            let reader_a = reader(281);
            let reader_b = reader(282);
            let a = arena_a
                .resolve((1, 0), |permit| load(&reader_a, (1, 0), permit))
                .unwrap();
            let b = arena_b
                .resolve((1, 0), |permit| load(&reader_b, (1, 0), permit))
                .unwrap();
            let generation_one = arena_b
                .resolve((1, 1), |permit| load(&reader_b, (1, 0), permit))
                .unwrap();
            assert_ne!(a.pointer(), b.pointer());
            assert_ne!(b.pointer(), generation_one.pointer());
            assert_ne!(a.owner().charge_pointer(), b.owner().charge_pointer());
            assert_ne!(
                b.owner().charge_pointer(),
                generation_one.owner().charge_pointer()
            );
            let ready_weight = a.owner().retained_bytes();
            assert_eq!(b.owner().retained_bytes(), ready_weight);
            assert_eq!(generation_one.owner().retained_bytes(), ready_weight);
            let a_permit = a.inner.owner.permit.clone();
            let b_permit = b.inner.owner.permit.clone();
            let generation_one_permit = generation_one.inner.owner.permit.clone();
            assert_eq!(a_permit.stats().current_bytes, ready_weight);
            assert_eq!(b_permit.stats().current_bytes, ready_weight);
            assert_eq!(generation_one_permit.stats().current_bytes, ready_weight);
            let before_close = broker.snapshot();
            assert_eq!(
                before_close.operations[&arena_a.epoch()].cache_bytes,
                ARENA_METADATA_BYTES + CELL_METADATA_BYTES
            );
            assert_eq!(
                before_close.operations[&arena_a.epoch()].pin_bytes,
                ready_weight
            );
            assert_eq!(before_close.operations[&arena_a.epoch()].bypass_bytes, 0);
            assert_eq!(
                before_close.operations[&arena_a.epoch()].self_pinned_bytes,
                ready_weight
            );
            assert_eq!(
                before_close.operations[&arena_b.epoch()].cache_bytes,
                ARENA_METADATA_BYTES + 2 * CELL_METADATA_BYTES
            );
            assert_eq!(
                before_close.operations[&arena_b.epoch()].pin_bytes,
                2 * ready_weight
            );
            assert_eq!(before_close.operations[&arena_b.epoch()].bypass_bytes, 0);
            assert_eq!(
                before_close.operations[&arena_b.epoch()].self_pinned_bytes,
                2 * ready_weight
            );
            let b_pointer = b.pointer();
            arena_a.close();
            assert_eq!(a.owner().as_object().as_i64().unwrap(), 281);
            assert_eq!(b.owner().as_object().as_i64().unwrap(), 282);
            let hit = arena_b
                .resolve((1, 0), |_| panic!("sibling ready owner must hit"))
                .unwrap();
            let hit_permit = hit.inner.owner.permit.clone();
            assert_eq!(hit_permit.stats().current_bytes, ready_weight);
            assert_eq!(hit.pointer(), b_pointer);
            let snapshot = domain.snapshot();
            assert_eq!(
                snapshot.raw,
                RepresentationSnapshot {
                    calls: 4,
                    loads: 3,
                    hits: 1,
                    ..RepresentationSnapshot::default()
                }
            );
            assert_eq!(snapshot.containers, RepresentationSnapshot::default());
            assert_eq!(snapshot.members, RepresentationSnapshot::default());
            assert_representation_counter_sums(&snapshot);
            let after_close = broker.snapshot();
            assert_eq!(after_close.operations[&arena_a.epoch()].cache_bytes, 0);
            assert_eq!(
                after_close.operations[&arena_a.epoch()].pin_bytes,
                ready_weight
            );
            assert_eq!(after_close.operations[&arena_a.epoch()].bypass_bytes, 0);
            assert_eq!(
                after_close.operations[&arena_a.epoch()].self_pinned_bytes,
                ready_weight
            );
            assert_eq!(
                after_close.operations[&arena_b.epoch()],
                before_close.operations[&arena_b.epoch()]
            );
            drop(hit);
            drop(generation_one);
            drop(b);
            drop(a);
            assert_eq!(a_permit.stats().current_bytes, 0);
            assert_eq!(b_permit.stats().current_bytes, ready_weight);
            assert_eq!(generation_one_permit.stats().current_bytes, ready_weight);
            assert_eq!(hit_permit.stats().current_bytes, ready_weight);
            arena_b.close();
            assert_eq!(b_permit.stats().current_bytes, 0);
            assert_eq!(generation_one_permit.stats().current_bytes, 0);
            assert_eq!(hit_permit.stats().current_bytes, 0);
            drop(arena_a);
            drop(arena_b);
            assert_gate4_terminal(&domain, &broker, 2);
        }

        {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena_a = domain.open_arena().unwrap();
            let arena_b = domain.open_arena().unwrap();
            let id = (283, 0);
            let generation_one_id = (283, 1);
            let mut a_detail = String::with_capacity(191);
            a_detail.push_str("epoch A persistent dynamic failure");
            let a_dynamic = (a_detail.as_ptr() as usize, a_detail.capacity() as u64);
            let mut b_detail = String::with_capacity(223);
            b_detail.push_str("epoch B persistent dynamic failure");
            let b_dynamic = (b_detail.as_ptr() as usize, b_detail.capacity() as u64);
            let a = arena_a
                .resolve_object_stream(id, |_| Err(dynamic_container_failure(id, a_detail)))
                .unwrap_err();
            let b = arena_b
                .resolve_object_stream(id, |_| Err(dynamic_container_failure(id, b_detail)))
                .unwrap_err();
            let generation_one = arena_b
                .resolve_object_stream(generation_one_id, |_| {
                    Err(container_persistent(generation_one_id))
                })
                .unwrap_err();
            let (ContainerCellError::Shared(a_owner), ContainerCellError::Shared(b_owner)) =
                (&a, &b)
            else {
                panic!("persistent rows must retain shared owners")
            };
            assert!(!Arc::ptr_eq(a_owner, b_owner));
            assert_ne!(a_owner.charge_pointer(), b_owner.charge_pointer());
            assert_ne!(b.shared_pointer(), generation_one.shared_pointer());
            assert_eq!(a_owner.objstm_dynamic_allocation(), Some(a_dynamic));
            assert_eq!(b_owner.objstm_dynamic_allocation(), Some(b_dynamic));
            assert_ne!(a_dynamic.0, b_dynamic.0);
            assert_ne!(a_dynamic.1, b_dynamic.1);
            let a_weight = a_owner.retained_weight();
            let b_weight = b_owner.retained_weight();
            let ContainerCellError::Shared(generation_one_owner) = &generation_one else {
                panic!("generation-one persistent owner")
            };
            let generation_one_weight = generation_one_owner.retained_weight();
            let before_close = broker.snapshot();
            assert_eq!(
                before_close.operations[&arena_a.epoch()].cache_bytes,
                ARENA_METADATA_BYTES + CELL_METADATA_BYTES + a_weight
            );
            assert_eq!(before_close.operations[&arena_a.epoch()].pin_bytes, 0);
            assert_eq!(before_close.operations[&arena_a.epoch()].bypass_bytes, 0);
            assert_eq!(
                before_close.operations[&arena_a.epoch()].self_pinned_bytes,
                0
            );
            assert_eq!(
                before_close.operations[&arena_b.epoch()].cache_bytes,
                ARENA_METADATA_BYTES + 2 * CELL_METADATA_BYTES + b_weight + generation_one_weight
            );
            let b_pointer = b.shared_pointer();
            arena_a.close();
            let hit = arena_b
                .resolve_object_stream(id, |_| panic!("sibling negative owner must hit"))
                .unwrap_err();
            assert_eq!(hit.shared_pointer(), b_pointer);
            let ContainerCellError::Shared(hit_owner) = &hit else {
                panic!("dynamic negative hit")
            };
            assert_eq!(hit_owner.objstm_dynamic_allocation(), Some(b_dynamic));
            let snapshot = domain.snapshot();
            assert_eq!(
                snapshot.containers,
                RepresentationSnapshot {
                    calls: 4,
                    loads: 3,
                    negative_hits: 1,
                    ..RepresentationSnapshot::default()
                }
            );
            assert_eq!(snapshot.raw, RepresentationSnapshot::default());
            assert_eq!(snapshot.members, RepresentationSnapshot::default());
            assert_representation_counter_sums(&snapshot);
            let after_close = broker.snapshot();
            assert_eq!(after_close.operations[&arena_a.epoch()].cache_bytes, 0);
            assert_eq!(after_close.operations[&arena_a.epoch()].pin_bytes, 0);
            assert_eq!(
                after_close.operations[&arena_a.epoch()].bypass_bytes,
                a_weight
            );
            assert_eq!(
                after_close.operations[&arena_a.epoch()].self_pinned_bytes,
                0
            );
            assert_eq!(
                after_close.operations[&arena_b.epoch()],
                before_close.operations[&arena_b.epoch()]
            );
            drop(hit);
            drop(generation_one);
            drop(b);
            drop(a);
            arena_b.close();
            drop(arena_a);
            drop(arena_b);
            assert_gate4_terminal(&domain, &broker, 2);
        }

        {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena_a = domain.open_arena().unwrap();
            let arena_b = domain.open_arena().unwrap();
            let id = (284, 0);
            let a_leader = arena_a
                .inner
                .request_representation(id, Representation::DeclaredObjStmContainer)
                .unwrap();
            let a_follower = arena_a
                .inner
                .request_representation(id, Representation::DeclaredObjStmContainer)
                .unwrap();
            let b_leader = arena_b
                .inner
                .request_representation(id, Representation::DeclaredObjStmContainer)
                .unwrap();
            let b_follower = arena_b
                .inner
                .request_representation(id, Representation::DeclaredObjStmContainer)
                .unwrap();
            let a = a_leader
                .resolve_object_stream(|_| Err(container_transient(id)))
                .unwrap_err();
            let b = b_leader
                .resolve_object_stream(|_| Err(container_transient(id)))
                .unwrap_err();
            assert_ne!(a.shared_pointer(), b.shared_pointer());
            let (ContainerCellError::Shared(a_owner), ContainerCellError::Shared(b_owner)) =
                (&a, &b)
            else {
                panic!("flight rows must retain shared owners")
            };
            assert_ne!(a_owner.charge_pointer(), b_owner.charge_pointer());
            let a_weight = a_owner.retained_weight();
            let b_weight = b_owner.retained_weight();
            assert_eq!(a_owner.charge_bytes(), Some(a_weight));
            assert_eq!(b_owner.charge_bytes(), Some(b_weight));
            let before_close = broker.snapshot();
            assert_eq!(
                before_close.operations[&arena_a.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            assert_eq!(before_close.operations[&arena_a.epoch()].pin_bytes, 0);
            assert_eq!(
                before_close.operations[&arena_a.epoch()].bypass_bytes,
                CELL_METADATA_BYTES + a_weight
            );
            assert_eq!(
                before_close.operations[&arena_a.epoch()].self_pinned_bytes,
                0
            );
            assert_eq!(
                before_close.operations[&arena_b.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            assert_eq!(before_close.operations[&arena_b.epoch()].pin_bytes, 0);
            assert_eq!(
                before_close.operations[&arena_b.epoch()].bypass_bytes,
                CELL_METADATA_BYTES + b_weight
            );
            assert_eq!(
                before_close.operations[&arena_b.epoch()].self_pinned_bytes,
                0
            );
            arena_a.close();
            let a_attached = a_follower
                .resolve_object_stream(|_| panic!("A attached flight owner must survive close"))
                .unwrap_err();
            let b_attached = b_follower
                .resolve_object_stream(|_| panic!("B attached flight owner must share"))
                .unwrap_err();
            assert_eq!(a.shared_pointer(), a_attached.shared_pointer());
            assert_eq!(b.shared_pointer(), b_attached.shared_pointer());
            let after_close = broker.snapshot();
            assert_eq!(after_close.operations[&arena_a.epoch()].cache_bytes, 0);
            assert_eq!(after_close.operations[&arena_a.epoch()].pin_bytes, 0);
            assert_eq!(
                after_close.operations[&arena_a.epoch()].bypass_bytes,
                a_weight
            );
            assert_eq!(
                after_close.operations[&arena_a.epoch()].self_pinned_bytes,
                0
            );
            assert_eq!(
                after_close.operations[&arena_b.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            assert_eq!(after_close.operations[&arena_b.epoch()].pin_bytes, 0);
            assert_eq!(
                after_close.operations[&arena_b.epoch()].bypass_bytes,
                b_weight
            );
            assert_eq!(
                after_close.operations[&arena_b.epoch()].self_pinned_bytes,
                0
            );
            let later = arena_b
                .resolve_object_stream(id, |_| Err(container_transient(id)))
                .unwrap_err();
            assert_ne!(later.shared_pointer(), b.shared_pointer());
            let ContainerCellError::Shared(later_owner) = &later else {
                panic!("later flight owner")
            };
            assert_ne!(later_owner.charge_pointer(), b_owner.charge_pointer());
            assert_eq!(
                later_owner.charge_bytes(),
                Some(later_owner.retained_weight())
            );
            let snapshot = domain.snapshot();
            assert_eq!(
                snapshot.containers,
                RepresentationSnapshot {
                    calls: 5,
                    loads: 3,
                    waits: 2,
                    transient_shares: 2,
                    ..RepresentationSnapshot::default()
                }
            );
            assert_eq!(snapshot.raw, RepresentationSnapshot::default());
            assert_eq!(snapshot.members, RepresentationSnapshot::default());
            assert_representation_counter_sums(&snapshot);
            let with_later = broker.snapshot();
            assert_eq!(
                with_later.operations[&arena_b.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            assert_eq!(with_later.operations[&arena_b.epoch()].pin_bytes, 0);
            assert_eq!(
                with_later.operations[&arena_b.epoch()].bypass_bytes,
                b_weight + later_owner.retained_weight()
            );
            assert_eq!(with_later.operations[&arena_b.epoch()].self_pinned_bytes, 0);
            drop(later);
            drop(b_attached);
            drop(a_attached);
            drop(b);
            drop(a);
            arena_b.close();
            drop(arena_a);
            drop(arena_b);
            assert_gate4_terminal(&domain, &broker, 2);
        }

        {
            let broker = broker();
            let domain = ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(0));
            let arena_a = domain.open_arena().unwrap();
            let arena_b = domain.open_arena().unwrap();
            let reader_a = reader(291);
            let reader_b = reader(292);
            let a = arena_a
                .resolve((1, 0), |permit| load(&reader_a, (1, 0), permit))
                .unwrap();
            let b = arena_b
                .resolve((1, 0), |permit| load(&reader_b, (1, 0), permit))
                .unwrap();
            assert_ne!(a.pointer(), b.pointer());
            assert_ne!(a.owner().charge_pointer(), b.owner().charge_pointer());
            let a_weight = a.owner().retained_bytes();
            let b_weight = b.owner().retained_bytes();
            let a_permit = a.inner.owner.permit.clone();
            let b_permit = b.inner.owner.permit.clone();
            assert_eq!(a_permit.stats().current_bytes, a_weight);
            assert_eq!(b_permit.stats().current_bytes, b_weight);
            let before_close = broker.snapshot();
            assert_eq!(
                before_close.operations[&arena_a.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            assert_eq!(before_close.operations[&arena_a.epoch()].pin_bytes, 0);
            assert_eq!(
                before_close.operations[&arena_a.epoch()].bypass_bytes,
                a_weight
            );
            assert_eq!(
                before_close.operations[&arena_a.epoch()].self_pinned_bytes,
                a_weight
            );
            assert_eq!(
                before_close.operations[&arena_b.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            assert_eq!(before_close.operations[&arena_b.epoch()].pin_bytes, 0);
            assert_eq!(
                before_close.operations[&arena_b.epoch()].bypass_bytes,
                b_weight
            );
            assert_eq!(
                before_close.operations[&arena_b.epoch()].self_pinned_bytes,
                b_weight
            );
            let b_pointer = b.pointer();
            let b_owner = Arc::clone(b.owner());
            arena_a.close();
            assert_eq!(a.owner().as_object().as_i64().unwrap(), 291);
            assert_eq!(b.owner().as_object().as_i64().unwrap(), 292);
            let after_close = broker.snapshot();
            assert_eq!(after_close.operations[&arena_a.epoch()].cache_bytes, 0);
            assert_eq!(after_close.operations[&arena_a.epoch()].pin_bytes, 0);
            assert_eq!(
                after_close.operations[&arena_a.epoch()].bypass_bytes,
                a_weight
            );
            assert_eq!(
                after_close.operations[&arena_a.epoch()].self_pinned_bytes,
                a_weight
            );
            assert_eq!(
                after_close.operations[&arena_b.epoch()],
                before_close.operations[&arena_b.epoch()]
            );
            drop(b);
            let owner_only = broker.snapshot();
            assert_eq!(b_permit.stats().current_bytes, b_weight);
            assert_eq!(
                owner_only.operations[&arena_b.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            assert_eq!(owner_only.operations[&arena_b.epoch()].pin_bytes, 0);
            assert_eq!(
                owner_only.operations[&arena_b.epoch()].bypass_bytes,
                b_weight
            );
            assert_eq!(
                owner_only.operations[&arena_b.epoch()].self_pinned_bytes,
                b_weight
            );
            let reloaded = arena_b
                .resolve((1, 0), |permit| load(&reader_b, (1, 0), permit))
                .unwrap();
            assert_ne!(reloaded.pointer(), b_pointer);
            assert!(!Arc::ptr_eq(reloaded.owner(), &b_owner));
            assert_ne!(reloaded.owner().charge_pointer(), b_owner.charge_pointer());
            let reload_weight = reloaded.owner().retained_bytes();
            let reload_permit = reloaded.inner.owner.permit.clone();
            assert_eq!(reload_permit.stats().current_bytes, reload_weight);
            let snapshot = domain.snapshot();
            assert_eq!(
                snapshot.raw,
                RepresentationSnapshot {
                    calls: 3,
                    loads: 3,
                    bypasses: 3,
                    ..RepresentationSnapshot::default()
                }
            );
            assert_eq!(snapshot.containers, RepresentationSnapshot::default());
            assert_eq!(snapshot.members, RepresentationSnapshot::default());
            assert_representation_counter_sums(&snapshot);
            let reloaded_ownership = broker.snapshot();
            assert_eq!(
                reloaded_ownership.operations[&arena_b.epoch()].cache_bytes,
                ARENA_METADATA_BYTES
            );
            assert_eq!(reloaded_ownership.operations[&arena_b.epoch()].pin_bytes, 0);
            assert_eq!(
                reloaded_ownership.operations[&arena_b.epoch()].bypass_bytes,
                b_weight + reload_weight
            );
            assert_eq!(
                reloaded_ownership.operations[&arena_b.epoch()].self_pinned_bytes,
                b_weight + reload_weight
            );
            drop(reloaded);
            assert_eq!(reload_permit.stats().current_bytes, 0);
            drop(b_owner);
            assert_eq!(b_permit.stats().current_bytes, 0);
            drop(a);
            assert_eq!(a_permit.stats().current_bytes, 0);
            arena_b.close();
            drop(arena_a);
            drop(arena_b);
            assert_gate4_terminal(&domain, &broker, 2);
        }
    }

    #[test]
    fn every_leader_phase_cancel_and_panic_acknowledges_before_fifo_successor() {
        let phases = [
            LeaderPhase::BeforeRequest,
            LeaderPhase::QueuedBeforeWait,
            LeaderPhase::Granted,
            LeaderPhase::BeforeLoader,
            LeaderPhase::AfterLoaderResult,
            LeaderPhase::ReconciledBeforePublication,
        ];
        for phase in phases {
            for action in [PhaseAction::Continue, PhaseAction::Panic] {
                let broker = broker();
                let domain = ObjectCellDomain::new(
                    broker.clone(),
                    ObjectCellConfig::scaled(32 * 1024 * 1024),
                );
                let arena = domain.open_arena().unwrap();
                let (stream_reader, _, container, _) = object_stream_reader();
                let (oracle_reader, _, oracle_container, _) = object_stream_reader();
                assert_eq!(oracle_container, container);
                let oracle_permit = ScalarResolutionPermit::new(LOADER_ESTIMATE_BYTES);
                let oracle_owner = oracle_reader
                    .prepare_object_stream_with_permit(oracle_container, &oracle_permit)
                    .unwrap();
                let expected_permit_peak = oracle_permit.stats().peak_bytes;
                assert_eq!(
                    oracle_permit.stats().current_bytes,
                    oracle_owner.retained_bytes()
                );
                drop(oracle_owner);
                assert_eq!(oracle_permit.stats().current_bytes, 0);
                let leader = arena
                    .inner
                    .request_representation(container, Representation::DeclaredObjStmContainer)
                    .unwrap();
                let leader_cancel = leader.cancellation_handle();
                let oldest = arena
                    .inner
                    .request_representation(container, Representation::DeclaredObjStmContainer)
                    .unwrap();
                let later = arena
                    .inner
                    .request_representation(container, Representation::DeclaredObjStmContainer)
                    .unwrap();

                let blocker = if phase == LeaderPhase::QueuedBeforeWait {
                    let operation = broker.register_operation().unwrap();
                    let pending = operation
                        .request(
                            Lane::Normal {
                                completion_reserve: 0,
                            },
                            LOADER_ESTIMATE_BYTES,
                        )
                        .unwrap();
                    Some((operation, pending.wait().unwrap()))
                } else {
                    None
                };

                let (phase_tx, phase_rx) = mpsc::sync_channel(0);
                let (action_tx, action_rx) = mpsc::sync_channel(0);
                domain.set_leader_phase_hooks(Arc::new(PausingLeaderPhaseHooks {
                    target: phase,
                    armed: AtomicBool::new(true),
                    entered: phase_tx,
                    action: Mutex::new(action_rx),
                }));
                let (wait_tx, wait_rx) = mpsc::sync_channel(0);
                let wait_hooks = Arc::new(CountingHooks {
                    adds: AtomicU64::new(0),
                    removes: AtomicU64::new(0),
                    entered: Mutex::new(Some(wait_tx)),
                    seen: Mutex::new(Vec::new()),
                });
                domain.set_wait_hooks(wait_hooks.clone());

                let leader_evidence = Arc::new(Mutex::new(None));
                let leader_evidence_for_load = Arc::clone(&leader_evidence);
                let leader_reader = Arc::clone(&stream_reader);
                let leader_join = thread::spawn(move || {
                    leader.resolve_object_stream(|permit| {
                        let result = leader_reader
                            .prepare_object_stream_with_permit(container, permit)
                            .map_err(|error| {
                                CellLoadError::objstm(crate::objstm_failures::classify(
                                    container, error,
                                ))
                            });
                        if let Ok(owner) = &result {
                            *lock(&leader_evidence_for_load) =
                                Some((permit.clone(), owner.retained_bytes()));
                        }
                        result
                    })
                });
                phase_rx.recv().unwrap();

                let snapshot = domain.snapshot();
                assert_eq!(snapshot.loading, 1, "{phase:?}");
                assert_eq!(snapshot.ready, 0, "{phase:?}");
                assert_eq!(snapshot.negative, 0, "{phase:?}");
                assert_eq!(snapshot.cells, 1, "{phase:?}");
                assert_eq!(snapshot.live_interests, 3, "{phase:?}");
                assert_eq!(snapshot.external_pins, 0, "{phase:?}");
                assert_eq!(snapshot.cache_bytes, CELL_METADATA_BYTES, "{phase:?}");
                assert_representation_counter_sums(&snapshot);
                assert_eq!(snapshot.raw, RepresentationSnapshot::default(), "{phase:?}");
                assert_eq!(
                    snapshot.members,
                    RepresentationSnapshot::default(),
                    "{phase:?}"
                );
                let broker_phase = broker.snapshot();
                assert_eq!(broker_phase.completion_reserve_bytes, 0, "{phase:?}");
                assert_eq!(broker_phase.oversize_bytes, 0, "{phase:?}");
                assert_eq!(broker_phase.oversize_owners, 0, "{phase:?}");
                let operation = broker_phase.operations[&arena.epoch()].clone();
                match phase {
                    LeaderPhase::BeforeRequest => {
                        assert_eq!(snapshot.containers.loads, 0);
                        assert_eq!(operation.queued, 0);
                        assert_eq!(operation.in_flight, 0);
                        assert!(arena.active_container_permit_stats(container).is_none());
                        assert!(lock(&leader_evidence).is_none());
                    }
                    LeaderPhase::QueuedBeforeWait => {
                        assert_eq!(snapshot.containers.loads, 0);
                        assert_eq!(operation.queued, 1);
                        assert_eq!(operation.in_flight, 0);
                        assert!(arena.active_container_permit_stats(container).is_none());
                        assert!(lock(&leader_evidence).is_none());
                    }
                    LeaderPhase::Granted => {
                        assert_eq!(snapshot.containers.loads, 0);
                        assert_eq!(operation.queued, 0);
                        assert_eq!(operation.in_flight, 1);
                        assert!(arena.active_container_permit_stats(container).is_none());
                        assert!(lock(&leader_evidence).is_none());
                    }
                    LeaderPhase::BeforeLoader => {
                        assert_eq!(snapshot.containers.loads, 1);
                        assert_eq!(operation.queued, 0);
                        assert_eq!(operation.in_flight, 1);
                        let stats = arena
                            .active_container_permit_stats(container)
                            .expect("fresh loader permit");
                        assert_eq!(stats.limit_bytes, LOADER_ESTIMATE_BYTES);
                        assert_eq!(stats.current_bytes, 0);
                        assert_eq!(stats.peak_bytes, 0);
                        assert_eq!(stats.reservations, 0);
                        assert!(!stats.cancelled);
                        assert!(!stats.closed);
                        assert!(lock(&leader_evidence).is_none());
                    }
                    LeaderPhase::AfterLoaderResult => {
                        assert_eq!(snapshot.containers.loads, 1);
                        assert_eq!(operation.queued, 0);
                        assert_eq!(operation.in_flight, 1);
                        let evidence = lock(&leader_evidence);
                        let (permit, retained) = evidence.as_ref().expect("loader evidence");
                        let stats = permit.stats();
                        assert_eq!(arena.active_container_permit_stats(container), Some(stats));
                        assert_eq!(stats.limit_bytes, LOADER_ESTIMATE_BYTES);
                        assert_eq!(stats.current_bytes, *retained);
                        assert_eq!(stats.peak_bytes, expected_permit_peak);
                        assert!(!stats.cancelled);
                        assert!(!stats.closed);
                    }
                    LeaderPhase::ReconciledBeforePublication => {
                        assert_eq!(snapshot.containers.loads, 1);
                        assert_eq!(operation.queued, 0);
                        assert_eq!(operation.in_flight, 0);
                        assert!(arena.active_container_permit_stats(container).is_none());
                        let evidence = lock(&leader_evidence);
                        let (permit, retained) = evidence.as_ref().expect("reconciled evidence");
                        let stats = permit.stats();
                        assert_eq!(stats.current_bytes, *retained);
                        assert_eq!(stats.peak_bytes, expected_permit_peak);
                        assert_eq!(operation.bypass_bytes, *retained);
                        assert_eq!(
                            operation.cache_bytes,
                            ARENA_METADATA_BYTES + CELL_METADATA_BYTES
                        );
                        assert_eq!(operation.pin_bytes, 0);
                    }
                }

                match action {
                    PhaseAction::Continue => {
                        leader_cancel.cancel();
                        action_tx.send(PhaseAction::Continue).unwrap();
                        assert!(leader_join.join().unwrap().is_err(), "{phase:?}");
                    }
                    PhaseAction::Panic => {
                        action_tx.send(PhaseAction::Panic).unwrap();
                        assert!(leader_join.join().is_err(), "{phase:?}");
                    }
                }
                let acknowledged = broker.snapshot().operations[&arena.epoch()].clone();
                assert_eq!(acknowledged.queued, 0, "{phase:?}");
                assert_eq!(acknowledged.in_flight, 0, "{phase:?}");
                assert_eq!(acknowledged.bypass_bytes, 0, "{phase:?}");
                assert_eq!(acknowledged.pin_bytes, 0, "{phase:?}");
                if let Some((permit, _)) = lock(&leader_evidence).as_ref() {
                    let stats = permit.stats();
                    assert_eq!(stats.current_bytes, 0, "{phase:?}");
                    assert_eq!(stats.peak_bytes, expected_permit_peak, "{phase:?}");
                    assert!(stats.reservations > 0, "{phase:?}");
                }
                drop(blocker);

                let later_join = thread::spawn(move || {
                    later.resolve_object_stream(|_| panic!("later waiter must not lead"))
                });
                wait_rx.recv().unwrap();
                let active = Arc::new(AtomicUsize::new(0));
                let peak = Arc::new(AtomicUsize::new(0));
                let active_oldest = Arc::clone(&active);
                let peak_oldest = Arc::clone(&peak);
                let successor_reader = Arc::clone(&stream_reader);
                let oldest_join = thread::spawn(move || {
                    oldest.resolve_object_stream(|permit| {
                        let now = active_oldest.fetch_add(1, Ordering::AcqRel) + 1;
                        peak_oldest.fetch_max(now, Ordering::AcqRel);
                        let result = successor_reader
                            .prepare_object_stream_with_permit(container, permit)
                            .map_err(|error| {
                                CellLoadError::objstm(crate::objstm_failures::classify(
                                    container, error,
                                ))
                            });
                        active_oldest.fetch_sub(1, Ordering::AcqRel);
                        result
                    })
                });
                let oldest_pin = oldest_join.join().unwrap().unwrap();
                let later_pin = later_join.join().unwrap().unwrap();
                assert!(std::ptr::eq(
                    oldest_pin.as_object_stream(),
                    later_pin.as_object_stream()
                ));
                assert_eq!(peak.load(Ordering::Acquire), 1);
                assert_eq!(wait_hooks.adds.load(Ordering::Relaxed), 1);
                assert_eq!(wait_hooks.removes.load(Ordering::Relaxed), 1);

                let sequential = arena
                    .resolve_object_stream(container, |_| panic!("successor must be cached"))
                    .unwrap();
                assert!(std::ptr::eq(
                    oldest_pin.as_object_stream(),
                    sequential.as_object_stream()
                ));
                let terminal = domain.snapshot();
                let old_loads = u64::from(matches!(
                    phase,
                    LeaderPhase::BeforeLoader
                        | LeaderPhase::AfterLoaderResult
                        | LeaderPhase::ReconciledBeforePublication
                ));
                assert_eq!(terminal.containers.calls, 4, "{phase:?}");
                assert_eq!(terminal.containers.loads, old_loads + 1, "{phase:?}");
                assert_eq!(terminal.containers.waits, 2, "{phase:?}");
                assert_eq!(terminal.containers.hits, 1, "{phase:?}");
                assert_eq!(terminal.containers.negative_hits, 0, "{phase:?}");
                assert_eq!(terminal.containers.transient_shares, 0, "{phase:?}");
                assert_eq!(terminal.containers.bypasses, 0, "{phase:?}");
                assert_eq!(terminal.containers.evictions, 0, "{phase:?}");
                assert_eq!(terminal.containers.cancellations, 1, "{phase:?}");
                assert_eq!(terminal.raw, RepresentationSnapshot::default(), "{phase:?}");
                assert_eq!(
                    terminal.members,
                    RepresentationSnapshot::default(),
                    "{phase:?}"
                );
                assert_representation_counter_sums(&terminal);

                let (retained, retained_permit, charge) = sequential.retained_evidence();
                assert_eq!(retained, sequential.as_object_stream().retained_bytes());
                assert_eq!(charge, retained);
                let retained_stats = retained_permit.stats();
                assert_eq!(retained_stats.current_bytes, retained);
                assert_eq!(retained_stats.peak_bytes, expected_permit_peak);
                let cached = arena
                    .container_retained_evidence(container)
                    .expect("cached container evidence");
                assert_eq!(cached.0, retained);
                assert_eq!(cached.1.stats(), retained_stats);
                assert_eq!(cached.2, retained);

                drop(sequential);
                drop(later_pin);
                drop(oldest_pin);
                arena.close();
                drop(arena);
                assert_eq!(retained_permit.stats().current_bytes, 0, "{phase:?}");
                let drained_cells = domain.snapshot();
                assert_eq!(drained_cells.arenas, 0, "{phase:?}");
                assert_eq!(drained_cells.cells, 0, "{phase:?}");
                assert_eq!(drained_cells.loading, 0, "{phase:?}");
                assert_eq!(drained_cells.ready, 0, "{phase:?}");
                assert_eq!(drained_cells.negative, 0, "{phase:?}");
                assert_eq!(drained_cells.live_interests, 0, "{phase:?}");
                assert_eq!(drained_cells.external_pins, 0, "{phase:?}");
                assert_eq!(drained_cells.cache_bytes, 0, "{phase:?}");
                assert_representation_counter_sums(&drained_cells);
                let drained = broker.snapshot();
                assert_eq!(drained.aggregate_bytes, 0, "{phase:?}");
                assert_eq!(drained.normal_payload_bytes, 0, "{phase:?}");
                assert_eq!(drained.normal_in_flight_estimate_bytes, 0, "{phase:?}");
                assert_eq!(drained.metadata_bytes, 0, "{phase:?}");
                assert_eq!(drained.completion_reserve_bytes, 0, "{phase:?}");
                assert_eq!(drained.oversize_bytes, 0, "{phase:?}");
                assert_eq!(drained.oversize_owners, 0, "{phase:?}");
                assert_eq!(drained.cache_bytes, 0, "{phase:?}");
                assert_eq!(drained.pin_bytes, 0, "{phase:?}");
                assert_eq!(drained.bypass_bytes, 0, "{phase:?}");
                assert_eq!(drained.queued, 0, "{phase:?}");
                assert_eq!(drained.in_flight, 0, "{phase:?}");
                assert_eq!(drained.live_request_records, 0, "{phase:?}");
                assert_eq!(drained.reservation_metadata_bytes, 0, "{phase:?}");
                assert_eq!(drained.active_operations, 0, "{phase:?}");
                assert!(drained.operations.is_empty(), "{phase:?}");
            }
        }
    }

    #[test]
    fn inactive_wait_hooks_are_balanced_and_cannot_change_outcome() {
        let domain = ObjectCellDomain::new(broker(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let hooks = Arc::new(CountingHooks {
            adds: AtomicU64::new(0),
            removes: AtomicU64::new(0),
            entered: Mutex::new(Some(entered_tx)),
            seen: Mutex::new(Vec::new()),
        });
        domain.set_wait_hooks(hooks.clone());
        let arena = domain.open_arena().unwrap();
        let leader = arena.request((1, 0)).unwrap();
        let waiter = arena.request((1, 0)).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = mpsc::sync_channel(0);
        let reader = reader(66);
        let leader_join = thread::spawn(move || {
            leader.resolve(|permit| {
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                load(&reader, (1, 0), permit)
            })
        });
        started_rx.recv().unwrap();
        let waiter_join =
            thread::spawn(move || waiter.resolve(|_| panic!("waiter must reuse leader value")));
        entered_rx.recv().unwrap();
        finish_tx.send(()).unwrap();
        let leader_pin = leader_join.join().unwrap().unwrap();
        let waiter_pin = waiter_join.join().unwrap().unwrap();
        assert_eq!(leader_pin.pointer(), waiter_pin.pointer());
        assert_eq!(hooks.adds.load(Ordering::Relaxed), 1);
        assert_eq!(hooks.removes.load(Ordering::Relaxed), 1);
        assert_eq!(&*lock(&hooks.seen), &[(1, 2)]);
    }

    #[test]
    fn close_cancels_loading_and_queued_flights_without_polling() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let first = arena.request((1, 0)).unwrap();
        let second = arena.request((2, 0)).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = mpsc::sync_channel(0);
        let first_reader = reader(1);
        let first_join = thread::spawn(move || {
            first.resolve(|permit| {
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                load(&first_reader, (1, 0), permit)
            })
        });
        started_rx.recv().unwrap();
        let (queued_tx, queued_rx) = mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);
        domain.set_leader_phase_hooks(Arc::new(PausingLeaderPhaseHooks {
            target: LeaderPhase::QueuedBeforeWait,
            armed: AtomicBool::new(true),
            entered: queued_tx,
            action: Mutex::new(resume_rx),
        }));
        let second_join = thread::spawn(move || {
            second.resolve(|_| panic!("queued loader must be cancelled before invocation"))
        });
        queued_rx.recv().unwrap();
        assert_eq!(broker.snapshot().queued, 1);
        arena.close();
        resume_tx.send(PhaseAction::Continue).unwrap();
        finish_tx.send(()).unwrap();
        assert!(first_join.join().unwrap().is_err());
        assert!(second_join.join().unwrap().is_err());
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.cells, 0);
        assert_eq!(snapshot.live_interests, 0);
        assert_eq!(snapshot.arenas, 0);
        drop(arena);
        let broker_snapshot = broker.snapshot();
        assert_eq!(broker_snapshot.aggregate_bytes, 0);
        assert_eq!(broker_snapshot.active_operations, 0);
        assert!(!broker_snapshot.invariant_failed);
    }

    #[test]
    fn close_keeps_external_owner_readable_until_last_pin_drops() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let object_reader = reader(88);
        let pin = arena
            .resolve((1, 0), |permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        arena
            .resolve((7, 0), |_| {
                Err(CellLoadError::new(
                    AccessError::typed((7, 0), AccessKind::Backend, "stable close error"),
                    NegativeDisposition::Persistent,
                ))
            })
            .unwrap_err();
        arena.close();
        assert_eq!(domain.snapshot().cells, 0);
        assert_eq!(pin.owner().as_object().as_i64().unwrap(), 88);
        assert!(broker.snapshot().pin_bytes > 0);
        drop(pin);
        drop(arena);
        let snapshot = broker.snapshot();
        assert_eq!(snapshot.aggregate_bytes, 0);
        assert_eq!(snapshot.active_operations, 0);
    }

    #[test]
    fn close_phase_replacement_linearizes_before_a_delayed_ready_pin() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let object_reader = reader(89);
        let initial = arena
            .resolve((1, 0), |permit| load(&object_reader, (1, 0), permit))
            .unwrap();
        let owner = Arc::clone(initial.owner());
        drop(initial);
        let cell = lock(&domain.inner.state)
            .cells
            .values()
            .next()
            .cloned()
            .unwrap();

        let hooks = Arc::new(PausingCloseHooks {
            entered: Barrier::new(2),
            release: Barrier::new(2),
        });
        domain.set_close_hooks(hooks.clone());
        let closing_arena = arena.clone();
        let close_join = thread::spawn(move || closing_arena.close());
        hooks.entered.wait();

        let error = arena
            .inner
            .pin(&cell, Arc::clone(&owner))
            .expect_err("phase replacement must reject a previously observed ready owner")
            .into_access();
        hooks.release.wait();
        close_join.join().unwrap();
        assert_eq!(error.kind, AccessKind::Backend);
        let held = broker.snapshot();
        assert_eq!(held.cache_bytes, 0);
        assert_eq!(held.pin_bytes, 0);
        assert_eq!(held.bypass_bytes, owner.retained_bytes());
        assert_eq!(held.operations[&arena.epoch()].self_pinned_bytes, 0);
        drop(owner);
        drop(cell);
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn c1_dependency_blocks_member_admission_and_cleans_with_balanced_edges() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (hooks, entered, action) =
            dependency_hook(&domain, DependencyEventKind::InsideChildResolve);
        let request = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let broker_before = broker.snapshot();
        let join = thread::spawn(move || request.resolve_dependency_synthetic(container));
        let blocked = entered.recv().unwrap();
        assert_eq!(blocked.kind, DependencyEventKind::InsideChildResolve);

        let cells = domain.snapshot();
        assert_eq!(cells.dependency_cancellations, 1);
        assert_eq!(cells.dependency_pins, 0);
        assert_eq!(cells.dependency_edges, 1);
        assert_eq!(cells.dependency_leaders, 1);
        assert_eq!(cells.dependency_edge_adds, 1);
        assert_eq!(cells.dependency_edge_removes, 0);
        assert_eq!(cells.members.loads, 0);
        let broker_blocked = broker.snapshot();
        assert_eq!(broker_blocked, broker_before);
        assert_eq!(
            lock(&hooks.events)
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                DependencyEventKind::BeforeChildInstall,
                DependencyEventKind::EdgeAdded,
                DependencyEventKind::InsideChildResolve,
            ]
        );

        action.send(PhaseAction::Continue).unwrap();
        join.join().unwrap().unwrap();
        let finished = domain.snapshot();
        assert_eq!(finished.dependency_cancellations, 0);
        assert_eq!(finished.dependency_pins, 0);
        assert_eq!(finished.dependency_edges, 0);
        assert_eq!(finished.dependency_leaders, 0);
        assert_eq!(finished.dependency_edge_adds, 1);
        assert_eq!(finished.dependency_edge_removes, 1);
        assert_eq!(finished.dependency_cleanup_acks, 1);
        assert_eq!(finished.members.loads, 0);
        let events = lock(&hooks.events);
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec![
                DependencyEventKind::BeforeChildInstall,
                DependencyEventKind::EdgeAdded,
                DependencyEventKind::InsideChildResolve,
                DependencyEventKind::ChildResolved,
                DependencyEventKind::PinInstalled,
                DependencyEventKind::BeforeMemberAdmission,
                DependencyEventKind::CleanupDetached,
                DependencyEventKind::EdgeRemoved,
                DependencyEventKind::CleanupAcknowledged,
            ]
        );
        assert!(events
            .windows(2)
            .all(|window| window[0].ordinal < window[1].ordinal));
        assert!(hooks.lock_probes.load(Ordering::Relaxed) >= 8);
        drop(events);
        arena.close();
        drop(arena);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn c1_active_leader_cleanup_ack_precedes_exactly_one_fifo_successor() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (_leader_hooks, entered, action) =
            dependency_hook(&domain, DependencyEventKind::InsideChildResolve);
        let leader = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let leader_cell = Arc::clone(&leader.cell);
        let leader_cancel = leader.cancellation_handle();
        let first_follower = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let first_slot = first_follower.slot;
        let first_ordinal = first_follower.ordinal;
        let later_follower = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let later_cancel = later_follower.cancellation_handle();
        let leader_join = thread::spawn(move || leader.resolve_dependency_synthetic(container));
        let first_join =
            thread::spawn(move || first_follower.resolve_dependency_synthetic(container));
        let later_join =
            thread::spawn(move || later_follower.resolve_dependency_synthetic(container));
        let old = entered.recv().unwrap().token;

        let (hooks, successor_entered, successor_action) =
            dependency_hook(&domain, DependencyEventKind::SuccessorElected);
        {
            let state = lock(&leader_cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("active dependency leader must remain loading")
            };
            loading.cancellation.store(true, Ordering::Release);
        }
        let cancel_join = thread::spawn(move || leader_cancel.cancel());
        action.send(PhaseAction::Continue).unwrap();
        let successor_event = successor_entered.recv().unwrap();
        let successor = successor_event
            .successor
            .expect("successor event carries selected identity");
        assert_eq!(successor_event.token, old);
        assert_eq!(successor.generation, old.generation + 1);
        assert_eq!(successor.leader_slot, first_slot);
        assert_eq!(successor.interest_ordinal, first_ordinal);
        assert!(!lock(&hooks.events).iter().any(|event| {
            event.token.generation == successor.generation
                && event.kind == DependencyEventKind::BeforeChildInstall
        }));
        later_cancel.cancel();
        successor_action.send(PhaseAction::Continue).unwrap();
        cancel_join.join().unwrap();
        assert!(leader_join.join().unwrap().is_err());
        first_join.join().unwrap().unwrap();
        assert!(later_join.join().unwrap().is_err());

        let events = lock(&hooks.events);
        let cleanup_ordinal = events
            .iter()
            .find(|event| {
                event.kind == DependencyEventKind::CleanupAcknowledged && event.token == old
            })
            .expect("active leader cancellation must acknowledge dependency cleanup")
            .ordinal;
        let successors: Vec<_> = events
            .iter()
            .filter(|event| {
                event.kind == DependencyEventKind::SuccessorElected && event.token == old
            })
            .collect();
        assert_eq!(successors.len(), 1);
        assert!(cleanup_ordinal < successors[0].ordinal);
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.dependency_edges, 0);
        assert_eq!(snapshot.dependency_leaders, 0);
        assert_eq!(snapshot.dependency_edge_adds, 2);
        assert_eq!(snapshot.dependency_edge_removes, 2);
        assert_eq!(snapshot.dependency_cleanup_acks, 2);
        assert_eq!(snapshot.members.loads, 0);
        drop(events);
        drop(leader_cell);
        arena.close();
        drop(arena);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn c1_follower_cancellation_never_takes_leader_dependency() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (hooks, entered, action) =
            dependency_hook(&domain, DependencyEventKind::InsideChildResolve);
        let leader = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let follower = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let follower_cancel = follower.cancellation_handle();
        let leader_join = thread::spawn(move || leader.resolve_dependency_synthetic(container));
        entered.recv().unwrap();

        follower_cancel.cancel();
        let paused = domain.snapshot();
        assert_eq!(paused.dependency_cancellations, 1);
        assert_eq!(paused.dependency_edges, 1);
        assert_eq!(paused.dependency_leaders, 1);
        assert_eq!(paused.dependency_cleanup_acks, 0);
        assert!(!lock(&hooks.events)
            .iter()
            .any(|event| event.kind == DependencyEventKind::CleanupAcknowledged));
        drop(follower);

        action.send(PhaseAction::Continue).unwrap();
        leader_join.join().unwrap().unwrap();
        let snapshot = domain.snapshot();
        assert_eq!(snapshot.dependency_edges, 0);
        assert_eq!(snapshot.dependency_leaders, 0);
        assert_eq!(snapshot.dependency_edge_adds, 1);
        assert_eq!(snapshot.dependency_edge_removes, 1);
        assert_eq!(snapshot.dependency_cleanup_acks, 1);
        arena.close();
        drop(arena);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn c1_panic_and_close_detach_dependency_resources_outside_locks() {
        for target in [
            DependencyEventKind::BeforeChildInstall,
            DependencyEventKind::EdgeAdded,
            DependencyEventKind::InsideChildResolve,
            DependencyEventKind::ChildResolved,
            DependencyEventKind::PinInstalled,
            DependencyEventKind::BeforeMemberAdmission,
            DependencyEventKind::CleanupDetached,
            DependencyEventKind::EdgeRemoved,
            DependencyEventKind::CleanupAcknowledged,
            DependencyEventKind::SuccessorElected,
        ] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (hooks, entered, action) = dependency_hook(&domain, target);
            let leader = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let follower = (target == DependencyEventKind::SuccessorElected).then(|| {
                arena
                    .inner
                    .request_representation(member, Representation::DeclaredObjStmMember)
                    .unwrap()
            });
            let join = thread::spawn(move || leader.resolve_dependency_synthetic(container));
            let follower_join = follower.map(|follower| {
                thread::spawn(move || follower.resolve_dependency_synthetic(container))
            });
            entered.recv().unwrap();
            action.send(PhaseAction::Panic).unwrap();
            assert!(
                join.join().is_err(),
                "hook panic did not propagate at {target:?}"
            );
            if let Some(follower_join) = follower_join {
                follower_join.join().unwrap().unwrap();
            }
            let snapshot = domain.snapshot();
            assert_eq!(snapshot.dependency_cancellations, 0);
            assert_eq!(snapshot.dependency_pins, 0);
            assert_eq!(snapshot.dependency_edges, 0);
            assert_eq!(snapshot.dependency_leaders, 0);
            assert_eq!(
                snapshot.dependency_edge_adds,
                snapshot.dependency_edge_removes
            );
            assert_eq!(
                snapshot.dependency_edge_adds,
                snapshot.dependency_edge_removes
            );
            assert!(hooks.lock_probes.load(Ordering::Relaxed) >= 1);
            arena.close();
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }
    }

    #[test]
    fn c1_close_and_exact_teardown_join_detached_cleanup_before_returning() {
        for close_arena in [true, false] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (_hooks, detached_entered, detached_action) =
                dependency_hook(&domain, DependencyEventKind::CleanupDetached);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let cell = Arc::clone(&request.cell);
            let leader_slot = request.slot;
            let leader_join =
                thread::spawn(move || request.resolve_dependency_synthetic(container));
            let detached = detached_entered.recv().unwrap();
            assert_eq!(
                lock(&cell.state).dependency_transition,
                DependencyTransition::Cleaning(detached.token)
            );

            let (wait_entered, wait_action) = dependency_wait_hook(&domain);
            if close_arena {
                let closer = arena.clone();
                let terminal_join = thread::spawn(move || closer.close());
                let (key, transition) = wait_entered.recv().unwrap();
                assert_eq!(key, cell.key);
                assert_eq!(transition, DependencyTransition::Cleaning(detached.token));
                detached_action.send(PhaseAction::Continue).unwrap();
                wait_action.send(()).unwrap();
                terminal_join.join().unwrap();
            } else {
                let inner = Arc::clone(&arena.inner);
                let teardown_cell = Arc::clone(&cell);
                let terminal_join = thread::spawn(move || {
                    inner.teardown_exact_key(
                        &teardown_cell,
                        ExactPhaseExpectation::Loading {
                            leader_slot: Some(leader_slot),
                            generation: Some(detached.token.generation),
                            require_publication: false,
                        },
                        CellControlFailure::new(
                            teardown_cell.key.id,
                            AccessKind::Backend,
                            CellControlTag::FlightCancelled,
                        ),
                        false,
                    )
                });
                let (key, transition) = wait_entered.recv().unwrap();
                assert_eq!(key, cell.key);
                assert_eq!(transition, DependencyTransition::Cleaning(detached.token));
                detached_action.send(PhaseAction::Continue).unwrap();
                wait_action.send(()).unwrap();
                let _won_exact_race = terminal_join.join().unwrap();
            }
            let _ = leader_join.join().unwrap();
            let snapshot = domain.snapshot();
            assert_eq!(snapshot.dependency_cancellations, 0);
            assert_eq!(snapshot.dependency_pins, 0);
            assert_eq!(snapshot.dependency_edges, 0);
            assert_eq!(snapshot.dependency_leaders, 0);
            assert_eq!(
                snapshot.dependency_edge_adds,
                snapshot.dependency_edge_removes
            );
            assert_eq!(snapshot.dependency_cleanup_acks, 1);
            assert!(lock(&cell.state).dependency_transition.is_idle());
            drop(cell);
            arena.close();
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }
    }

    #[test]
    fn c1_lock_depth_observer_is_caller_local_and_condvar_safe() {
        let (_broker, domain, _arena, member, container) = prepared_dependency_fixture();
        assert_eq!(caller_thread_lock_depth(), 0);
        {
            let _guard = lock(&domain.inner.state);
            assert_eq!(caller_thread_lock_depth(), 1);
        }
        assert_eq!(caller_thread_lock_depth(), 0);

        let (held_tx, held_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let inner = Arc::clone(&domain.inner);
        let holder = thread::spawn(move || {
            let _guard = lock(&inner.state);
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv().unwrap();
        assert_eq!(caller_thread_lock_depth(), 0);
        let (hooks, _entered, _action) = dependency_hook(&domain, DependencyEventKind::EdgeAdded);
        hooks.event(DependencyEvent {
            token: DependencyLeaderToken {
                key: CellKey {
                    epoch: 1,
                    id: member,
                    representation: Representation::DeclaredObjStmMember,
                },
                generation: 1,
                leader_slot: 0,
            },
            edge: DependencyEdge {
                from: CellKey {
                    epoch: 1,
                    id: member,
                    representation: Representation::DeclaredObjStmMember,
                },
                to: CellKey {
                    epoch: 1,
                    id: container,
                    representation: Representation::DeclaredObjStmContainer,
                },
            },
            kind: DependencyEventKind::BeforeChildInstall,
            ordinal: 1,
            successor: None,
        });
        assert_eq!(hooks.lock_probes.load(Ordering::Relaxed), 1);
        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn c1_checked_event_ordinal_and_install_add_boundaries_are_transactional() {
        let (_broker, domain, arena, member, container) = prepared_dependency_fixture();
        let token = DependencyLeaderToken {
            key: CellKey {
                epoch: arena.epoch(),
                id: member,
                representation: Representation::DeclaredObjStmMember,
            },
            generation: 1,
            leader_slot: 0,
        };
        let edge = DependencyEdge {
            from: token.key,
            to: CellKey {
                epoch: arena.epoch(),
                id: container,
                representation: Representation::DeclaredObjStmContainer,
            },
        };
        {
            let mut state = lock(&domain.inner.state);
            state.dependency_event_ordinal = u64::MAX - 1;
            let last = DomainInner::dependency_event_locked(
                &mut state,
                token,
                edge,
                DependencyEventKind::BeforeChildInstall,
                None,
            )
            .unwrap();
            assert_eq!(last.ordinal, u64::MAX);
            assert!(DomainInner::dependency_event_locked(
                &mut state,
                token,
                edge,
                DependencyEventKind::EdgeAdded,
                None,
            )
            .is_none());
            assert_eq!(state.dependency_event_ordinal, u64::MAX);
            assert!(state.invariant_failed);
        }
        arena.close();

        let (_broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (_hooks, entered, action) =
            dependency_hook(&domain, DependencyEventKind::BeforeChildInstall);
        let request = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let join = thread::spawn(move || request.resolve_dependency_synthetic(container));
        entered.recv().unwrap();
        {
            let mut state = lock(&domain.inner.state);
            state.dependency_edge_adds = u64::MAX;
        }
        action.send(PhaseAction::Continue).unwrap();
        assert!(join.join().unwrap().is_err());
        let state = domain.snapshot();
        assert_eq!(state.dependency_edge_adds, u64::MAX);
        assert_eq!(state.dependency_edges, 0);
        assert_eq!(state.dependency_leaders, 0);
        assert!(state.invariant_failed);
        arena.close();
    }

    #[test]
    fn c1_checked_cleanup_boundaries_never_manufacture_balance_or_successor() {
        for case in 0..6 {
            let (_broker, domain, arena, member, container) = prepared_dependency_fixture();
            let target = if case == 5 {
                DependencyEventKind::BeforeMemberAdmission
            } else {
                DependencyEventKind::InsideChildResolve
            };
            let (hooks, entered, action) = dependency_hook(&domain, target);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let cell = Arc::clone(&request.cell);
            let join = thread::spawn(move || request.resolve_dependency_synthetic(container));
            entered.recv().unwrap();
            {
                let mut state = lock(&domain.inner.state);
                match case {
                    0 => state.dependency_cancellations = 0,
                    1 => state.dependency_edges = 0,
                    2 => state.dependency_leaders = 0,
                    3 => state.dependency_edge_removes = u64::MAX,
                    4 => state.dependency_cleanup_acks = u64::MAX,
                    5 => state.dependency_pins = 0,
                    _ => unreachable!(),
                }
            }
            {
                let state = lock(&cell.state);
                let CellPhase::Loading(loading) = &state.phase else {
                    panic!("boundary dependency must remain loading")
                };
                loading.cancellation.store(true, Ordering::Release);
            }
            action.send(PhaseAction::Continue).unwrap();
            assert!(
                join.join().unwrap().is_err(),
                "boundary case {case} succeeded"
            );
            let snapshot = domain.snapshot();
            assert!(snapshot.invariant_failed);
            assert!(
                snapshot.dependency_edges != 0
                    || snapshot.dependency_leaders != 0
                    || snapshot.dependency_edge_adds != snapshot.dependency_edge_removes
                    || snapshot.dependency_cleanup_acks == u64::MAX
                    || snapshot.dependency_cancellations != 0
                    || snapshot.dependency_pins != 0
            );
            assert!(!lock(&hooks.events)
                .iter()
                .any(|event| event.kind == DependencyEventKind::SuccessorElected));
            arena.close();
        }
    }

    #[test]
    fn c1_interest_slot_zero_and_max_attach_are_transactional_and_fixed() {
        assert!(!EMPTY_INTEREST.is_active());
        assert_eq!(std::mem::size_of::<InterestSlot>(), 16);
        assert_eq!(
            std::mem::size_of_val(&DomainState::default().dependency_leaders),
            std::mem::size_of::<usize>()
        );
        assert!(CELL_FIXED_STRUCTURAL_BYTES <= CELL_BASE_METADATA_BYTES as usize);

        let (broker, domain, arena, member, _container) = prepared_dependency_fixture();
        let request = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let before = domain.snapshot();
        let (leader_slot, leader_generation, live_interests) = {
            let mut state = lock(&request.cell.state);
            state.next_interest_ordinal = u64::MAX;
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("member cell must be loading")
            };
            (
                loading.leader_slot,
                loading.generation,
                state.live_interests,
            )
        };
        let Err(failure) = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
        else {
            panic!("MAX ordinal must refuse before slot mutation")
        };
        assert_eq!(
            failure.detail,
            CellControlDetail::Static(CellControlTag::InterestOrdinalOverflow)
        );
        let state = lock(&request.cell.state);
        assert_eq!(state.next_interest_ordinal, u64::MAX);
        assert_eq!(state.live_interests, live_interests);
        assert_eq!(
            state
                .interests
                .iter()
                .filter(|slot| slot.is_active())
                .count(),
            1
        );
        let CellPhase::Loading(loading) = &state.phase else {
            panic!("failed attach cannot replace loading phase")
        };
        assert_eq!(loading.leader_slot, leader_slot);
        assert_eq!(loading.generation, leader_generation);
        drop(state);
        let after = domain.snapshot();
        assert_eq!(after.live_interests, before.live_interests);
        drop(request);
        arena.close();
        drop(arena);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn c1_stale_tokens_and_exact_teardown_cannot_touch_current_dependency() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (_old_hooks, entered, action) =
            dependency_hook(&domain, DependencyEventKind::InsideChildResolve);
        let leader = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let cell = Arc::clone(&leader.cell);
        let leader_cancel = leader.cancellation_handle();
        let follower = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let leader_join = thread::spawn(move || leader.resolve_dependency_synthetic(container));
        let follower_join = thread::spawn(move || follower.resolve_dependency_synthetic(container));
        let stale = entered.recv().unwrap().token;

        let (_edge_hooks, edge_entered, edge_action) = dependency_hook_for_generation(
            &domain,
            DependencyEventKind::EdgeAdded,
            stale.generation + 1,
        );
        {
            let state = lock(&cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("old dependency leader must remain loading")
            };
            loading.cancellation.store(true, Ordering::Release);
        }
        let cancel_join = thread::spawn(move || leader_cancel.cancel());
        action.send(PhaseAction::Continue).unwrap();
        let successor_edge = edge_entered.recv().unwrap();
        assert_eq!(successor_edge.token.generation, stale.generation + 1);

        let stale_child = arena
            .inner
            .request_representation(container, Representation::DeclaredObjStmContainer)
            .unwrap();
        let stale_cancellation = stale_child.cancellation_handle();
        let Err((stale_cancellation, control)) = arena.inner.install_dependency_cancellation(
            &cell,
            stale,
            container,
            stale_cancellation,
        ) else {
            panic!("stale dependency token installed a cancellation handle")
        };
        assert_eq!(
            control.detail,
            CellControlDetail::Static(CellControlTag::DependencyRejectedStale)
        );
        drop(stale_cancellation);
        drop(stale_child);

        let attack_pin = arena
            .resolve_object_stream(container, |_| {
                panic!("prepared dependency container unexpectedly reloaded")
            })
            .unwrap();
        let (attack_pin, control) = arena
            .inner
            .install_dependency_pin(&cell, stale, attack_pin)
            .expect_err("stale dependency token installed a build pin");
        assert_eq!(
            control.detail,
            CellControlDetail::Static(CellControlTag::DependencyRejectedStale)
        );
        drop(attack_pin);
        assert!(arena
            .inner
            .take_dependency_cancellation(&cell, stale)
            .unwrap()
            .is_none());
        let unchanged = domain.snapshot();
        assert_eq!(unchanged.dependency_cancellations, 1);
        assert_eq!(unchanged.dependency_pins, 0);
        assert_eq!(unchanged.dependency_edges, 1);
        assert_eq!(unchanged.dependency_leaders, 1);
        {
            let state = lock(&cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("successor must remain loading")
            };
            let dependency = loading.dependency.as_ref().unwrap();
            assert_eq!(dependency.token, successor_edge.token);
            assert!(dependency.cancellation.is_some());
        }

        let (_pin_hooks, pin_entered, pin_action) = dependency_hook_for_generation(
            &domain,
            DependencyEventKind::PinInstalled,
            stale.generation + 1,
        );
        edge_action.send(PhaseAction::Continue).unwrap();
        let successor_pin_event = pin_entered.recv().unwrap();
        assert_eq!(successor_pin_event.token, successor_edge.token);
        let successor_pin_pointer = {
            let state = lock(&cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("successor must remain loading")
            };
            loading
                .dependency
                .as_ref()
                .and_then(|dependency| dependency.pin.as_ref())
                .unwrap()
                .pointer()
        };
        assert!(arena
            .inner
            .take_dependency_cancellation(&cell, stale)
            .unwrap()
            .is_none());
        let attack_pin = arena
            .resolve_object_stream(container, |_| panic!("prepared container reloaded"))
            .unwrap();
        let (attack_pin, control) = arena
            .inner
            .install_dependency_pin(&cell, stale, attack_pin)
            .expect_err("old token installed over successor pin");
        assert_eq!(
            control.detail,
            CellControlDetail::Static(CellControlTag::DependencyRejectedStale)
        );
        drop(attack_pin);
        {
            let state = lock(&cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("successor must remain loading")
            };
            let dependency = loading.dependency.as_ref().unwrap();
            assert_eq!(dependency.token, successor_edge.token);
            assert_eq!(
                dependency.pin.as_ref().unwrap().pointer(),
                successor_pin_pointer
            );
        }
        let pinned = domain.snapshot();
        assert_eq!(pinned.dependency_cancellations, 0);
        assert_eq!(pinned.dependency_pins, 1);
        assert_eq!(pinned.dependency_edges, 1);
        assert_eq!(pinned.dependency_leaders, 1);

        pin_action.send(PhaseAction::Continue).unwrap();
        cancel_join.join().unwrap();
        assert!(leader_join.join().unwrap().is_err());
        follower_join.join().unwrap().unwrap();
        let drained = domain.snapshot();
        assert_eq!(drained.dependency_cancellations, 0);
        assert_eq!(drained.dependency_pins, 0);
        assert_eq!(drained.dependency_edges, 0);
        assert_eq!(drained.dependency_leaders, 0);
        assert!(!drained.invariant_failed);
        assert!(arena.has_container_cell(container));
        drop(cell);
        arena.close();
        drop(arena);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn c1_r6_ready_cleanup_failure_rolls_back_before_visibility_and_preserves_hook_panic() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (object_reader, loaded_member, loaded_container, _) = object_stream_reader();
        assert_eq!((loaded_member, loaded_container), (member, container));
        let sibling_reader = reader(601);
        let sibling = arena
            .resolve((1, 0), |permit| load(&sibling_reader, (1, 0), permit))
            .unwrap();
        let sibling_pointer = sibling.pointer();
        drop(sibling);

        let (leader, cell, token) = prepared_member_dependency(&arena, member, container);
        let follower = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let (ordinary_wait_tx, ordinary_wait_rx) = mpsc::channel();
        domain.set_wait_hooks(Arc::new(SignallingWaitHooks {
            adds: AtomicU64::new(0),
            removes: AtomicU64::new(0),
            entered: ordinary_wait_tx,
        }));
        let follower_join = thread::spawn(move || {
            follower.resolve(|_| panic!("follower must never load or observe rolled-back Ready"))
        });
        ordinary_wait_rx.recv().unwrap();

        {
            let mut state = lock(&domain.inner.state);
            state.dependency_cleanup_acks = u64::MAX;
        }
        let (_cleanup_hooks, cleanup_entered, cleanup_action) =
            dependency_hook(&domain, DependencyEventKind::CleanupDetached);
        let leader_join =
            thread::spawn(move || leader.resolve(|permit| load(&object_reader, member, permit)));
        let detached = cleanup_entered.recv().unwrap();
        assert_eq!(detached.token, token);
        {
            let state = lock(&cell.state);
            assert!(matches!(state.phase, CellPhase::Ready(_)));
            assert_eq!(
                state.dependency_transition,
                DependencyTransition::Cleaning(token)
            );
        }

        let (visibility_entered, visibility_action) = dependency_wait_hook(&domain);
        cell.ready.notify_all();
        let (waited_key, waited_transition) = visibility_entered.recv().unwrap();
        assert_eq!(waited_key, cell.key);
        assert_eq!(waited_transition, DependencyTransition::Cleaning(token));
        cleanup_action.send(PhaseAction::Panic).unwrap();
        assert!(leader_join.join().is_err());
        visibility_action.send(()).unwrap();
        assert!(follower_join.join().unwrap().is_err());
        assert_dependency_cleanup_failure_closed(&cell);
        assert!(domain.snapshot().invariant_failed);

        let sibling = arena
            .resolve((1, 0), |_| panic!("unrelated cached sibling reloaded"))
            .unwrap();
        assert_eq!(sibling.pointer(), sibling_pointer);
        drop(sibling);
        drop(cell);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
        assert_eq!(broker.snapshot().active_operations, 0);
    }

    #[test]
    fn c1_r6_persistent_negative_cleanup_failure_rolls_back_exact_owner() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (request, cell, _token) = prepared_member_dependency(&arena, member, container);
        {
            let mut state = lock(&domain.inner.state);
            state.dependency_event_ordinal = u64::MAX;
        }
        let error = request
            .resolve(|_| {
                Err(CellLoadError::new(
                    AccessError::typed(member, AccessKind::Backend, "persistent R6 authority"),
                    NegativeDisposition::Persistent,
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind, AccessKind::Backend);
        assert_dependency_cleanup_failure_closed(&cell);
        let snapshot = domain.snapshot();
        assert!(snapshot.invariant_failed);
        assert_eq!(snapshot.negative, 0);
        assert!(!lock(&domain.inner.state).cells.contains_key(&cell.key));
        drop(cell);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
        assert_eq!(broker.snapshot().active_operations, 0);
    }

    #[test]
    fn c1_r6_flight_error_cleanup_failure_closes_map_removed_owner() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (request, cell, _token) = prepared_member_dependency(&arena, member, container);
        {
            let mut state = lock(&domain.inner.state);
            state.dependency_event_ordinal = u64::MAX;
        }
        let error = request
            .resolve(|_| Err(transient(member, "flight-only R6 authority")))
            .unwrap_err();
        assert_eq!(error.kind, AccessKind::Backend);
        assert_dependency_cleanup_failure_closed(&cell);
        let snapshot = domain.snapshot();
        assert!(snapshot.invariant_failed);
        assert_eq!(snapshot.negative, 0);
        assert!(!lock(&domain.inner.state).cells.contains_key(&cell.key));
        drop(cell);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
        assert_eq!(broker.snapshot().active_operations, 0);
    }

    #[test]
    fn c1_r6_exact_teardown_cleanup_failure_closes_exact_loading_owner_only() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let sibling_reader = reader(602);
        let sibling = arena
            .resolve((1, 0), |permit| load(&sibling_reader, (1, 0), permit))
            .unwrap();
        let sibling_pointer = sibling.pointer();
        drop(sibling);
        let (request, cell, token) = prepared_member_dependency(&arena, member, container);
        {
            let mut state = lock(&domain.inner.state);
            state.dependency_event_ordinal = u64::MAX;
        }
        assert!(arena.inner.teardown_exact_key(
            &cell,
            ExactPhaseExpectation::Loading {
                leader_slot: Some(token.leader_slot),
                generation: Some(token.generation),
                require_publication: false,
            },
            CellControlFailure::new(member, AccessKind::Backend, CellControlTag::FlightCancelled,),
            false,
        ));
        assert_dependency_cleanup_failure_closed(&cell);
        assert!(domain.snapshot().invariant_failed);
        let sibling = arena
            .resolve((1, 0), |_| {
                panic!("exact teardown touched an unrelated sibling")
            })
            .unwrap();
        assert_eq!(sibling.pointer(), sibling_pointer);
        drop(sibling);
        drop(request);
        drop(cell);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
        assert_eq!(broker.snapshot().active_operations, 0);
    }

    #[test]
    fn c1_r6_arena_close_cleanup_failure_finishes_exact_owner_and_operation_cleanup() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (request, cell, _token) = prepared_member_dependency(&arena, member, container);
        {
            let mut state = lock(&domain.inner.state);
            state.dependency_event_ordinal = u64::MAX;
        }
        arena.close();
        assert_dependency_cleanup_failure_closed(&cell);
        let snapshot = domain.snapshot();
        assert!(snapshot.invariant_failed);
        assert_eq!(snapshot.cells, 0);
        assert_eq!(snapshot.arenas, 0);
        drop(request);
        assert_eq!(domain.snapshot().live_interests, 0);
        drop(cell);
        drop(arena);
        let broker_snapshot = broker.snapshot();
        assert_eq!(broker_snapshot.aggregate_bytes, 0);
        assert_eq!(broker_snapshot.active_operations, 0);
        assert!(!broker_snapshot.invariant_failed);
    }

    #[test]
    fn c1_r7_touch_overflow_ready_keeps_provisional_closed_hidden_until_failure_cleanup() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (object_reader, loaded_member, loaded_container, _) = object_stream_reader();
        assert_eq!((loaded_member, loaded_container), (member, container));
        let (leader, cell, token) = prepared_member_dependency(&arena, member, container);
        let follower = attach_waiting_member_follower(&domain, &arena, member);
        {
            let mut state = lock(&domain.inner.state);
            state.touch = u64::MAX;
            state.dependency_event_ordinal = u64::MAX;
        }
        let (failed_entered, failed_action) = dependency_cleanup_failure_hook(&domain);
        let leader_join =
            thread::spawn(move || leader.resolve(|permit| load(&object_reader, member, permit)));
        let (failed_key, failed_token) = failed_entered.recv().unwrap();
        assert_eq!((failed_key, failed_token), (cell.key, token));
        assert_closed_transition(
            &cell,
            DependencyTransition::CleanupFailed(token),
            CellControlTag::TouchSequenceOverflow,
        );
        force_dependency_follower_wait(&domain, &cell, DependencyTransition::CleanupFailed(token));
        failed_action.send(()).unwrap();
        let leader_error = leader_join.join().unwrap().unwrap_err();
        let follower_error = follower.join().unwrap().unwrap_err();
        assert_dependency_invariant_access(&leader_error);
        assert_dependency_invariant_access(&follower_error);
        assert_dependency_cleanup_failure_closed(&cell);
        drop(cell);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
        assert_eq!(broker.snapshot().active_operations, 0);
    }

    #[test]
    fn c1_r7_touch_overflow_error_keeps_provisional_closed_hidden_until_failure_cleanup() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (leader, cell, token) = prepared_member_dependency(&arena, member, container);
        let follower = attach_waiting_member_follower(&domain, &arena, member);
        {
            let mut state = lock(&domain.inner.state);
            state.touch = u64::MAX;
            state.dependency_event_ordinal = u64::MAX;
        }
        let (failed_entered, failed_action) = dependency_cleanup_failure_hook(&domain);
        let leader_join = thread::spawn(move || {
            leader.resolve(|_| {
                Err(CellLoadError::new(
                    AccessError::typed(member, AccessKind::Backend, "provisional R7 error"),
                    NegativeDisposition::Persistent,
                ))
            })
        });
        let (failed_key, failed_token) = failed_entered.recv().unwrap();
        assert_eq!((failed_key, failed_token), (cell.key, token));
        assert_closed_transition(
            &cell,
            DependencyTransition::CleanupFailed(token),
            CellControlTag::TouchSequenceOverflow,
        );
        force_dependency_follower_wait(&domain, &cell, DependencyTransition::CleanupFailed(token));
        failed_action.send(()).unwrap();
        let leader_error = leader_join.join().unwrap().unwrap_err();
        let follower_error = follower.join().unwrap().unwrap_err();
        assert_dependency_invariant_access(&leader_error);
        assert_dependency_invariant_access(&follower_error);
        assert_dependency_cleanup_failure_closed(&cell);
        drop(cell);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
        assert_eq!(broker.snapshot().active_operations, 0);
    }

    #[test]
    fn c1_r7_ready_negative_and_flight_rollbacks_gate_cleanup_failed_followers() {
        for case in 0..3 {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (object_reader, loaded_member, loaded_container, _) = object_stream_reader();
            assert_eq!((loaded_member, loaded_container), (member, container));
            let (leader, cell, token) = prepared_member_dependency(&arena, member, container);
            let follower = attach_waiting_member_follower(&domain, &arena, member);
            lock(&domain.inner.state).dependency_event_ordinal = u64::MAX;
            let (failed_entered, failed_action) = dependency_cleanup_failure_hook(&domain);
            let leader_join = thread::spawn(move || {
                leader.resolve(|permit| match case {
                    0 => load(&object_reader, member, permit),
                    1 => Err(CellLoadError::new(
                        AccessError::typed(member, AccessKind::Backend, "persistent R7 error"),
                        NegativeDisposition::Persistent,
                    )),
                    2 => Err(transient(member, "flight-only R7 error")),
                    _ => unreachable!(),
                })
            });
            let (failed_key, failed_token) = failed_entered.recv().unwrap();
            assert_eq!((failed_key, failed_token), (cell.key, token));
            {
                let state = lock(&cell.state);
                assert_eq!(
                    state.dependency_transition,
                    DependencyTransition::CleanupFailed(token)
                );
                assert!(matches!(
                    (&state.phase, case),
                    (CellPhase::Ready(_), 0)
                        | (CellPhase::Negative(_), 1)
                        | (CellPhase::FlightError(_), 2)
                ));
            }
            force_dependency_follower_wait(
                &domain,
                &cell,
                DependencyTransition::CleanupFailed(token),
            );
            failed_action.send(()).unwrap();
            let leader_error = leader_join.join().unwrap().unwrap_err();
            let follower_error = follower.join().unwrap().unwrap_err();
            assert_dependency_invariant_access(&leader_error);
            assert_dependency_invariant_access(&follower_error);
            assert_dependency_cleanup_failure_closed(&cell);
            drop(cell);
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0);
            assert_eq!(broker.snapshot().active_operations, 0);
        }
    }

    #[test]
    fn c1_r7_exact_teardown_gates_closed_followers_for_success_and_cleanup_failure() {
        for cleanup_fails in [false, true] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (request, cell, token) = prepared_member_dependency(&arena, member, container);
            let follower = attach_waiting_member_follower(&domain, &arena, member);
            if cleanup_fails {
                lock(&domain.inner.state).dependency_event_ordinal = u64::MAX;
            }
            let failure_hook = cleanup_fails.then(|| dependency_cleanup_failure_hook(&domain));
            let detached_hook = (!cleanup_fails)
                .then(|| dependency_hook(&domain, DependencyEventKind::CleanupDetached));
            let inner = Arc::clone(&arena.inner);
            let teardown_cell = Arc::clone(&cell);
            let terminal = thread::spawn(move || {
                inner.teardown_exact_key(
                    &teardown_cell,
                    ExactPhaseExpectation::Loading {
                        leader_slot: Some(token.leader_slot),
                        generation: Some(token.generation),
                        require_publication: false,
                    },
                    CellControlFailure::new(
                        member,
                        AccessKind::Backend,
                        CellControlTag::FlightCancelled,
                    ),
                    false,
                )
            });
            let (transition, release_failure, release_detached) =
                if let Some((entered, action)) = failure_hook {
                    let (key, failed_token) = entered.recv().unwrap();
                    assert_eq!((key, failed_token), (cell.key, token));
                    (
                        DependencyTransition::CleanupFailed(token),
                        Some(action),
                        None,
                    )
                } else {
                    let (_hooks, entered, action) = detached_hook.unwrap();
                    let event = entered.recv().unwrap();
                    assert_eq!(event.token, token);
                    (DependencyTransition::Cleaning(token), None, Some(action))
                };
            assert_closed_transition(&cell, transition, CellControlTag::FlightCancelled);
            force_dependency_follower_wait(&domain, &cell, transition);
            if let Some(action) = release_failure {
                action.send(()).unwrap();
            }
            if let Some(action) = release_detached {
                action.send(PhaseAction::Continue).unwrap();
            }
            assert!(terminal.join().unwrap());
            let error = follower.join().unwrap().unwrap_err();
            if cleanup_fails {
                assert_dependency_invariant_access(&error);
                assert_dependency_cleanup_failure_closed(&cell);
            } else {
                assert_eq!(error.detail, CellControlTag::FlightCancelled.detail());
                assert!(lock(&cell.state).dependency_transition.is_idle());
            }
            drop(request);
            drop(cell);
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0);
            assert_eq!(broker.snapshot().active_operations, 0);
        }
    }

    #[test]
    fn c1_r7_arena_close_gates_closed_followers_through_cleanup_and_operation_close() {
        for cleanup_fails in [false, true] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (request, cell, token) = prepared_member_dependency(&arena, member, container);
            let follower = attach_waiting_member_follower(&domain, &arena, member);
            if cleanup_fails {
                lock(&domain.inner.state).dependency_event_ordinal = u64::MAX;
            }
            let failure_hook = cleanup_fails.then(|| dependency_cleanup_failure_hook(&domain));
            let detached_hook = (!cleanup_fails)
                .then(|| dependency_hook(&domain, DependencyEventKind::CleanupDetached));
            let closing = arena.clone();
            let terminal = thread::spawn(move || closing.close());
            let (transition, release_failure, release_detached) =
                if let Some((entered, action)) = failure_hook {
                    let (key, failed_token) = entered.recv().unwrap();
                    assert_eq!((key, failed_token), (cell.key, token));
                    assert_eq!(broker.snapshot().active_operations, 1);
                    (
                        DependencyTransition::CleanupFailed(token),
                        Some(action),
                        None,
                    )
                } else {
                    let (_hooks, entered, action) = detached_hook.unwrap();
                    let event = entered.recv().unwrap();
                    assert_eq!(event.token, token);
                    (DependencyTransition::Cleaning(token), None, Some(action))
                };
            assert_closed_transition(&cell, transition, CellControlTag::ArenaClosed);
            force_dependency_follower_wait(&domain, &cell, transition);
            if let Some(action) = release_failure {
                action.send(()).unwrap();
            }
            if let Some(action) = release_detached {
                action.send(PhaseAction::Continue).unwrap();
            }
            terminal.join().unwrap();
            let error = follower.join().unwrap().unwrap_err();
            if cleanup_fails {
                assert_dependency_invariant_access(&error);
                assert_dependency_cleanup_failure_closed(&cell);
            } else {
                assert_eq!(error.detail, CellControlTag::ArenaClosed.detail());
                assert!(lock(&cell.state).dependency_transition.is_idle());
            }
            assert_eq!(broker.snapshot().active_operations, 0);
            drop(request);
            drop(cell);
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0);
        }
    }

    #[test]
    fn c1_r8_touch_ready_and_error_hold_acknowledging_through_terminal_cleanup_c1_r9_evidence() {
        for error_publication in [false, true] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (object_reader, loaded_member, loaded_container, _) = object_stream_reader();
            assert_eq!((loaded_member, loaded_container), (member, container));
            let (leader, cell, token) = prepared_member_dependency(&arena, member, container);
            let (outcome_tx, outcome_rx) = mpsc::channel();
            let follower = attach_r9_outcome_follower(&domain, &arena, member, outcome_tx.clone());
            lock(&domain.inner.state).touch = u64::MAX;
            let (_hooks, acknowledged, acknowledged_action) =
                dependency_hook(&domain, DependencyEventKind::CleanupAcknowledged);
            let leader_join = thread::spawn(move || {
                leader.resolve(|permit| {
                    if error_publication {
                        Err(CellLoadError::new(
                            AccessError::typed(
                                member,
                                AccessKind::Backend,
                                "healthy R8 touch error",
                            ),
                            NegativeDisposition::Persistent,
                        ))
                    } else {
                        load(&object_reader, member, permit)
                    }
                })
            });
            let event = acknowledged.recv().unwrap();
            assert_eq!(event.token, token);
            assert_closed_transition(
                &cell,
                DependencyTransition::Acknowledging(token),
                CellControlTag::TouchSequenceOverflow,
            );
            establish_r10_condvar_wait(&domain, &cell, DependencyTransition::Acknowledging(token));
            let metadata = lock(&cell.metadata);
            let terminal_cleanup = dependency_terminal_cleanup_hook(&domain);
            rearm_r9_wait_observer(&domain, outcome_tx);
            acknowledged_action.send(PhaseAction::Continue).unwrap();
            assert_eq!(terminal_cleanup.recv().unwrap().0, cell.key);
            cell.ready.notify_all();
            expect_r9_waited(&outcome_rx, DependencyTransition::Acknowledging(token));
            assert_eq!(
                lock(&cell.state).dependency_transition,
                DependencyTransition::Acknowledging(token)
            );
            drop(metadata);
            let follower_error = expect_r9_returned(&outcome_rx).unwrap_err();
            follower.join().unwrap();
            assert_eq!(follower_error.kind, AccessKind::ResourceLimit);
            assert_eq!(
                follower_error.detail,
                CellControlTag::TouchSequenceOverflow.detail()
            );
            let leader_error = leader_join.join().unwrap().unwrap_err();
            assert_eq!(leader_error, follower_error);
            assert!(lock(&cell.state).dependency_transition.is_idle());
            drop(cell);
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0);
            assert_eq!(broker.snapshot().active_operations, 0);
        }
    }

    #[test]
    fn c1_r8_exact_teardown_holds_acknowledging_until_external_terminal_cleanup_c1_r9_evidence() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (request, cell, token) = prepared_member_dependency(&arena, member, container);
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let follower = attach_r9_outcome_follower(&domain, &arena, member, outcome_tx.clone());
        let (_hooks, acknowledged, acknowledged_action) =
            dependency_hook(&domain, DependencyEventKind::CleanupAcknowledged);
        let inner = Arc::clone(&arena.inner);
        let teardown_cell = Arc::clone(&cell);
        let terminal = thread::spawn(move || {
            inner.teardown_exact_key(
                &teardown_cell,
                ExactPhaseExpectation::Loading {
                    leader_slot: Some(token.leader_slot),
                    generation: Some(token.generation),
                    require_publication: false,
                },
                CellControlFailure::new(
                    member,
                    AccessKind::Backend,
                    CellControlTag::FlightCancelled,
                ),
                false,
            )
        });
        let event = acknowledged.recv().unwrap();
        assert_eq!(event.token, token);
        assert_closed_transition(
            &cell,
            DependencyTransition::Acknowledging(token),
            CellControlTag::FlightCancelled,
        );
        establish_r10_condvar_wait(&domain, &cell, DependencyTransition::Acknowledging(token));
        let metadata = lock(&cell.metadata);
        let terminal_cleanup = dependency_terminal_cleanup_hook(&domain);
        rearm_r9_wait_observer(&domain, outcome_tx);
        acknowledged_action.send(PhaseAction::Continue).unwrap();
        assert_eq!(terminal_cleanup.recv().unwrap().0, cell.key);
        cell.ready.notify_all();
        expect_r9_waited(&outcome_rx, DependencyTransition::Acknowledging(token));
        assert_eq!(
            lock(&cell.state).dependency_transition,
            DependencyTransition::Acknowledging(token)
        );
        drop(metadata);
        let follower_error = expect_r9_returned(&outcome_rx).unwrap_err();
        follower.join().unwrap();
        assert_eq!(follower_error.kind, AccessKind::Backend);
        assert_eq!(
            follower_error.detail,
            CellControlTag::FlightCancelled.detail()
        );
        assert!(terminal.join().unwrap());
        assert!(lock(&cell.state).dependency_transition.is_idle());
        drop(request);
        drop(cell);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
        assert_eq!(broker.snapshot().active_operations, 0);
    }

    #[test]
    fn c1_r8_arena_close_holds_acknowledging_through_operation_close_c1_r9_evidence() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let (request, cell, token) = prepared_member_dependency(&arena, member, container);
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let follower = attach_r9_outcome_follower(&domain, &arena, member, outcome_tx.clone());
        let (_hooks, acknowledged, acknowledged_action) =
            dependency_hook(&domain, DependencyEventKind::CleanupAcknowledged);
        let closing = arena.clone();
        let terminal = thread::spawn(move || closing.close());
        let event = acknowledged.recv().unwrap();
        assert_eq!(event.token, token);
        assert_closed_transition(
            &cell,
            DependencyTransition::Acknowledging(token),
            CellControlTag::ArenaClosed,
        );
        assert_eq!(broker.snapshot().active_operations, 1);
        establish_r10_condvar_wait(&domain, &cell, DependencyTransition::Acknowledging(token));
        let metadata = lock(&cell.metadata);
        let terminal_cleanup = dependency_terminal_cleanup_hook(&domain);
        rearm_r9_wait_observer(&domain, outcome_tx);
        acknowledged_action.send(PhaseAction::Continue).unwrap();
        assert_eq!(terminal_cleanup.recv().unwrap().0, cell.key);
        assert_eq!(broker.snapshot().active_operations, 1);
        cell.ready.notify_all();
        expect_r9_waited(&outcome_rx, DependencyTransition::Acknowledging(token));
        assert_eq!(broker.snapshot().active_operations, 1);
        drop(metadata);
        let follower_error = expect_r9_returned(&outcome_rx).unwrap_err();
        follower.join().unwrap();
        assert_eq!(follower_error.kind, AccessKind::Backend);
        assert_eq!(follower_error.detail, CellControlTag::ArenaClosed.detail());
        assert_eq!(broker.snapshot().active_operations, 0);
        terminal.join().unwrap();
        assert!(lock(&cell.state).dependency_transition.is_idle());
        drop(request);
        drop(cell);
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn c2_scheduled_member_uses_one_post_pin_grant_then_ready_hits_without_child_work() {
        let broker = broker();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let arena = domain.open_arena().unwrap();
        let (stream_reader, member, container, _) = object_stream_reader();
        let hooks = Arc::new(RecordingDependencyHooks {
            events: Mutex::new(Vec::new()),
        });
        domain.set_dependency_hooks(hooks.clone());
        let request = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let child_reader = Arc::clone(&stream_reader);
        let member_reader = Arc::clone(&stream_reader);
        let pin = request
            .resolve_scheduled_member_synthetic(
                container,
                move |permit| {
                    child_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                },
                move |permit| {
                    assert_eq!(permit.limit_bytes(), 4 * 1024 * 1024);
                    load(&member_reader, member, permit)
                },
            )
            .unwrap();
        assert_eq!(
            pin.owner()
                .as_object()
                .as_dict()
                .unwrap()
                .get(b"Answer")
                .unwrap()
                .as_i64()
                .unwrap(),
            42
        );
        let pointer = pin.pointer();
        drop(pin);

        let ordered = [
            DependencyEventKind::ChildResolved,
            DependencyEventKind::ChildHandleTaken,
            DependencyEventKind::PinInstalled,
            DependencyEventKind::BeforeMemberAdmission,
            DependencyEventKind::MemberRequestCreated,
            DependencyEventKind::MemberGranted,
            DependencyEventKind::MemberParseEnter,
            DependencyEventKind::MemberParseExit,
            DependencyEventKind::MemberReconciledBeforePublication,
            DependencyEventKind::MemberPublished,
        ];
        let events = lock(&hooks.events);
        let scheduled = events
            .iter()
            .filter(|event| ordered.contains(&event.kind))
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            scheduled.iter().map(|event| event.kind).collect::<Vec<_>>(),
            ordered
        );
        let token = scheduled[0].token;
        let expected_edge = DependencyEdge {
            from: token.key,
            to: CellKey {
                epoch: token.key.epoch,
                id: container,
                representation: Representation::DeclaredObjStmContainer,
            },
        };
        assert_eq!(token.key.id, member);
        assert_eq!(
            token.key.representation,
            Representation::DeclaredObjStmMember
        );
        assert!(scheduled.iter().all(|event| event.token == token));
        assert!(scheduled.iter().all(|event| event.edge == expected_edge));
        assert!(scheduled
            .windows(2)
            .all(|pair| pair[0].ordinal < pair[1].ordinal));
        drop(events);
        let first = domain.snapshot();
        assert_eq!(first.members.calls, 1);
        assert_eq!(first.members.loads, 1);
        assert_eq!(first.containers.calls, 1);
        assert_eq!(first.containers.loads, 1);
        assert_eq!(first.dependency_cancellations, 0);
        assert_eq!(first.dependency_pins, 0);
        assert_eq!(first.dependency_edges, 0);

        let event_count = lock(&hooks.events).len();
        let hit = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap()
            .resolve_scheduled_member_synthetic(
                container,
                |_| panic!("ready member hit must not resolve its child"),
                |_| panic!("ready member hit must not request or parse"),
            )
            .unwrap();
        assert_eq!(hit.pointer(), pointer);
        assert_eq!(lock(&hooks.events).len(), event_count);
        let second = domain.snapshot();
        assert_eq!(second.members.calls, 2);
        assert_eq!(second.members.loads, 1);
        assert_eq!(second.members.hits, 1);
        assert_eq!(second.containers.calls, 1);
        drop(hit);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn c2_each_stage_acknowledgement_is_an_exact_outside_lock_ordering_barrier() {
        let ordered = [
            DependencyEventKind::ChildResolved,
            DependencyEventKind::ChildHandleTaken,
            DependencyEventKind::PinInstalled,
            DependencyEventKind::BeforeMemberAdmission,
            DependencyEventKind::MemberRequestCreated,
            DependencyEventKind::MemberGranted,
            DependencyEventKind::MemberParseEnter,
            DependencyEventKind::MemberParseExit,
            DependencyEventKind::MemberReconciledBeforePublication,
            DependencyEventKind::MemberPublished,
        ];
        for target_index in 2..ordered.len() {
            let target = ordered[target_index];
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (hooks, entered, action) = dependency_hook(&domain, target);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let cell = Arc::clone(&request.cell);
            let scalar_reader = reader(1_414);
            let join = thread::spawn(move || {
                request.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared child must hit"),
                    move |permit| load(&scalar_reader, (1, 0), permit),
                )
            });
            let paused = entered.recv().unwrap();
            assert_eq!(paused.kind, target);
            assert_eq!(paused.token.key.id, member);
            assert_eq!(
                paused.token.key.representation,
                Representation::DeclaredObjStmMember
            );
            assert_eq!(paused.edge.from, paused.token.key);
            assert_eq!(paused.edge.to.id, container);
            assert_eq!(
                paused.edge.to.representation,
                Representation::DeclaredObjStmContainer
            );
            let events = lock(&hooks.events);
            let prefix = events
                .iter()
                .filter(|event| ordered.contains(&event.kind))
                .copied()
                .collect::<Vec<_>>();
            assert_eq!(
                prefix.iter().map(|event| event.kind).collect::<Vec<_>>(),
                ordered[..=target_index],
                "callback returned or a later stage escaped before {target:?} acknowledgement"
            );
            assert!(prefix.iter().all(|event| event.token == paused.token));
            assert!(prefix.iter().all(|event| event.edge == paused.edge));
            assert!(prefix
                .windows(2)
                .all(|pair| pair[0].ordinal < pair[1].ordinal));
            assert_eq!(prefix.last().unwrap(), &paused);
            drop(events);
            assert!(hooks.lock_probes.load(Ordering::Relaxed) >= prefix.len() as u64);
            let transition = lock(&cell.state).dependency_transition;
            if target == DependencyEventKind::MemberPublished {
                assert!(transition.is_idle());
            } else {
                assert_eq!(
                    transition,
                    DependencyTransition::Announcing {
                        token: paused.token,
                        kind: target,
                    }
                );
            }
            action.send(PhaseAction::Continue).unwrap();
            let pin = join.join().unwrap().unwrap();
            assert_eq!(pin.owner().as_object().as_i64().unwrap(), 1_414);
            drop(pin);
            drop(cell);
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "{target:?}");
        }
    }

    #[test]
    fn c2_same_member_n_2_4_16_has_one_child_grant_parse_owner_and_second_wave_hits() {
        for callers in [2usize, 4, 16] {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let (reader, member, container, _) = object_stream_reader();
            let hooks = Arc::new(RecordingDependencyHooks {
                events: Mutex::new(Vec::new()),
            });
            domain.set_dependency_hooks(hooks.clone());
            let child_pause = Arc::new(AtomicBool::new(true));
            let (child_entered_tx, child_entered_rx) = mpsc::sync_channel(0);
            let (child_action_tx, child_action_rx) = mpsc::sync_channel(0);
            let child_action_rx = Arc::new(Mutex::new(child_action_rx));
            let (wait_tx, wait_rx) = mpsc::channel();
            domain.set_wait_hooks(Arc::new(SignallingWaitHooks {
                adds: AtomicU64::new(0),
                removes: AtomicU64::new(0),
                entered: wait_tx,
            }));
            let requests = (0..callers)
                .map(|_| {
                    arena
                        .inner
                        .request_representation(member, Representation::DeclaredObjStmMember)
                        .unwrap()
                })
                .collect::<Vec<_>>();
            let before_child_grants = broker.snapshot().operations[&arena.epoch()].grants;
            let joins = requests
                .into_iter()
                .map(|request| {
                    let child_reader = Arc::clone(&reader);
                    let member_reader = Arc::clone(&reader);
                    let child_pause = Arc::clone(&child_pause);
                    let child_entered = child_entered_tx.clone();
                    let child_action = Arc::clone(&child_action_rx);
                    thread::spawn(move || {
                        request.resolve_scheduled_member_synthetic(
                            container,
                            move |permit| {
                                if child_pause.swap(false, Ordering::AcqRel) {
                                    child_entered.send(()).unwrap();
                                    lock(&child_action).recv().unwrap();
                                }
                                child_reader
                                    .prepare_object_stream_with_permit(container, permit)
                                    .map_err(|error| {
                                        CellLoadError::objstm(crate::objstm_failures::classify(
                                            container, error,
                                        ))
                                    })
                            },
                            move |permit| load(&member_reader, member, permit),
                        )
                    })
                })
                .collect::<Vec<_>>();
            child_entered_rx.recv().unwrap();
            for _ in 1..callers {
                wait_rx.recv().unwrap();
            }
            let blocked = domain.snapshot();
            assert_eq!(blocked.members.loads, 0, "N={callers}");
            assert_eq!(blocked.members.waits, (callers - 1) as u64, "N={callers}");
            assert_eq!(blocked.containers.loads, 1, "N={callers}");
            let blocked_events = lock(&hooks.events);
            assert_eq!(
                blocked_events
                    .iter()
                    .filter(|event| matches!(
                        event.kind,
                        DependencyEventKind::PinInstalled
                            | DependencyEventKind::BeforeMemberAdmission
                            | DependencyEventKind::MemberRequestCreated
                            | DependencyEventKind::MemberQueued
                            | DependencyEventKind::MemberGranted
                            | DependencyEventKind::MemberParseEnter
                    ))
                    .count(),
                0,
                "N={callers}"
            );
            drop(blocked_events);
            let blocked_operation = broker.snapshot().operations[&arena.epoch()].clone();
            assert_eq!(
                blocked_operation.grants,
                before_child_grants + 2,
                "N={callers}"
            );
            assert_eq!(blocked_operation.in_flight, 1, "N={callers}");
            child_action_tx.send(()).unwrap();
            let pins = joins
                .into_iter()
                .map(|join| join.join().unwrap().unwrap())
                .collect::<Vec<_>>();
            assert!(pins.iter().all(|pin| pin.pointer() == pins[0].pointer()));
            let first = domain.snapshot();
            assert_eq!(first.members.calls, callers as u64, "N={callers}");
            assert_eq!(first.members.loads, 1, "N={callers}");
            assert_eq!(first.members.waits, (callers - 1) as u64, "N={callers}");
            assert_eq!(first.members.hits, 0, "N={callers}");
            assert_eq!(first.containers.calls, 1, "N={callers}");
            assert_eq!(first.containers.loads, 1, "N={callers}");
            assert_eq!(first.dependency_edge_adds, 1, "N={callers}");
            assert_eq!(first.dependency_edge_removes, 1, "N={callers}");
            assert_eq!(first.dependency_cleanup_acks, 1, "N={callers}");
            let events = lock(&hooks.events);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.kind == DependencyEventKind::MemberGranted)
                    .count(),
                1,
                "N={callers}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.kind == DependencyEventKind::MemberParseEnter)
                    .count(),
                1,
                "N={callers}"
            );
            let event_count = events.len();
            drop(events);
            let pointer = pins[0].pointer();
            drop(pins);

            for _ in 0..callers {
                let hit = arena
                    .inner
                    .request_representation(member, Representation::DeclaredObjStmMember)
                    .unwrap()
                    .resolve_scheduled_member_synthetic(
                        container,
                        |_| panic!("same-member second wave child work"),
                        |_| panic!("same-member second wave parse"),
                    )
                    .unwrap();
                assert_eq!(hit.pointer(), pointer);
            }
            let second = domain.snapshot();
            assert_eq!(second.members.calls, (2 * callers) as u64, "N={callers}");
            assert_eq!(second.members.loads, 1, "N={callers}");
            assert_eq!(second.members.hits, callers as u64, "N={callers}");
            assert_eq!(second.containers.calls, 1, "N={callers}");
            assert_eq!(lock(&hooks.events).len(), event_count, "N={callers}");
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "N={callers}");
        }
    }

    #[test]
    fn c2_distinct_members_n_2_4_16_acknowledge_each_own_child_chain_independently() {
        for callers in [2usize, 4, 16] {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let (reader, loaded_member, container, _) = object_stream_reader();
            let member_ids = (0..=callers)
                .map(|offset| (20_000 + u32::try_from(offset).unwrap(), 0))
                .collect::<Vec<_>>();
            let sentinel = *member_ids.last().unwrap();

            let (child_entered_tx, child_entered_rx) = mpsc::channel();
            let (request_entered_tx, request_entered_rx) = mpsc::channel();
            let (grant_entered_tx, grant_entered_rx) = mpsc::channel();
            let mut child_actions = BTreeMap::new();
            let mut child_senders = BTreeMap::new();
            let mut request_actions = BTreeMap::new();
            let mut request_senders = BTreeMap::new();
            let mut grant_actions = BTreeMap::new();
            let mut grant_senders = BTreeMap::new();
            for id in &member_ids {
                let (child_tx, child_rx) = mpsc::sync_channel(0);
                child_senders.insert(*id, child_tx);
                child_actions.insert(*id, Mutex::new(child_rx));
                let (request_tx, request_rx) = mpsc::sync_channel(0);
                request_senders.insert(*id, request_tx);
                request_actions.insert(*id, Mutex::new(request_rx));
                let (grant_tx, grant_rx) = mpsc::sync_channel(0);
                grant_senders.insert(*id, grant_tx);
                grant_actions.insert(*id, Mutex::new(grant_rx));
            }
            let hooks = Arc::new(PerTokenChainHooks {
                events: Mutex::new(Vec::new()),
                child_entered: child_entered_tx,
                request_entered: request_entered_tx,
                grant_entered: grant_entered_tx,
                child_actions,
                request_actions,
                grant_actions,
            });
            domain.set_dependency_hooks(hooks.clone());

            let first_child = Arc::new(AtomicBool::new(true));
            let (loader_entered_tx, loader_entered_rx) = mpsc::sync_channel(0);
            let (loader_action_tx, loader_action_rx) = mpsc::sync_channel(0);
            let loader_action = Arc::new(Mutex::new(loader_action_rx));
            let (container_wait_tx, container_wait_rx) = mpsc::channel();
            domain.set_wait_hooks(Arc::new(SignallingWaitHooks {
                adds: AtomicU64::new(0),
                removes: AtomicU64::new(0),
                entered: container_wait_tx,
            }));

            let mut cancellations = Vec::new();
            let mut joins = Vec::new();
            for (index, id) in member_ids.iter().copied().enumerate() {
                let request = arena
                    .inner
                    .request_representation(id, Representation::DeclaredObjStmMember)
                    .unwrap();
                cancellations.push(request.cancellation_handle());
                let child_reader = Arc::clone(&reader);
                let first_child = Arc::clone(&first_child);
                let loader_entered = loader_entered_tx.clone();
                let loader_action = Arc::clone(&loader_action);
                joins.push(thread::spawn(move || {
                    request.resolve_scheduled_member_synthetic(
                        container,
                        move |permit| {
                            if first_child.swap(false, Ordering::AcqRel) {
                                loader_entered.send(()).unwrap();
                                lock(&loader_action).recv().unwrap();
                            }
                            child_reader
                                .prepare_object_stream_with_permit(container, permit)
                                .map_err(|error| {
                                    CellLoadError::objstm(crate::objstm_failures::classify(
                                        container, error,
                                    ))
                                })
                        },
                        move |permit| {
                            panic!("per-token chain row must cancel before member parse: {index} {id:?} {permit:?}")
                        },
                    )
                }));
                if index == 0 {
                    loader_entered_rx.recv().unwrap();
                }
            }
            for _ in 1..member_ids.len() {
                container_wait_rx.recv().unwrap();
            }
            loader_action_tx.send(()).unwrap();
            let child_events = (0..member_ids.len())
                .map(|_| child_entered_rx.recv().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                child_events
                    .iter()
                    .map(|event| event.token.key.id)
                    .collect::<BTreeSet<_>>(),
                member_ids.iter().copied().collect::<BTreeSet<_>>(),
                "N={callers}"
            );

            for id in member_ids.iter().take(callers).copied() {
                child_senders[&id].send(()).unwrap();
                let requested = request_entered_rx.recv().unwrap();
                assert_eq!(requested.token.key.id, id, "N={callers}");
                let events = lock(&hooks.events);
                let chain = [
                    DependencyEventKind::ChildResolved,
                    DependencyEventKind::ChildHandleTaken,
                    DependencyEventKind::PinInstalled,
                    DependencyEventKind::BeforeMemberAdmission,
                    DependencyEventKind::MemberRequestCreated,
                ];
                let ordinals = chain.map(|kind| {
                    events
                        .iter()
                        .find(|event| event.token == requested.token && event.kind == kind)
                        .unwrap()
                        .ordinal
                });
                assert!(
                    ordinals.windows(2).all(|pair| pair[0] < pair[1]),
                    "N={callers}"
                );
                assert!(events.iter().all(|event| {
                    event.token.key.id != sentinel
                        || !matches!(
                            event.kind,
                            DependencyEventKind::ChildHandleTaken
                                | DependencyEventKind::PinInstalled
                                | DependencyEventKind::BeforeMemberAdmission
                                | DependencyEventKind::MemberRequestCreated
                        )
                }));
                drop(events);
                request_senders[&id].send(()).unwrap();
                let granted = grant_entered_rx.recv().unwrap();
                assert_eq!(granted.token, requested.token, "N={callers}");
                let events = lock(&hooks.events);
                let chain = [
                    DependencyEventKind::ChildResolved,
                    DependencyEventKind::ChildHandleTaken,
                    DependencyEventKind::PinInstalled,
                    DependencyEventKind::BeforeMemberAdmission,
                    DependencyEventKind::MemberRequestCreated,
                    DependencyEventKind::MemberGranted,
                ];
                let ordinals = chain.map(|kind| {
                    events
                        .iter()
                        .find(|event| event.token == granted.token && event.kind == kind)
                        .unwrap()
                        .ordinal
                });
                assert!(
                    ordinals.windows(2).all(|pair| pair[0] < pair[1]),
                    "N={callers}"
                );
                assert!(events.iter().all(|event| {
                    event.token.key.id != sentinel
                        || !matches!(
                            event.kind,
                            DependencyEventKind::ChildHandleTaken
                                | DependencyEventKind::PinInstalled
                                | DependencyEventKind::BeforeMemberAdmission
                                | DependencyEventKind::MemberRequestCreated
                                | DependencyEventKind::MemberGranted
                        )
                }));
            }

            let (cancel_wait_tx, cancel_wait_rx) = mpsc::channel();
            domain.set_dependency_wait_hooks(Arc::new(SignallingDependencyWaitHooks {
                entered: cancel_wait_tx,
            }));
            let cancel_joins = cancellations
                .into_iter()
                .map(|cancel| thread::spawn(move || cancel.cancel()))
                .collect::<Vec<_>>();
            let waits = (0..member_ids.len())
                .map(|_| cancel_wait_rx.recv().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                waits.iter().map(|(key, _)| key.id).collect::<BTreeSet<_>>(),
                member_ids.iter().copied().collect::<BTreeSet<_>>(),
                "N={callers}"
            );
            for id in member_ids.iter().take(callers) {
                grant_senders[id].send(()).unwrap();
            }
            child_senders[&sentinel].send(()).unwrap();
            for join in cancel_joins {
                join.join().unwrap();
            }
            for join in joins {
                assert!(join.join().unwrap().is_err(), "N={callers}");
            }
            let terminal = domain.snapshot();
            assert_eq!(terminal.members.loads, 0, "N={callers}");
            assert_eq!(terminal.dependency_edges, 0, "N={callers}");
            arena.close();
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
            let _ = loaded_member;
        }
    }

    #[test]
    fn c2_distinct_members_n_2_4_16_share_one_container_flight_then_hit_independently() {
        for callers in [2usize, 4, 16] {
            let broker = broker();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let (reader, loaded_member, container, _) = object_stream_reader();
            let member_ids = (0..callers)
                .map(|offset| (10_000 + u32::try_from(offset).unwrap(), 0))
                .collect::<Vec<_>>();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::sync_channel(0);
            let hooks = Arc::new(SignallingDependencyHooks {
                target: DependencyEventKind::InsideChildResolve,
                first_paused: AtomicBool::new(true),
                entered: entered_tx,
                first_action: Mutex::new(action_rx),
                events: Mutex::new(Vec::new()),
            });
            domain.set_dependency_hooks(hooks.clone());
            let (wait_tx, wait_rx) = mpsc::channel();
            domain.set_wait_hooks(Arc::new(SignallingWaitHooks {
                adds: AtomicU64::new(0),
                removes: AtomicU64::new(0),
                entered: wait_tx,
            }));

            let spawn_member = |id: ObjectId| {
                let request = arena
                    .inner
                    .request_representation(id, Representation::DeclaredObjStmMember)
                    .unwrap();
                let child_reader = Arc::clone(&reader);
                let member_reader = Arc::clone(&reader);
                thread::spawn(move || {
                    request.resolve_scheduled_member_synthetic(
                        container,
                        move |permit| {
                            child_reader
                                .prepare_object_stream_with_permit(container, permit)
                                .map_err(|error| {
                                    CellLoadError::objstm(crate::objstm_failures::classify(
                                        container, error,
                                    ))
                                })
                        },
                        move |permit| load(&member_reader, loaded_member, permit),
                    )
                })
            };
            let mut joins = vec![spawn_member(member_ids[0])];
            let first_event = entered_rx.recv().unwrap();
            assert_eq!(first_event.token.key.id, member_ids[0]);
            for id in member_ids.iter().skip(1).copied() {
                joins.push(spawn_member(id));
            }
            for _ in 1..callers {
                entered_rx.recv().unwrap();
            }
            for _ in 1..callers {
                wait_rx.recv().unwrap();
            }
            let blocked = domain.snapshot();
            let blocked_broker = broker.snapshot();
            assert_eq!(blocked.members.loads, 0, "N={callers}");
            assert_eq!(blocked.containers.calls, callers as u64, "N={callers}");
            assert_eq!(blocked.containers.loads, 0, "N={callers}");
            assert_eq!(
                blocked.containers.waits,
                (callers - 1) as u64,
                "N={callers}"
            );
            let (parse_hooks, parse_entered, parse_action) =
                multi_dependency_hook(&domain, DependencyEventKind::MemberParseEnter);
            action_tx.send(()).unwrap();

            let parse_events = (0..callers)
                .map(|_| parse_entered.recv().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                parse_events
                    .iter()
                    .map(|event| event.token.key.id)
                    .collect::<BTreeSet<_>>(),
                member_ids.iter().copied().collect::<BTreeSet<_>>(),
                "N={callers}"
            );
            let granted = broker.snapshot();
            let granted_operation = &granted.operations[&arena.epoch()];
            assert_eq!(granted_operation.in_flight, callers, "N={callers}");
            assert_eq!(granted_operation.queued, 0, "N={callers}");
            assert_eq!(
                granted_operation.grants,
                blocked_broker.operations[&arena.epoch()].grants + callers as u64 + 1,
                "N={callers}"
            );
            assert_eq!(
                granted_operation.granted_bytes,
                blocked_broker.operations[&arena.epoch()].granted_bytes
                    + 64 * 1024 * 1024
                    + (callers as u64 * 4 * 1024 * 1024),
                "N={callers}"
            );
            let active_permits = {
                let state = lock(&domain.inner.state);
                member_ids
                    .iter()
                    .filter(|id| {
                        state
                            .cells
                            .iter()
                            .find(|(key, _)| {
                                key.id == **id
                                    && key.representation == Representation::DeclaredObjStmMember
                            })
                            .is_some_and(|(_, cell)| {
                                matches!(
                                    &lock(&cell.state).phase,
                                    CellPhase::Loading(loading) if loading.permit.is_some()
                                )
                            })
                    })
                    .count()
            };
            assert_eq!(active_permits, callers, "N={callers}");
            let synchronized_events = lock(&parse_hooks.events);
            for parse in &parse_events {
                let pin = synchronized_events
                    .iter()
                    .find(|event| {
                        event.token == parse.token
                            && event.kind == DependencyEventKind::PinInstalled
                    })
                    .unwrap();
                let grant = synchronized_events
                    .iter()
                    .find(|event| {
                        event.token == parse.token
                            && event.kind == DependencyEventKind::MemberGranted
                    })
                    .unwrap();
                assert!(pin.ordinal < grant.ordinal && grant.ordinal < parse.ordinal);
            }
            drop(synchronized_events);
            for _ in 0..callers {
                parse_action.send(()).unwrap();
            }

            let pins = joins
                .drain(..)
                .map(|join| join.join().unwrap().unwrap())
                .collect::<Vec<_>>();
            for left in 0..pins.len() {
                for right in (left + 1)..pins.len() {
                    assert_ne!(pins[left].pointer(), pins[right].pointer(), "N={callers}");
                }
            }
            let first = domain.snapshot();
            assert_eq!(first.members.calls, callers as u64, "N={callers}");
            assert_eq!(first.members.loads, callers as u64, "N={callers}");
            assert_eq!(first.members.waits, 0, "N={callers}");
            assert_eq!(first.containers.calls, callers as u64, "N={callers}");
            assert_eq!(first.containers.loads, 1, "N={callers}");
            assert_eq!(first.containers.waits, (callers - 1) as u64, "N={callers}");
            assert_eq!(first.dependency_edge_adds, callers as u64, "N={callers}");
            assert_eq!(first.dependency_edge_removes, callers as u64, "N={callers}");
            assert_eq!(first.dependency_cleanup_acks, callers as u64, "N={callers}");
            let events = lock(&parse_hooks.events);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.kind == DependencyEventKind::MemberGranted)
                    .count(),
                callers,
                "N={callers}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.kind == DependencyEventKind::MemberParseEnter)
                    .count(),
                callers,
                "N={callers}"
            );
            let event_count = events.len();
            drop(events);
            let pointers = pins
                .iter()
                .map(ResolvedObjectPin::pointer)
                .collect::<Vec<_>>();
            drop(pins);

            for (index, id) in member_ids.iter().copied().enumerate() {
                let hit = arena
                    .inner
                    .request_representation(id, Representation::DeclaredObjStmMember)
                    .unwrap()
                    .resolve_scheduled_member_synthetic(
                        container,
                        |_| panic!("distinct-member second wave child work"),
                        |_| panic!("distinct-member second wave parse"),
                    )
                    .unwrap();
                assert_eq!(hit.pointer(), pointers[index]);
            }
            let second = domain.snapshot();
            assert_eq!(second.members.calls, (2 * callers) as u64, "N={callers}");
            assert_eq!(second.members.loads, callers as u64, "N={callers}");
            assert_eq!(second.members.hits, callers as u64, "N={callers}");
            assert_eq!(second.containers.calls, callers as u64, "N={callers}");
            assert_eq!(lock(&parse_hooks.events).len(), event_count, "N={callers}");
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "N={callers}");
        }
    }

    #[test]
    fn c2_last_interest_cancellation_at_each_post_pin_stage_cleans_exact_resources() {
        let rows = [
            (DependencyEventKind::MemberRequestCreated, 0),
            (DependencyEventKind::MemberGranted, 0),
            (DependencyEventKind::MemberParseEnter, 0),
            (DependencyEventKind::MemberParseExit, 1),
            (DependencyEventKind::MemberReconciledBeforePublication, 1),
        ];
        for (phase, expected_parses) in rows {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (_hooks, entered, action) = dependency_hook(&domain, phase);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let cancellation = request.cancellation_handle();
            let parses = Arc::new(AtomicU64::new(0));
            let parses_for_load = Arc::clone(&parses);
            let scalar_reader = reader(313);
            let join = thread::spawn(move || {
                request.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared container must hit in cleanup row"),
                    move |permit| {
                        parses_for_load.fetch_add(1, Ordering::Relaxed);
                        load(&scalar_reader, (1, 0), permit)
                    },
                )
            });
            let event = entered.recv().unwrap();
            assert_eq!(event.kind, phase);
            let (wait_entered, wait_action) = dependency_wait_hook(&domain);
            let cancel_join = thread::spawn(move || cancellation.cancel());
            wait_entered.recv().unwrap();
            action.send(PhaseAction::Continue).unwrap();
            wait_action.send(()).unwrap();
            cancel_join.join().unwrap();
            let error = join.join().unwrap().unwrap_err();
            assert_eq!(error.kind, AccessKind::Backend, "{phase:?}");
            assert_eq!(parses.load(Ordering::Relaxed), expected_parses, "{phase:?}");
            let terminal = domain.snapshot();
            assert_eq!(terminal.dependency_cancellations, 0, "{phase:?}");
            assert_eq!(terminal.dependency_pins, 0, "{phase:?}");
            assert_eq!(terminal.dependency_edges, 0, "{phase:?}");
            assert_eq!(terminal.dependency_leaders, 0, "{phase:?}");
            assert_eq!(terminal.dependency_edge_adds, 1, "{phase:?}");
            assert_eq!(terminal.dependency_edge_removes, 1, "{phase:?}");
            assert_eq!(terminal.dependency_cleanup_acks, 1, "{phase:?}");
            let operation = broker.snapshot().operations[&arena.epoch()].clone();
            assert_eq!(operation.queued, 0, "{phase:?}");
            assert_eq!(operation.in_flight, 0, "{phase:?}");
            assert_eq!(operation.pin_bytes, 0, "{phase:?}");
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "{phase:?}");
        }
    }

    #[test]
    fn c2_follower_cancel_at_each_post_pin_stage_preserves_exact_leader_resources() {
        for phase in [
            DependencyEventKind::MemberRequestCreated,
            DependencyEventKind::MemberGranted,
            DependencyEventKind::MemberParseEnter,
            DependencyEventKind::MemberParseExit,
            DependencyEventKind::MemberReconciledBeforePublication,
        ] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let sibling_reader = reader(414);
            let sibling = arena
                .resolve((1, 0), |permit| load(&sibling_reader, (1, 0), permit))
                .unwrap();
            let sibling_pointer = sibling.pointer();
            drop(sibling);
            let (_hooks, entered, action) = dependency_hook(&domain, phase);
            let leader = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let follower = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let follower_cancel = follower.cancellation_handle();
            let scalar_reader = reader(415);
            let leader_join = thread::spawn(move || {
                leader.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared child must hit"),
                    move |permit| load(&scalar_reader, (1, 0), permit),
                )
            });
            assert_eq!(entered.recv().unwrap().kind, phase);
            let held_before = broker.snapshot();
            let follower_join = thread::spawn(move || {
                follower.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("cancelled follower cannot resolve child"),
                    |_| panic!("cancelled follower cannot parse"),
                )
            });
            follower_cancel.cancel();
            assert!(follower_join.join().unwrap().is_err(), "{phase:?}");
            assert_broker_snapshot_unchanged(
                &held_before,
                &broker.snapshot(),
                &format!("follower cancellation at {phase:?}"),
            );
            let held = domain.snapshot();
            assert_eq!(held.dependency_pins, 1, "{phase:?}");
            assert_eq!(held.dependency_edges, 1, "{phase:?}");
            assert_eq!(held.dependency_leaders, 1, "{phase:?}");
            action.send(PhaseAction::Continue).unwrap();
            let pin = leader_join.join().unwrap().unwrap();
            assert_eq!(pin.owner().as_object().as_i64().unwrap(), 415, "{phase:?}");
            drop(pin);
            let sibling = arena
                .resolve((1, 0), |_| panic!("follower cancellation reloaded sibling"))
                .unwrap();
            assert_eq!(sibling.pointer(), sibling_pointer, "{phase:?}");
            drop(sibling);
            let terminal = domain.snapshot();
            assert_eq!(terminal.members.calls, 2, "{phase:?}");
            assert_eq!(terminal.members.loads, 1, "{phase:?}");
            assert_eq!(terminal.members.cancellations, 1, "{phase:?}");
            assert_eq!(terminal.dependency_pins, 0, "{phase:?}");
            assert_eq!(terminal.dependency_edges, 0, "{phase:?}");
            assert_eq!(terminal.dependency_edge_adds, 1, "{phase:?}");
            assert_eq!(terminal.dependency_edge_removes, 1, "{phase:?}");
            assert_eq!(terminal.dependency_cleanup_acks, 1, "{phase:?}");
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "{phase:?}");
        }
    }

    #[test]
    fn c2_hook_panic_at_each_post_pin_stage_drops_every_owned_resource() {
        for phase in [
            DependencyEventKind::MemberRequestCreated,
            DependencyEventKind::MemberGranted,
            DependencyEventKind::MemberParseEnter,
            DependencyEventKind::MemberParseExit,
            DependencyEventKind::MemberReconciledBeforePublication,
        ] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (_hooks, entered, action) = dependency_hook(&domain, phase);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let scalar_reader = reader(515);
            let join = thread::spawn(move || {
                request.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared child must hit"),
                    move |permit| load(&scalar_reader, (1, 0), permit),
                )
            });
            assert_eq!(entered.recv().unwrap().kind, phase);
            action.send(PhaseAction::Panic).unwrap();
            assert!(join.join().is_err(), "{phase:?}");
            let terminal = domain.snapshot();
            assert_eq!(terminal.dependency_cancellations, 0, "{phase:?}");
            assert_eq!(terminal.dependency_pins, 0, "{phase:?}");
            assert_eq!(terminal.dependency_edges, 0, "{phase:?}");
            assert_eq!(terminal.dependency_leaders, 0, "{phase:?}");
            assert_eq!(terminal.dependency_edge_adds, 1, "{phase:?}");
            assert_eq!(terminal.dependency_edge_removes, 1, "{phase:?}");
            assert_eq!(terminal.dependency_cleanup_acks, 1, "{phase:?}");
            let operation = broker.snapshot().operations[&arena.epoch()].clone();
            assert_eq!(operation.queued, 0, "{phase:?}");
            assert_eq!(operation.in_flight, 0, "{phase:?}");
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "{phase:?}");
        }
    }

    #[test]
    fn c2_arena_close_at_each_post_pin_stage_cancels_and_drains_exact_resources() {
        for (phase, expected_parses) in [
            (DependencyEventKind::MemberRequestCreated, 0),
            (DependencyEventKind::MemberGranted, 0),
            (DependencyEventKind::MemberParseEnter, 0),
            (DependencyEventKind::MemberParseExit, 1),
            (DependencyEventKind::MemberReconciledBeforePublication, 1),
        ] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (_hooks, entered, action) = dependency_hook(&domain, phase);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let parses = Arc::new(AtomicU64::new(0));
            let parses_for_load = Arc::clone(&parses);
            let scalar_reader = reader(616);
            let join = thread::spawn(move || {
                request.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared child must hit"),
                    move |permit| {
                        parses_for_load.fetch_add(1, Ordering::Relaxed);
                        load(&scalar_reader, (1, 0), permit)
                    },
                )
            });
            assert_eq!(entered.recv().unwrap().kind, phase);
            let (wait_entered, wait_action) = dependency_wait_hook(&domain);
            let closing = arena.clone();
            let close_join = thread::spawn(move || closing.close());
            let (waited_key, _) = wait_entered.recv().unwrap();
            assert_eq!(waited_key.id, member, "{phase:?}");
            action.send(PhaseAction::Continue).unwrap();
            wait_action.send(()).unwrap();
            assert!(join.join().unwrap().is_err(), "{phase:?}");
            close_join.join().unwrap();
            assert_eq!(parses.load(Ordering::Relaxed), expected_parses, "{phase:?}");
            let terminal = domain.snapshot();
            assert_eq!(terminal.cells, 0, "{phase:?}");
            assert_eq!(terminal.dependency_cancellations, 0, "{phase:?}");
            assert_eq!(terminal.dependency_pins, 0, "{phase:?}");
            assert_eq!(terminal.dependency_edges, 0, "{phase:?}");
            assert_eq!(terminal.dependency_edge_adds, 1, "{phase:?}");
            assert_eq!(terminal.dependency_edge_removes, 1, "{phase:?}");
            assert_eq!(terminal.dependency_cleanup_acks, 1, "{phase:?}");
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "{phase:?}");
        }
    }

    #[test]
    fn c2_success_error_and_flight_retry_each_use_one_exact_four_mib_normal_grant() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let hooks = Arc::new(RecordingDependencyHooks {
            events: Mutex::new(Vec::new()),
        });
        domain.set_dependency_hooks(hooks.clone());
        let estimate = Representation::DeclaredObjStmMember.loader_estimate();
        assert_eq!(estimate, 4_194_304);
        let initial_grants = arena.inner.operation.grant_count();
        let success_reader = reader(717);
        let success = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap()
            .resolve_scheduled_member_synthetic(
                container,
                |_| panic!("prepared child must hit"),
                move |permit| {
                    let stats = permit.stats();
                    assert_eq!(stats.limit_bytes, estimate);
                    assert_eq!(stats.current_bytes, 0);
                    assert_eq!(stats.peak_bytes, 0);
                    assert_eq!(stats.reservations, 0);
                    load(&success_reader, (1, 0), permit)
                },
            )
            .unwrap();
        drop(success);
        assert_eq!(arena.inner.operation.grant_count(), initial_grants + 2);
        assert_eq!(
            lock(&hooks.events)
                .iter()
                .filter(|event| event.kind == DependencyEventKind::MemberGranted)
                .count(),
            1
        );

        let failure_id = (member.0 + 10_000, 0);
        let leader = arena
            .inner
            .request_representation(failure_id, Representation::DeclaredObjStmMember)
            .unwrap();
        let follower = arena
            .inner
            .request_representation(failure_id, Representation::DeclaredObjStmMember)
            .unwrap();
        let failure_cell = Arc::clone(&leader.cell);
        let failure_calls = Arc::new(AtomicU64::new(0));
        let failure_calls_for_load = Arc::clone(&failure_calls);
        let leader_join = thread::spawn(move || {
            leader.resolve_scheduled_member_synthetic(
                container,
                |_| panic!("prepared child must hit"),
                move |permit| {
                    assert_eq!(permit.limit_bytes(), estimate);
                    failure_calls_for_load.fetch_add(1, Ordering::Relaxed);
                    Err(transient(failure_id, "scheduled member flight failure"))
                },
            )
        });
        let follower_join = thread::spawn(move || {
            follower.resolve_scheduled_member_synthetic(
                container,
                |_| panic!("failure follower child loader"),
                |_| panic!("failure follower member loader"),
            )
        });
        let first_error = leader_join.join().unwrap().unwrap_err();
        let second_error = follower_join.join().unwrap().unwrap_err();
        assert_eq!(first_error, second_error);
        assert_eq!(failure_calls.load(Ordering::Relaxed), 1);
        assert_eq!(arena.inner.operation.grant_count(), initial_grants + 4);
        assert_eq!(
            lock(&hooks.events)
                .iter()
                .filter(|event| event.kind == DependencyEventKind::MemberGranted)
                .count(),
            2
        );
        let failure_owner = {
            let state = lock(&failure_cell.state);
            let CellPhase::FlightError(owner) = &state.phase else {
                panic!("attached callers must share one flight owner")
            };
            Arc::clone(owner)
        };
        assert_eq!(Arc::strong_count(&failure_owner), 2);
        drop(failure_owner);
        drop(failure_cell);
        let after_error = broker.snapshot();
        assert_eq!(after_error.completion_reserve_bytes, 0);
        assert_eq!(after_error.oversize_bytes, 0);
        assert_eq!(after_error.oversize_owners, 0);

        let retry_reader = reader(818);
        let retry = arena
            .inner
            .request_representation(failure_id, Representation::DeclaredObjStmMember)
            .unwrap()
            .resolve_scheduled_member_synthetic(
                container,
                |_| panic!("prepared child must hit"),
                move |permit| load(&retry_reader, (1, 0), permit),
            )
            .unwrap();
        assert_eq!(retry.owner().as_object().as_i64().unwrap(), 818);
        assert_eq!(arena.inner.operation.grant_count(), initial_grants + 6);
        assert_eq!(
            lock(&hooks.events)
                .iter()
                .filter(|event| event.kind == DependencyEventKind::MemberGranted)
                .count(),
            3
        );
        let terminal = domain.snapshot();
        assert_eq!(terminal.members.loads, 3);
        assert_eq!(terminal.members.negative_hits, 0);
        drop(retry);
        arena.close();
        drop(arena);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn c2_cancelled_leader_at_each_post_pin_stage_cleans_before_oldest_successor() {
        for (phase, expected_loads) in [
            (DependencyEventKind::MemberRequestCreated, 1),
            (DependencyEventKind::MemberGranted, 1),
            (DependencyEventKind::MemberParseEnter, 2),
            (DependencyEventKind::MemberParseExit, 2),
            (DependencyEventKind::MemberReconciledBeforePublication, 2),
        ] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let (_first_hooks, first_entered, first_action) = dependency_hook(&domain, phase);
            let leader = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let leader_cancel = leader.cancellation_handle();
            let oldest = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let oldest_slot = oldest.slot;
            let later = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let cancelled_reader = reader(918);
            let leader_join = thread::spawn(move || {
                leader.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared child must hit"),
                    move |permit| load(&cancelled_reader, (1, 0), permit),
                )
            });
            let first = first_entered.recv().unwrap();
            assert_eq!(first.kind, phase);
            let successor_generation = first.token.generation + 1;
            let (successor_hooks, successor_entered, successor_action) =
                dependency_hook_for_generation(
                    &domain,
                    DependencyEventKind::BeforeChildInstall,
                    successor_generation,
                );
            let (wait_entered, wait_action) = dependency_wait_hook(&domain);
            let cancel_join = thread::spawn(move || leader_cancel.cancel());
            let (waited_key, _) = wait_entered.recv().unwrap();
            assert_eq!(waited_key.id, member, "{phase:?}");
            first_action.send(PhaseAction::Continue).unwrap();
            wait_action.send(()).unwrap();
            cancel_join.join().unwrap();
            let scalar_reader = reader(919);
            let oldest_join = thread::spawn(move || {
                oldest.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared child must hit"),
                    move |permit| load(&scalar_reader, (1, 0), permit),
                )
            });
            let later_join = thread::spawn(move || {
                later.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("later follower cannot resolve child"),
                    |_| panic!("later follower cannot parse"),
                )
            });
            let successor = successor_entered.recv().unwrap();
            assert_eq!(
                successor.token.generation, successor_generation,
                "{phase:?}"
            );
            assert_eq!(successor.token.leader_slot, oldest_slot, "{phase:?}");
            let successor_kinds = lock(&successor_hooks.events)
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            let cleanup_ack = successor_kinds
                .iter()
                .position(|kind| *kind == DependencyEventKind::CleanupAcknowledged)
                .unwrap();
            let successor_start = successor_kinds
                .iter()
                .position(|kind| *kind == DependencyEventKind::BeforeChildInstall)
                .unwrap();
            assert!(cleanup_ack < successor_start, "{phase:?}");
            successor_action.send(PhaseAction::Continue).unwrap();
            assert!(leader_join.join().unwrap().is_err(), "{phase:?}");
            let oldest_pin = oldest_join.join().unwrap().unwrap();
            let later_pin = later_join.join().unwrap().unwrap();
            assert_eq!(oldest_pin.pointer(), later_pin.pointer(), "{phase:?}");
            assert_eq!(
                oldest_pin.owner().as_object().as_i64().unwrap(),
                919,
                "{phase:?}"
            );
            drop(oldest_pin);
            drop(later_pin);
            let terminal = domain.snapshot();
            assert_eq!(terminal.members.calls, 3, "{phase:?}");
            assert_eq!(terminal.members.loads, expected_loads, "{phase:?}");
            assert_eq!(terminal.members.cancellations, 1, "{phase:?}");
            assert_eq!(terminal.dependency_edge_adds, 2, "{phase:?}");
            assert_eq!(terminal.dependency_edge_removes, 2, "{phase:?}");
            assert_eq!(terminal.dependency_cleanup_acks, 2, "{phase:?}");
            assert_eq!(terminal.dependency_edges, 0, "{phase:?}");
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "{phase:?}");
        }
    }

    #[test]
    fn c2_ready_member_hits_after_its_exact_older_container_is_evicted() {
        let oracle_broker = broker();
        let oracle_domain = ObjectCellDomain::new(
            oracle_broker.clone(),
            ObjectCellConfig::scaled(32 * 1024 * 1024),
        );
        let oracle_arena = oracle_domain.open_arena().unwrap();
        let (oracle_reader, oracle_member, oracle_container, _) = object_stream_reader();
        let oracle_child = Arc::clone(&oracle_reader);
        let oracle_scalar = Arc::clone(&oracle_reader);
        let oracle_pin = oracle_arena
            .inner
            .request_representation(oracle_member, Representation::DeclaredObjStmMember)
            .unwrap()
            .resolve_scheduled_member_synthetic(
                oracle_container,
                move |permit| {
                    oracle_child
                        .prepare_object_stream_with_permit(oracle_container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                oracle_container,
                                error,
                            ))
                        })
                },
                move |permit| load(&oracle_scalar, oracle_member, permit),
            )
            .unwrap();
        drop(oracle_pin);
        let exact_target = {
            let domain = lock(&oracle_domain.inner.state);
            let container = domain
                .cells
                .iter()
                .find(|(key, _)| {
                    key.id == oracle_container
                        && key.representation == Representation::DeclaredObjStmContainer
                })
                .unwrap();
            let member = domain
                .cells
                .iter()
                .find(|(key, _)| {
                    key.id == oracle_member
                        && key.representation == Representation::DeclaredObjStmMember
                })
                .unwrap();
            let container_weight = lock(&container.1.state).completed_weight;
            let member_weight = lock(&member.1.state).completed_weight;
            container_weight + member_weight
        };
        oracle_arena.close();
        drop(oracle_arena);
        assert_eq!(oracle_broker.snapshot().aggregate_bytes, 0);

        let broker = broker();
        let domain = ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(exact_target));
        let arena = domain.open_arena().unwrap();
        let (stream_reader, member, container, _) = object_stream_reader();
        assert_eq!(member, oracle_member);
        assert_eq!(container, oracle_container);
        let child_reader = Arc::clone(&stream_reader);
        let member_reader = Arc::clone(&stream_reader);
        let member_pin = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap()
            .resolve_scheduled_member_synthetic(
                container,
                move |permit| {
                    child_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                },
                move |permit| load(&member_reader, member, permit),
            )
            .unwrap();
        let member_pointer = member_pin.pointer();
        drop(member_pin);
        let before_pressure = domain.snapshot();
        assert_eq!(before_pressure.cache_bytes, exact_target);
        let raw_reader = reader(1_020);
        let pressure = arena
            .resolve((50_000, 0), move |permit| load(&raw_reader, (1, 0), permit))
            .unwrap();
        drop(pressure);
        {
            let state = lock(&domain.inner.state);
            assert!(!state.cells.keys().any(|key| {
                key.id == container && key.representation == Representation::DeclaredObjStmContainer
            }));
            assert!(state.cells.keys().any(|key| {
                key.id == member && key.representation == Representation::DeclaredObjStmMember
            }));
        }
        let after_pressure = domain.snapshot();
        assert_eq!(after_pressure.containers.evictions, 1);
        let container_calls = after_pressure.containers.calls;
        domain.set_dependency_hooks(Arc::new(PanickingDependencyHooks));
        let hit = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap()
            .resolve_scheduled_member_synthetic(
                container,
                |_| panic!("evicted container must be irrelevant to Ready member"),
                |_| panic!("Ready member must not parse after container eviction"),
            )
            .unwrap();
        assert_eq!(hit.pointer(), member_pointer);
        let hit_snapshot = domain.snapshot();
        assert_eq!(hit_snapshot.containers.calls, container_calls);
        assert_eq!(hit_snapshot.members.hits, 1);
        assert_eq!(hit_snapshot.dependency_edges, 0);
        drop(hit);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn c2_queue_full_after_pin_publishes_one_shared_flight_error_then_retries() {
        let broker = BudgetBroker::new(crate::broker::BrokerConfig {
            normal_limit: 128 * 1024 * 1024,
            oversize_limit: 64 * 1024 * 1024,
            completion_reserve_limit: 32 * 1024 * 1024,
            queue_metadata_weight: crate::broker::QUEUE_METADATA_WEIGHT,
            operation_metadata_weight: crate::broker::OPERATION_METADATA_WEIGHT,
            max_active_operations: 128,
            max_queued_requests: 2,
        })
        .unwrap();
        let domain =
            ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
        let delivery_hooks = Arc::new(RecordingFailureDeliveryHooks {
            deliveries: Mutex::new(Vec::new()),
        });
        domain.set_failure_delivery_hooks(delivery_hooks.clone());
        let arena = domain.open_arena().unwrap();
        let (stream_reader, member, container, _) = object_stream_reader();
        let (_hooks, entered, action) =
            dependency_hook(&domain, DependencyEventKind::BeforeMemberAdmission);
        let leader = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let follower = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let failure_cell = Arc::clone(&leader.cell);
        let parses = Arc::new(AtomicU64::new(0));
        let parses_for_load = Arc::clone(&parses);
        let child_reader = Arc::clone(&stream_reader);
        let leader_join = thread::spawn(move || {
            leader.resolve_scheduled_member_synthetic(
                container,
                move |permit| {
                    child_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                },
                move |_| {
                    parses_for_load.fetch_add(1, Ordering::Relaxed);
                    panic!("QueueFull must precede parser entry")
                },
            )
        });
        let follower_join = thread::spawn(move || {
            follower.resolve_scheduled_member_synthetic(
                container,
                |_| panic!("failure follower child loader"),
                |_| panic!("failure follower member loader"),
            )
        });
        assert_eq!(
            entered.recv().unwrap().kind,
            DependencyEventKind::BeforeMemberAdmission
        );
        let blocker_operation = broker.register_operation().unwrap();
        let waiting_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                80 * 1024 * 1024,
            )
            .unwrap();
        let queued = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                20 * 1024 * 1024,
            )
            .unwrap();
        let saturated = broker.snapshot();
        assert_eq!(saturated.live_request_records, 2);
        assert_eq!(saturated.queued, 1);
        assert_eq!(saturated.oversize_bytes, 0);
        assert_eq!(saturated.oversize_owners, 0);
        assert_eq!(saturated.completion_reserve_bytes, 0);
        let member_before = saturated.operations[&arena.epoch()].clone();
        action.send(PhaseAction::Continue).unwrap();
        let first_error = leader_join.join().unwrap().unwrap_err();
        let second_error = follower_join.join().unwrap().unwrap_err();
        assert_eq!(first_error, second_error);
        assert!(first_error.detail.contains("queue is full"));
        assert_eq!(parses.load(Ordering::Relaxed), 0);
        let owner = {
            let state = lock(&failure_cell.state);
            let CellPhase::FlightError(owner) = &state.phase else {
                panic!("QueueFull must publish one flight owner")
            };
            assert!(owner.cell_envelope);
            assert!(owner.retained_weight <= CELL_ERROR_ENVELOPE_BYTES);
            assert!(lock(&owner.charge).is_none());
            assert!(lock(&owner._reservation).is_none());
            Arc::clone(owner)
        };
        let owner_pointer = Arc::as_ptr(&owner) as usize;
        let deliveries = lock(&delivery_hooks.deliveries);
        assert_eq!(deliveries.len(), 2);
        assert!(deliveries.iter().all(|(key, _)| *key == failure_cell.key));
        assert!(deliveries
            .iter()
            .all(|(_, pointer)| *pointer == owner_pointer));
        drop(deliveries);
        assert_eq!(Arc::strong_count(&owner), 2);
        drop(owner);
        drop(failure_cell);
        let refusal = domain.snapshot();
        assert_eq!(refusal.members.loads, 0);
        assert_eq!(refusal.members.negative_hits, 0);
        assert_eq!(refusal.dependency_pins, 0);
        assert_eq!(refusal.dependency_edges, 0);
        let refused_broker = broker.snapshot();
        assert_eq!(
            broker_snapshot_delta(&saturated, &refused_broker),
            expected_bounded_flight_refusal_delta(&saturated, arena.epoch(), 1, false),
            "QueueFull refusal must change only the frozen failure-owner accounting fields"
        );
        let refused_member = refused_broker.operations[&arena.epoch()].clone();
        assert_eq!(refused_broker.denials, saturated.denials + 1);
        assert_eq!(refused_broker.grants, saturated.grants);
        assert_eq!(refused_broker.oversize_bytes, saturated.oversize_bytes);
        assert_eq!(refused_broker.oversize_owners, saturated.oversize_owners);
        assert_eq!(
            refused_broker.completion_reserve_bytes,
            saturated.completion_reserve_bytes
        );
        assert_eq!(refused_member.grants, member_before.grants);
        assert_eq!(refused_member.granted_bytes, member_before.granted_bytes);
        assert_eq!(refused_member.queued, 0);
        assert_eq!(refused_member.in_flight, 0);
        assert_eq!(refused_member.error_owners, 0);

        queued.cancel();
        if let Ok(reservation) = queued.wait() {
            reservation.cancel();
            drop(reservation);
        }
        drop(blocker);
        waiting_operation.close();
        blocker_operation.close();
        let retry_reader = Arc::clone(&stream_reader);
        let retry = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap()
            .resolve_scheduled_member_synthetic(
                container,
                |_| panic!("container remained cached after refusal"),
                move |permit| load(&retry_reader, member, permit),
            )
            .unwrap();
        assert_eq!(
            retry
                .owner()
                .as_object()
                .as_dict()
                .unwrap()
                .get(b"Answer")
                .unwrap()
                .as_i64()
                .unwrap(),
            42
        );
        let terminal = domain.snapshot();
        assert_eq!(terminal.members.loads, 1);
        assert_eq!(terminal.members.negative_hits, 0);
        drop(retry);
        arena.close();
        drop(arena);
        assert_gate4_terminal(&domain, &broker, 1);
    }

    #[test]
    fn c2_exact_and_one_byte_short_four_mib_capacity_are_granted_and_queued_exactly() {
        for (shortfall, producer_error) in [(0u64, false), (0, true), (1, false), (1, true)] {
            let broker = BudgetBroker::new(crate::broker::BrokerConfig {
                normal_limit: 128 * 1024 * 1024,
                oversize_limit: 64 * 1024 * 1024,
                completion_reserve_limit: 0,
                queue_metadata_weight: crate::broker::QUEUE_METADATA_WEIGHT,
                operation_metadata_weight: crate::broker::OPERATION_METADATA_WEIGHT,
                max_active_operations: 128,
                max_queued_requests: 128,
            })
            .unwrap();
            let blocker_operation = broker.register_operation().unwrap();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let arena = domain.open_arena().unwrap();
            let (stream_reader, member, container, _) = object_stream_reader();
            let (_pin_hooks, pin_entered, pin_action) =
                dependency_hook(&domain, DependencyEventKind::BeforeMemberAdmission);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let flight_cell = Arc::clone(&request.cell);
            let cancellation = request.cancellation_handle();
            let parses = Arc::new(AtomicU64::new(0));
            let parses_for_load = Arc::clone(&parses);
            let child_reader = Arc::clone(&stream_reader);
            let member_reader = Arc::clone(&stream_reader);
            let join = thread::spawn(move || {
                request.resolve_scheduled_member_synthetic(
                    container,
                    move |permit| {
                        child_reader
                            .prepare_object_stream_with_permit(container, permit)
                            .map_err(|error| {
                                CellLoadError::objstm(crate::objstm_failures::classify(
                                    container, error,
                                ))
                            })
                    },
                    move |permit| {
                        parses_for_load.fetch_add(1, Ordering::Relaxed);
                        if producer_error {
                            Err(transient(member, "exact-budget producer error"))
                        } else {
                            load(&member_reader, member, permit)
                        }
                    },
                )
            });
            pin_entered.recv().unwrap();

            let before_probe = broker.snapshot();
            let probe = blocker_operation
                .reserve(
                    Lane::Normal {
                        completion_reserve: 0,
                    },
                    1,
                )
                .unwrap();
            let probe_snapshot = broker.snapshot();
            let reservation_metadata = probe_snapshot.metadata_bytes - before_probe.metadata_bytes;
            drop(probe);
            let baseline = broker.snapshot();
            let estimate = Representation::DeclaredObjStmMember.loader_estimate();
            let self_pinned = baseline.operations[&arena.epoch()].self_pinned_bytes;
            let self_pin_adjustment = u64::from(shortfall == 1) * self_pinned;
            let blocker_bytes = baseline
                .normal_limit_bytes
                .checked_add(shortfall + self_pin_adjustment)
                .and_then(|bytes| bytes.checked_sub(baseline.normal_payload_bytes))
                .and_then(|bytes| bytes.checked_sub(baseline.metadata_bytes))
                .and_then(|bytes| bytes.checked_sub(reservation_metadata))
                .and_then(|bytes| bytes.checked_sub(crate::broker::QUEUE_METADATA_WEIGHT))
                .and_then(|bytes| bytes.checked_sub(estimate))
                .unwrap();
            let blocker = blocker_operation
                .reserve(
                    Lane::Normal {
                        completion_reserve: 0,
                    },
                    blocker_bytes,
                )
                .unwrap();
            let held = broker.snapshot();
            assert_eq!(
                held.metadata_bytes,
                baseline.metadata_bytes + reservation_metadata
            );
            assert_eq!(
                held.normal_payload_bytes
                    + held.metadata_bytes
                    + crate::broker::QUEUE_METADATA_WEIGHT
                    + estimate,
                held.normal_limit_bytes + shortfall + self_pin_adjustment
            );
            assert_eq!(held.normal_in_flight_estimate_bytes, blocker_bytes);
            assert!(held.normal_in_flight_estimate_bytes + estimate <= held.normal_limit_bytes);
            let admission_event = if shortfall == 0 {
                DependencyEventKind::MemberRequestCreated
            } else {
                DependencyEventKind::MemberQueued
            };
            let (_queued_hooks, queued_entered, queued_action) =
                dependency_hook(&domain, admission_event);
            pin_action.send(PhaseAction::Continue).unwrap();
            queued_entered.recv().unwrap();
            let admission = broker.snapshot();
            assert_eq!(
                broker_snapshot_delta(&held, &admission),
                expected_member_request_delta(&held, arena.epoch(), shortfall == 1),
                "M/M-1 admission must match the exhaustive broker ledger delta"
            );
            let member_operation = admission.operations[&arena.epoch()].clone();
            if shortfall == 0 {
                assert_eq!(member_operation.queued, 0);
                assert_eq!(member_operation.in_flight, 1);
                queued_action.send(PhaseAction::Continue).unwrap();
                let result = join.join().unwrap();
                let completion = if producer_error {
                    assert!(result.is_err());
                    let state = lock(&flight_cell.state);
                    let CellPhase::FlightError(owner) = &state.phase else {
                        panic!("producer error must publish a FlightError owner")
                    };
                    Err(owner.retained_weight)
                } else {
                    let pin = result.unwrap();
                    let retained = pin.owner().retained_bytes();
                    drop(pin);
                    Ok(retained)
                };
                assert_eq!(parses.load(Ordering::Relaxed), 1);
                drop(flight_cell);
                let completed = broker.snapshot();
                assert_eq!(
                    broker_snapshot_delta(&admission, &completed),
                    expected_member_completion_delta(
                        &admission,
                        arena.epoch(),
                        completion,
                        completion.err().unwrap_or(0),
                    ),
                    "success/error completion must match the exhaustive reconciliation ledger delta"
                );
            } else {
                assert_eq!(member_operation.queued, 1);
                assert_eq!(member_operation.in_flight, 0);
                let (wait_entered, wait_action) = dependency_wait_hook(&domain);
                let cancel_join = thread::spawn(move || cancellation.cancel());
                wait_entered.recv().unwrap();
                queued_action.send(PhaseAction::Continue).unwrap();
                wait_action.send(()).unwrap();
                cancel_join.join().unwrap();
                assert!(join.join().unwrap().is_err());
                assert_eq!(parses.load(Ordering::Relaxed), 0);
                drop(flight_cell);
            }
            drop(blocker);
            let terminal = domain.snapshot();
            assert_eq!(terminal.members.loads, u64::from(shortfall == 0));
            assert_eq!(terminal.dependency_edges, 0);
            blocker_operation.close();
            arena.close();
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }
    }

    #[test]
    fn c2_retained_exact_grants_success_and_error_while_m_minus_one_shares_flight_refusal() {
        for (shortfall, producer_error) in [(0u64, false), (0, true), (1, false), (1, true)] {
            let broker = BudgetBroker::new(crate::broker::BrokerConfig {
                normal_limit: 128 * 1024 * 1024,
                oversize_limit: 64 * 1024 * 1024,
                completion_reserve_limit: 0,
                queue_metadata_weight: crate::broker::QUEUE_METADATA_WEIGHT,
                operation_metadata_weight: crate::broker::OPERATION_METADATA_WEIGHT,
                max_active_operations: 128,
                max_queued_requests: 128,
            })
            .unwrap();
            let blocker_operation = broker.register_operation().unwrap();
            let domain =
                ObjectCellDomain::new(broker.clone(), ObjectCellConfig::scaled(32 * 1024 * 1024));
            let delivery_hooks = Arc::new(RecordingFailureDeliveryHooks {
                deliveries: Mutex::new(Vec::new()),
            });
            domain.set_failure_delivery_hooks(delivery_hooks.clone());
            let arena = domain.open_arena().unwrap();
            let (stream_reader, member, container, _) = object_stream_reader();
            let (_pin_hooks, pin_entered, pin_action) =
                dependency_hook(&domain, DependencyEventKind::BeforeMemberAdmission);
            let leader = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let follower = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let mut flight_cell = Some(Arc::clone(&leader.cell));
            let parses = Arc::new(AtomicU64::new(0));
            let parses_for_load = Arc::clone(&parses);
            let child_reader = Arc::clone(&stream_reader);
            let member_reader = Arc::clone(&stream_reader);
            let leader_join = thread::spawn(move || {
                leader.resolve_scheduled_member_synthetic(
                    container,
                    move |permit| {
                        child_reader
                            .prepare_object_stream_with_permit(container, permit)
                            .map_err(|error| {
                                CellLoadError::objstm(crate::objstm_failures::classify(
                                    container, error,
                                ))
                            })
                    },
                    move |permit| {
                        parses_for_load.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(permit.limit_bytes(), 4_194_304);
                        let stats = permit.stats();
                        assert_eq!(stats.current_bytes, 0);
                        assert_eq!(stats.peak_bytes, 0);
                        if producer_error {
                            Err(transient(member, "retained exact producer error"))
                        } else {
                            load(&member_reader, member, permit)
                        }
                    },
                )
            });
            let follower_join = thread::spawn(move || {
                follower.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("retained follower cannot resolve child"),
                    |_| panic!("retained follower cannot produce"),
                )
            });
            pin_entered.recv().unwrap();
            let baseline = broker.snapshot();
            let estimate = Representation::DeclaredObjStmMember.loader_estimate();
            let payload_headroom = baseline.normal_limit_bytes
                - (baseline.normal_payload_bytes
                    + baseline.completion_reserve_bytes
                    + baseline.metadata_bytes
                    + crate::broker::QUEUE_METADATA_WEIGHT);
            let blocker_bytes = payload_headroom - (estimate - shortfall);
            let blocker_reservation = blocker_operation
                .reserve(
                    Lane::Normal {
                        completion_reserve: 0,
                    },
                    blocker_bytes,
                )
                .unwrap();
            let blocker = blocker_reservation.reconcile(blocker_bytes).unwrap();
            let held = broker.snapshot();
            assert_eq!(held.normal_in_flight_estimate_bytes, 0);
            assert_eq!(held.metadata_bytes, baseline.metadata_bytes);
            assert_eq!(
                held.normal_limit_bytes
                    - (held.normal_payload_bytes
                        + held.completion_reserve_bytes
                        + held.metadata_bytes
                        + crate::broker::QUEUE_METADATA_WEIGHT),
                estimate - shortfall
            );

            if shortfall == 0 {
                let (_request_hooks, requested, requested_action) =
                    dependency_hook(&domain, DependencyEventKind::MemberRequestCreated);
                pin_action.send(PhaseAction::Continue).unwrap();
                requested.recv().unwrap();
                let admitted = broker.snapshot();
                assert_eq!(
                    broker_snapshot_delta(&held, &admitted),
                    expected_member_request_delta(&held, arena.epoch(), false),
                    "retained exact admission must match the exhaustive broker ledger delta"
                );
                let operation = admitted.operations[&arena.epoch()].clone();
                assert_eq!(operation.queued, 0);
                assert_eq!(operation.in_flight, 1);
                assert_eq!(admitted.denials, held.denials);
                requested_action.send(PhaseAction::Continue).unwrap();
                let leader_result = leader_join.join().unwrap();
                let follower_result = follower_join.join().unwrap();
                assert_eq!(parses.load(Ordering::Relaxed), 1);
                let completion = if producer_error {
                    assert_eq!(leader_result.unwrap_err(), follower_result.unwrap_err());
                    let state = lock(&flight_cell.as_ref().unwrap().state);
                    let CellPhase::FlightError(owner) = &state.phase else {
                        panic!("retained producer error must publish a FlightError owner")
                    };
                    Err(owner.retained_weight)
                } else {
                    let leader_pin = leader_result.unwrap();
                    let follower_pin = follower_result.unwrap();
                    assert_eq!(leader_pin.pointer(), follower_pin.pointer());
                    let retained = leader_pin.owner().retained_bytes();
                    drop(leader_pin);
                    drop(follower_pin);
                    Ok(retained)
                };
                drop(flight_cell.take());
                let completed = broker.snapshot();
                assert_eq!(
                    broker_snapshot_delta(&admitted, &completed),
                    expected_member_completion_delta(
                        &admitted,
                        arena.epoch(),
                        completion,
                        match completion {
                            Ok(retained) => retained,
                            Err(failure_weight) => CELL_METADATA_BYTES + failure_weight,
                        },
                    ),
                    "retained success/error completion must match the exhaustive reconciliation ledger delta"
                );
            } else {
                pin_action.send(PhaseAction::Continue).unwrap();
                let first_error = leader_join.join().unwrap().unwrap_err();
                let second_error = follower_join.join().unwrap().unwrap_err();
                assert_eq!(first_error, second_error);
                assert_eq!(
                    first_error.detail,
                    CellControlTag::LoaderHeadroomUnavailable.detail()
                );
                assert_eq!(parses.load(Ordering::Relaxed), 0);
                let refused = broker.snapshot();
                assert_eq!(
                    broker_snapshot_delta(&held, &refused),
                    expected_bounded_flight_refusal_delta(&held, arena.epoch(), 0, true),
                    "retained M-1 refusal must change only the frozen failure-owner accounting fields"
                );
                assert_eq!(refused.queued, held.queued);
                assert_eq!(refused.in_flight, held.in_flight);
                assert_eq!(refused.grants, held.grants);
                let owner = {
                    let state = lock(&flight_cell.as_ref().unwrap().state);
                    let CellPhase::FlightError(owner) = &state.phase else {
                        panic!("retained M-1 must publish a FlightError")
                    };
                    assert!(owner.cell_envelope);
                    assert!(owner.retained_weight <= CELL_ERROR_ENVELOPE_BYTES);
                    assert!(lock(&owner.charge).is_none());
                    assert!(lock(&owner._reservation).is_none());
                    Arc::clone(owner)
                };
                let owner_pointer = Arc::as_ptr(&owner) as usize;
                let deliveries = lock(&delivery_hooks.deliveries);
                assert_eq!(deliveries.len(), 2);
                assert!(deliveries
                    .iter()
                    .all(|(key, _)| *key == flight_cell.as_ref().unwrap().key));
                assert!(deliveries
                    .iter()
                    .all(|(_, pointer)| *pointer == owner_pointer));
                drop(deliveries);
                assert_eq!(Arc::strong_count(&owner), 2);
                drop(owner);
                drop(blocker);
                blocker_operation.close();
                let retry_reader = Arc::clone(&stream_reader);
                let retry = arena
                    .inner
                    .request_representation(member, Representation::DeclaredObjStmMember)
                    .unwrap()
                    .resolve_scheduled_member_synthetic(
                        container,
                        |_| panic!("retained-refusal retry container must hit"),
                        move |permit| load(&retry_reader, member, permit),
                    )
                    .unwrap();
                drop(retry);
                let retried = domain.snapshot();
                assert_eq!(retried.members.negative_hits, 0);
                assert_eq!(retried.members.loads, 1);
                drop(flight_cell);
                arena.close();
                drop(arena);
                assert_gate4_terminal(&domain, &broker, 1);
                continue;
            }
            drop(blocker);
            blocker_operation.close();
            drop(flight_cell);
            arena.close();
            drop(arena);
            assert_gate4_terminal(&domain, &broker, 1);
        }
    }

    #[test]
    fn c2_scheduled_success_that_cannot_cache_closes_instead_of_returning_bypass_ready() {
        let broker = broker();
        let domain = ObjectCellDomain::new(
            broker.clone(),
            ObjectCellConfig::scaled(CELL_METADATA_BYTES),
        );
        let arena = domain.open_arena().unwrap();
        let (stream_reader, member, container, _) = object_stream_reader();
        let child_reader = Arc::clone(&stream_reader);
        let member_reader = Arc::clone(&stream_reader);
        let error = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap()
            .resolve_scheduled_member_synthetic(
                container,
                move |permit| {
                    child_reader
                        .prepare_object_stream_with_permit(container, permit)
                        .map_err(|error| {
                            CellLoadError::objstm(crate::objstm_failures::classify(
                                container, error,
                            ))
                        })
                },
                move |permit| load(&member_reader, member, permit),
            )
            .unwrap_err();
        assert_eq!(
            error.detail,
            CellControlTag::ObjectStreamCacheBypassInvariant.detail()
        );
        let snapshot = domain.snapshot();
        assert!(snapshot.invariant_failed);
        assert_eq!(snapshot.ready, 0);
        assert_eq!(snapshot.dependency_pins, 0);
        assert_eq!(snapshot.dependency_edges, 0);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn c2_stale_install_take_and_publication_at_each_stage_cannot_mutate_exact_flight() {
        for (phase, permit_present) in [
            (DependencyEventKind::MemberRequestCreated, false),
            (DependencyEventKind::MemberGranted, true),
            (DependencyEventKind::MemberParseEnter, true),
            (DependencyEventKind::MemberParseExit, true),
            (
                DependencyEventKind::MemberReconciledBeforePublication,
                false,
            ),
        ] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let sibling_reader = reader(1_110);
            let sibling = arena
                .resolve((1, 0), |permit| load(&sibling_reader, (1, 0), permit))
                .unwrap();
            let sibling_pointer = sibling.pointer();
            drop(sibling);
            let (_hooks, entered, action) = dependency_hook(&domain, phase);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let cell = Arc::clone(&request.cell);
            let scalar_reader = reader(1_111);
            let join = thread::spawn(move || {
                request.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared child must hit"),
                    move |permit| load(&scalar_reader, (1, 0), permit),
                )
            });
            assert_eq!(entered.recv().unwrap().kind, phase);
            let (token, pin_pointer, cancellation_present) = {
                let state = lock(&cell.state);
                let CellPhase::Loading(loading) = &state.phase else {
                    panic!("paused stage must retain Loading: {phase:?}")
                };
                let dependency = loading.dependency.as_ref().unwrap();
                assert_eq!(loading.permit.is_some(), permit_present, "{phase:?}");
                (
                    dependency.token,
                    dependency.pin.as_ref().unwrap().pointer(),
                    dependency.cancellation.is_some(),
                )
            };
            let stale = DependencyLeaderToken {
                generation: token.generation + 1,
                ..token
            };
            let attack_permit = ScalarResolutionPermit::new(4_194_304);
            let (attack_permit, install_error) = arena
                .inner
                .install_member_permit(&cell, stale, attack_permit)
                .expect_err("stale permit installed into exact flight");
            assert_eq!(
                install_error.detail,
                CellControlDetail::Static(CellControlTag::DependencyRejectedStale),
                "{phase:?}"
            );
            drop(attack_permit);
            assert!(
                arena
                    .inner
                    .take_member_permit_after_attempt(&cell, stale)
                    .unwrap()
                    .is_none(),
                "{phase:?}"
            );

            let attack_operation = broker.register_operation().unwrap();
            let attack_reservation = attack_operation
                .reserve(
                    Lane::Normal {
                        completion_reserve: 0,
                    },
                    4_194_304,
                )
                .unwrap();
            let attack_scalar_reader = reader(1_212);
            let attack_scalar_permit = ScalarResolutionPermit::new(4_194_304);
            let attack_object = load(&attack_scalar_reader, (1, 0), &attack_scalar_permit).unwrap();
            let attack_charge = attack_reservation
                .reconcile(attack_object.retained_bytes())
                .unwrap();
            let attack_owner = Arc::new(ResolvedObjectOwner {
                payload: CellPayload::Object(attack_object),
                permit: test_clone_permit(&attack_scalar_permit),
                transition_gate: Mutex::new(()),
                cache_backed: AtomicBool::new(false),
                charge: Mutex::new(Some(attack_charge)),
                self_pin: Mutex::new(None),
            });
            let (wait_entered, wait_action) = dependency_wait_hook(&domain);
            let attack_inner = Arc::clone(&arena.inner);
            let attack_cell = Arc::clone(&cell);
            let attack_join = thread::spawn(move || {
                attack_inner.publish_ready(
                    &attack_cell,
                    stale.leader_slot,
                    stale.generation,
                    attack_owner,
                    true,
                );
            });
            let (waited_key, waited_transition) = wait_entered.recv().unwrap();
            assert_eq!(waited_key, cell.key, "{phase:?}");
            assert_eq!(
                waited_transition,
                DependencyTransition::Announcing { token, kind: phase },
                "{phase:?}"
            );
            {
                let state = lock(&cell.state);
                let CellPhase::Loading(loading) = &state.phase else {
                    panic!("stale publication replaced exact Loading: {phase:?}")
                };
                let dependency = loading.dependency.as_ref().unwrap();
                assert_eq!(loading.generation, token.generation, "{phase:?}");
                assert_eq!(loading.leader_slot, token.leader_slot, "{phase:?}");
                assert_eq!(loading.permit.is_some(), permit_present, "{phase:?}");
                assert_eq!(dependency.token, token, "{phase:?}");
                assert_eq!(
                    dependency.pin.as_ref().unwrap().pointer(),
                    pin_pointer,
                    "{phase:?}"
                );
                assert_eq!(
                    dependency.cancellation.is_some(),
                    cancellation_present,
                    "{phase:?}"
                );
            }
            action.send(PhaseAction::Continue).unwrap();
            wait_action.send(()).unwrap();
            attack_join.join().unwrap();
            attack_operation.close();
            let pin = join.join().unwrap().unwrap();
            assert_eq!(
                pin.owner().as_object().as_i64().unwrap(),
                1_111,
                "{phase:?}"
            );
            drop(pin);
            let sibling = arena
                .resolve((1, 0), |_| {
                    panic!("stale attack reloaded unrelated sibling")
                })
                .unwrap();
            assert_eq!(sibling.pointer(), sibling_pointer, "{phase:?}");
            drop(sibling);
            drop(cell);
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "{phase:?}");
        }
    }

    #[test]
    fn c2_exact_duplicate_permit_invariant_tears_down_only_its_token() {
        let (broker, domain, arena, member, container) = prepared_dependency_fixture();
        let request = arena
            .inner
            .request_representation(member, Representation::DeclaredObjStmMember)
            .unwrap();
        let cell = Arc::clone(&request.cell);
        let (parse_entered_tx, parse_entered_rx) = mpsc::sync_channel(0);
        let (parse_release_tx, parse_release_rx) = mpsc::sync_channel(0);
        let scalar_reader = reader(1_313);
        let join = thread::spawn(move || {
            request.resolve_scheduled_member_synthetic(
                container,
                |_| panic!("prepared child must hit"),
                move |permit| {
                    parse_entered_tx.send(()).unwrap();
                    parse_release_rx.recv().unwrap();
                    load(&scalar_reader, (1, 0), permit)
                },
            )
        });
        parse_entered_rx.recv().unwrap();
        let token = {
            let state = lock(&cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                panic!("paused parser must retain Loading")
            };
            loading.dependency.as_ref().unwrap().token
        };
        let duplicate = ScalarResolutionPermit::new(4_194_304);
        let (duplicate, invariant) = arena
            .inner
            .install_member_permit(&cell, token, duplicate)
            .expect_err("duplicate exact permit did not trip invariant");
        assert_eq!(
            invariant.detail,
            CellControlDetail::Static(CellControlTag::DependencyStateInvariant)
        );
        drop(duplicate);
        assert!(arena.inner.close_exact_control(
            &cell,
            token.leader_slot,
            token.generation,
            invariant,
            true,
        ));
        parse_release_tx.send(()).unwrap();
        let error = join.join().unwrap().unwrap_err();
        assert_eq!(
            error.detail,
            CellControlTag::DependencyStateInvariant.detail()
        );
        let terminal = domain.snapshot();
        assert!(terminal.invariant_failed);
        assert_eq!(terminal.dependency_pins, 0);
        assert_eq!(terminal.dependency_edges, 0);
        assert_eq!(terminal.dependency_edge_adds, 1);
        assert_eq!(terminal.dependency_edge_removes, 1);
        assert_eq!(terminal.dependency_cleanup_acks, 1);
        drop(cell);
        arena.close();
        drop(arena);
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn c2_exact_invariant_teardown_covers_each_owned_scheduled_resource_shape() {
        for (shape, expected_parses, permit_present) in [
            (ScheduledResourceShape::Requested, 0, false),
            (ScheduledResourceShape::Granted, 0, true),
            (ScheduledResourceShape::Returned, 1, false),
            (ScheduledResourceShape::Reconciled, 1, false),
        ] {
            let (broker, domain, arena, member, container) = prepared_dependency_fixture();
            let sibling_reader = reader(1_717);
            let sibling = arena
                .resolve((1, 0), |permit| load(&sibling_reader, (1, 0), permit))
                .unwrap();
            let sibling_pointer = sibling.pointer();
            drop(sibling);
            let (entered, action, dropped, resource_hooks) =
                scheduled_resource_hook(&domain, shape);
            let request = arena
                .inner
                .request_representation(member, Representation::DeclaredObjStmMember)
                .unwrap();
            let cell = Arc::clone(&request.cell);
            let parses = Arc::new(AtomicU64::new(0));
            let parses_for_load = Arc::clone(&parses);
            let scalar_reader = reader(1_718);
            let join = thread::spawn(move || {
                request.resolve_scheduled_member_synthetic(
                    container,
                    |_| panic!("prepared child must hit"),
                    move |permit| {
                        parses_for_load.fetch_add(1, Ordering::Relaxed);
                        load(&scalar_reader, (1, 0), permit)
                    },
                )
            });
            let (entered_key, token, entered_shape) = entered.recv().unwrap();
            assert_eq!(entered_key, cell.key, "{shape:?}");
            assert_eq!(entered_shape, shape);
            {
                let state = lock(&cell.state);
                let CellPhase::Loading(loading) = &state.phase else {
                    panic!("resource-shape pause must retain Loading: {shape:?}")
                };
                assert_eq!(loading.generation, token.generation, "{shape:?}");
                assert_eq!(loading.leader_slot, token.leader_slot, "{shape:?}");
                assert!(loading.broker_cancellation.is_some(), "{shape:?}");
                assert_eq!(loading.permit.is_some(), permit_present, "{shape:?}");
                assert_eq!(loading.dependency.as_ref().unwrap().token, token);
            }
            assert_eq!(domain.snapshot().active_generations, 1, "{shape:?}");
            {
                let ledger = lock(&domain.inner.state);
                assert_eq!(ledger.active_generations.len(), 1, "{shape:?}");
                assert!(ledger.active_generations.contains(&token), "{shape:?}");
            }
            let (terminal_cleanup, terminal_action) =
                pausing_dependency_terminal_cleanup_hook(&domain);
            let teardown_inner = Arc::clone(&arena.inner);
            let teardown_cell = Arc::clone(&cell);
            let teardown = thread::spawn(move || {
                teardown_inner.close_exact_control(
                    &teardown_cell,
                    token.leader_slot,
                    token.generation,
                    CellControlFailure::new(
                        token.key.id,
                        AccessKind::Backend,
                        CellControlTag::DependencyStateInvariant,
                    ),
                    true,
                )
            });
            let (cleanup_key, cleanup_owner) = terminal_cleanup.recv().unwrap();
            assert_eq!(cleanup_key, cell.key, "{shape:?}");
            assert_eq!(
                cleanup_owner,
                DependencyTerminalCleanupOwner::ExactTeardown,
                "{shape:?}"
            );
            let cleanup_paused = domain.snapshot();
            assert_eq!(cleanup_paused.loading, 0, "{shape:?}");
            assert_eq!(cleanup_paused.active_generations, 1, "{shape:?}");
            {
                let ledger = lock(&domain.inner.state);
                assert_eq!(ledger.active_generations.len(), 1, "{shape:?}");
                assert!(ledger.active_generations.contains(&token), "{shape:?}");
            }
            terminal_action.send(()).unwrap();
            assert!(teardown.join().unwrap(), "{shape:?}");
            assert_eq!(domain.snapshot().active_generations, 0, "{shape:?}");
            action.send(()).unwrap();
            if matches!(
                shape,
                ScheduledResourceShape::Returned | ScheduledResourceShape::Reconciled
            ) {
                let (dropped_key, dropped_token, dropped_shape) = dropped.recv().unwrap();
                assert_eq!(dropped_key, cell.key, "{shape:?}");
                assert_eq!(dropped_token, token, "{shape:?}");
                assert_eq!(dropped_shape, shape, "{shape:?}");
                assert_eq!(
                    resource_hooks.drop_acknowledgements.load(Ordering::Relaxed),
                    1,
                    "{shape:?}"
                );
                let broker_at_drop = broker.snapshot();
                let operation_at_drop = &broker_at_drop.operations[&arena.epoch()];
                assert_eq!(operation_at_drop.queued, 0, "{shape:?}");
                assert_eq!(operation_at_drop.in_flight, 0, "{shape:?}");
                assert_eq!(broker_at_drop.live_request_records, 0, "{shape:?}");
                assert_eq!(broker_at_drop.reservation_metadata_bytes, 0, "{shape:?}");
                let _headroom = lock(&domain.inner.headroom);
                let _domain = lock(&domain.inner.state);
                assert_eq!(caller_thread_lock_depth(), 2, "{shape:?}");
                drop(_domain);
                drop(_headroom);
            }
            let error = join.join().unwrap().unwrap_err();
            if matches!(
                shape,
                ScheduledResourceShape::Returned | ScheduledResourceShape::Reconciled
            ) {
                assert_eq!(
                    resource_hooks.drop_acknowledgements.load(Ordering::Relaxed),
                    1,
                    "{shape:?}"
                );
                assert!(dropped.try_recv().is_err(), "{shape:?}");
            }
            assert_eq!(
                error.detail,
                CellControlTag::DependencyStateInvariant.detail(),
                "{shape:?}"
            );
            assert_eq!(parses.load(Ordering::Relaxed), expected_parses, "{shape:?}");
            let terminal = domain.snapshot();
            assert!(terminal.invariant_failed, "{shape:?}");
            assert_eq!(terminal.dependency_pins, 0, "{shape:?}");
            assert_eq!(terminal.dependency_edges, 0, "{shape:?}");
            assert_eq!(terminal.dependency_cleanup_acks, 1, "{shape:?}");
            let operation = broker.snapshot().operations[&arena.epoch()].clone();
            assert_eq!(operation.queued, 0, "{shape:?}");
            assert_eq!(operation.in_flight, 0, "{shape:?}");
            assert_eq!(operation.pin_bytes, 0, "{shape:?}");
            let sibling = arena
                .resolve((1, 0), |_| panic!("{shape:?}: sibling reloaded"))
                .unwrap();
            assert_eq!(sibling.pointer(), sibling_pointer, "{shape:?}");
            drop(sibling);
            assert!(arena.has_container_cell(container), "{shape:?}");
            drop(cell);
            arena.close();
            drop(arena);
            assert_eq!(broker.snapshot().aggregate_bytes, 0, "{shape:?}");
        }
    }
}
