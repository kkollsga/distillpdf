//! Broker-owned, per-document single-flight cells for raw normal PDF objects.
//!
//! This module is deliberately isolated from resolver routing.  It owns the
//! process-wide cell index, document epochs, bounded interests, publication,
//! cache ownership and close semantics; `access` supplies the actual bounded
//! object loader in the integration slice.

#![cfg_attr(not(test), allow(dead_code))]

use crate::access::{AccessError, AccessKind};
use crate::broker::{
    BrokerError, BrokerOperation, BudgetBroker, Lane, OwnershipClass, ReservationCancellation,
    RetainedCharge, SelfPinCharge,
};
use lopdf::{
    BoundedObject, BoundedObjectStream, Object, ObjectId, ScalarResolutionPermit,
    BOUNDED_OBJECT_STREAM_STRUCTURAL_ENVELOPE_BYTES,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};

const ARENA_METADATA_BYTES: u64 = 4 * 1024;
// The fixed 5 KiB precharge explicitly partitions an emergency flight-error
// owner. Broker admission/close failures cannot acquire another reservation,
// so their bounded owner and detail remain charged by this cell-local slice.
const CELL_BASE_METADATA_BYTES: u64 = 4_608;
const CELL_ERROR_ENVELOPE_BYTES: u64 = 512;
const CELL_METADATA_BYTES: u64 = CELL_BASE_METADATA_BYTES + CELL_ERROR_ENVELOPE_BYTES;
const ERROR_OWNER_BYTES: u64 = 256;
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

#[derive(Clone, Debug)]
pub(crate) struct CellLoadError {
    error: AccessError,
    disposition: NegativeDisposition,
}

impl CellLoadError {
    pub(crate) fn new(error: AccessError, disposition: NegativeDisposition) -> Self {
        Self { error, disposition }
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
    #[cfg(test)]
    close_hooks: Mutex<Option<Arc<dyn CloseEdgeHooks>>>,
    state: Mutex<DomainState>,
}

trait WaitEdgeHooks: Send + Sync {
    fn add(&self, epoch: u64, id: ObjectId, generation: u64, ordinal: u64);
    fn remove(&self, epoch: u64, id: ObjectId, generation: u64, ordinal: u64);
}

#[cfg(test)]
trait CloseEdgeHooks: Send + Sync {
    fn after_phase_replacement(&self);
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
    active: bool,
    ordinal: u64,
    generation: u64,
}

const EMPTY_INTEREST: InterestSlot = InterestSlot {
    active: false,
    ordinal: 0,
    generation: 0,
};

struct LoadingState {
    generation: u64,
    leader_slot: usize,
    leader_running: bool,
    cancellation: Arc<AtomicBool>,
    broker_cancellation: Option<ReservationCancellation>,
    permit: Option<ScalarResolutionPermit>,
}

enum CellPhase {
    Loading(LoadingState),
    Ready(Arc<ResolvedObjectOwner>),
    Negative(Arc<FailureOwner>),
    FlightError(Arc<FailureOwner>),
    Closed(Arc<AccessError>),
}

struct CellState {
    phase: CellPhase,
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

struct FailureOwner {
    error: AccessError,
    charge: Mutex<Option<RetainedCharge>>,
    _reservation: Mutex<Option<crate::broker::Reservation>>,
    cell_envelope: bool,
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

enum ResolveStep {
    Lead {
        generation: u64,
        cancellation: Arc<AtomicBool>,
    },
    Ready(Arc<ResolvedObjectOwner>),
    Error(Arc<FailureOwner>),
    Closed(Arc<AccessError>),
    Wait,
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
                #[cfg(test)]
                close_hooks: Mutex::new(None),
                state: Mutex::new(DomainState::default()),
            }),
        }
    }

    pub(crate) fn open_arena(&self) -> Result<ObjectCellArena, AccessError> {
        let _headroom = lock(&self.inner.headroom);
        let operation_metadata = self
            .inner
            .broker
            .normal_headroom()
            .operation_metadata_weight;
        self.inner.reclaim_loader_headroom(
            ARENA_METADATA_BYTES
                .checked_add(operation_metadata)
                .ok_or_else(|| {
                    cell_error(
                        (0, 0),
                        AccessKind::ResourceLimit,
                        "object cell arena headroom overflow",
                    )
                })?,
            LOADER_ESTIMATE_BYTES,
            (0, 0),
        )?;
        let operation = self
            .inner
            .broker
            .register_operation()
            .map_err(|error| broker_error((0, 0), error))?;
        let epoch = operation.id();
        let reservation = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                ARENA_METADATA_BYTES,
            )
            .map_err(|error| broker_error((0, 0), error))?;
        let mut metadata = reservation
            .reconcile(ARENA_METADATA_BYTES)
            .map_err(|error| broker_error((0, 0), error))?;
        metadata
            .transition(OwnershipClass::Cache)
            .map_err(|error| broker_error((0, 0), error))?;
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
    }

    pub(crate) fn snapshot(&self) -> ObjectCellSnapshot {
        self.inner.snapshot()
    }

    #[cfg(test)]
    fn set_wait_hooks(&self, hooks: Arc<dyn WaitEdgeHooks>) {
        *lock(&self.inner.wait_hooks) = Some(hooks);
    }

    #[cfg(test)]
    fn set_close_hooks(&self, hooks: Arc<dyn CloseEdgeHooks>) {
        *lock(&self.inner.close_hooks) = Some(hooks);
    }
}

impl ObjectCellArena {
    pub(crate) fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    pub(crate) fn request(&self, id: ObjectId) -> Result<ObjectCellRequest, AccessError> {
        self.inner.request(id)
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
    ) -> Result<ResolvedObjectStreamPin, AccessError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObjectStream, CellLoadError>,
    {
        self.inner
            .request_representation(id, Representation::DeclaredObjStmContainer)?
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
            .request_representation(id, Representation::DeclaredObjStmMember)?
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

impl ObjectCellRequest {
    pub(crate) fn cancellation_handle(&self) -> CellCancellation {
        CellCancellation {
            arena: Arc::downgrade(&self.arena),
            cell: Arc::downgrade(&self.cell),
            slot: self.slot,
            ordinal: self.ordinal,
        }
    }

    pub(crate) fn resolve<F>(self, loader: F) -> Result<ResolvedObjectPin, AccessError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObject, CellLoadError>,
    {
        let id = self.cell.key.id;
        let pin = self.resolve_payload(|permit| loader(permit).map(CellPayload::Object))?;
        if !matches!(&pin.owner.payload, CellPayload::Object(_)) {
            return Err(cell_error(
                id,
                AccessKind::Backend,
                "object cell payload representation mismatch",
            ));
        }
        Ok(ResolvedObjectPin { inner: pin })
    }

    fn resolve_object_stream<F>(self, loader: F) -> Result<ResolvedObjectStreamPin, AccessError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<BoundedObjectStream, CellLoadError>,
    {
        let pin = self.resolve_payload(|permit| loader(permit).map(CellPayload::ObjectStream))?;
        debug_assert!(matches!(&pin.owner.payload, CellPayload::ObjectStream(_)));
        Ok(ResolvedObjectStreamPin { inner: pin })
    }

    fn resolve_payload<F>(mut self, loader: F) -> Result<ResolvedCellPin, AccessError>
    where
        F: FnOnce(&ScalarResolutionPermit) -> Result<CellPayload, CellLoadError>,
    {
        let mut loader = Some(loader);
        loop {
            let step = {
                let mut state = lock(&self.cell.state);
                if state
                    .interests
                    .get(self.slot)
                    .is_none_or(|interest| !interest.active || interest.ordinal != self.ordinal)
                {
                    self.completed = true;
                    return Err(cell_error(
                        self.cell.key.id,
                        AccessKind::Backend,
                        "object cell interest was cancelled",
                    ));
                }
                match &mut state.phase {
                    CellPhase::Loading(loading)
                        if loading.leader_slot == self.slot && !loading.leader_running =>
                    {
                        loading.leader_running = true;
                        ResolveStep::Lead {
                            generation: loading.generation,
                            cancellation: Arc::clone(&loading.cancellation),
                        }
                    }
                    CellPhase::Loading(_) => ResolveStep::Wait,
                    CellPhase::Ready(owner) => ResolveStep::Ready(Arc::clone(owner)),
                    CellPhase::Negative(error) | CellPhase::FlightError(error) => {
                        ResolveStep::Error(Arc::clone(error))
                    }
                    CellPhase::Closed(error) => ResolveStep::Closed(Arc::clone(error)),
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
                        let failure = self.arena.invalidate_payload_mismatch(&self.cell);
                        self.finish_interest();
                        self.completed = true;
                        return Err(failure);
                    }
                    let pin = self.arena.pin(&self.cell, owner)?;
                    self.finish_interest();
                    self.completed = true;
                    return Ok(pin);
                }
                ResolveStep::Error(error) => {
                    let failure = error.error.clone();
                    self.finish_interest();
                    self.completed = true;
                    return Err(failure);
                }
                ResolveStep::Closed(error) => {
                    let failure = error.as_ref().clone();
                    self.finish_interest();
                    self.completed = true;
                    return Err(failure);
                }
                ResolveStep::Wait => {
                    let generation = {
                        let state = lock(&self.cell.state);
                        match &state.phase {
                            CellPhase::Loading(loading)
                                if !(loading.leader_slot == self.slot
                                    && !loading.leader_running)
                                    && state.interests.get(self.slot).is_some_and(|interest| {
                                        interest.active && interest.ordinal == self.ordinal
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
                            interest.active && interest.ordinal == self.ordinal
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
        let loader_estimate = self.cell.key.representation.loader_estimate();
        let pending = match {
            let _headroom = lock(&self.arena.domain.headroom);
            self.arena
                .domain
                .reclaim_loader_headroom(0, loader_estimate, self.cell.key.id)
                .and_then(|()| {
                    self.arena
                        .operation
                        .request(
                            Lane::Normal {
                                completion_reserve: 0,
                            },
                            loader_estimate,
                        )
                        .map_err(|error| broker_error(self.cell.key.id, error))
                })
        } {
            Ok(pending) => pending,
            Err(error) => {
                self.publish_envelope_error(error);
                return;
            }
        };
        let broker_cancellation = pending.cancellation_handle();
        let replaced_broker_cancellation = {
            let mut state = lock(&self.cell.state);
            if let CellPhase::Loading(loading) = &mut state.phase {
                if loading.generation == generation && loading.leader_slot == self.slot {
                    std::mem::replace(
                        &mut loading.broker_cancellation,
                        Some(broker_cancellation.clone()),
                    )
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(replaced_broker_cancellation);
        if load_cancel.load(Ordering::Acquire) {
            broker_cancellation.cancel();
        }
        let reservation = match pending.wait() {
            Ok(reservation) => reservation,
            Err(error) => {
                self.publish_broker_error(error);
                return;
            }
        };
        if load_cancel.load(Ordering::Acquire) {
            reservation.cancel();
        }
        let permit = ScalarResolutionPermit::new(loader_estimate);
        let replaced_permit = {
            let mut state = lock(&self.cell.state);
            if let CellPhase::Loading(loading) = &mut state.phase {
                if loading.generation == generation && loading.leader_slot == self.slot {
                    std::mem::replace(&mut loading.permit, Some(permit.clone()))
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
                domain.invariant_failed = true;
                drop(domain);
                drop(reservation);
                self.publish_broker_error(BrokerError::ArithmeticOverflow);
                return;
            }
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loader(&permit)));
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
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(payload) => {
                drop(reservation);
                self.arena.release_interest(
                    &self.cell,
                    self.slot,
                    self.ordinal,
                    InterestRelease::Abandon,
                );
                self.arena.leader_terminal(&self.cell, self.slot);
                std::panic::resume_unwind(payload);
            }
        };
        match outcome {
            Ok(payload) if payload.peak_bytes(&permit) <= loader_estimate => {
                let retained = payload.retained_bytes();
                match reservation.reconcile(retained) {
                    Ok(charge) => self.publish_value(payload, charge),
                    Err(error) => self.publish_broker_error(error),
                }
            }
            Ok(_) => {
                drop(reservation);
                self.publish_broker_error(BrokerError::ReconciliationLimit);
            }
            Err(error) => self.publish_load_error(reservation, error),
        }
    }

    fn publish_value(&self, payload: CellPayload, charge: RetainedCharge) {
        let owner = Arc::new(ResolvedObjectOwner {
            payload,
            transition_gate: Mutex::new(()),
            cache_backed: AtomicBool::new(false),
            charge: Mutex::new(Some(charge)),
            self_pin: Mutex::new(None),
        });
        self.arena.publish_ready(&self.cell, self.slot, owner);
    }

    fn publish_load_error(&self, reservation: crate::broker::Reservation, failure: CellLoadError) {
        let weight = ERROR_OWNER_BYTES.checked_add(failure.error.detail.capacity() as u64);
        let (charge, reservation) = match weight {
            Some(weight)
                if weight <= self.cell.key.representation.loader_estimate()
                    && !reservation.is_cancelled() =>
            {
                match reservation.reconcile(weight) {
                    Ok(charge) => (Some(charge), None),
                    Err(error) => {
                        self.publish_broker_error(error);
                        return;
                    }
                }
            }
            _ => (None, Some(reservation)),
        };
        let owner = Arc::new(FailureOwner {
            error: failure.error,
            charge: Mutex::new(charge),
            _reservation: Mutex::new(reservation),
            cell_envelope: false,
        });
        self.arena
            .publish_error(&self.cell, self.slot, owner, failure.disposition);
    }

    fn publish_broker_error(&self, error: BrokerError) {
        self.publish_envelope_error(broker_error(self.cell.key.id, error));
    }

    fn publish_envelope_error(&self, error: AccessError) {
        let weight = ERROR_OWNER_BYTES.checked_add(error.detail.capacity() as u64);
        let error = if weight.is_some_and(|bytes| bytes <= CELL_ERROR_ENVELOPE_BYTES) {
            error
        } else {
            cell_error(
                self.cell.key.id,
                AccessKind::ResourceLimit,
                "broker error exceeds the precharged cell envelope",
            )
        };
        let checked_weight = ERROR_OWNER_BYTES
            .checked_add(error.detail.capacity() as u64)
            .filter(|bytes| *bytes <= CELL_ERROR_ENVELOPE_BYTES);
        let error = if checked_weight.is_none() {
            let mut domain = lock(&self.arena.domain.state);
            domain.invariant_failed = true;
            drop(domain);
            cell_error(self.cell.key.id, AccessKind::ResourceLimit, "")
        } else {
            error
        };
        let owner = Arc::new(FailureOwner {
            error,
            charge: Mutex::new(None),
            _reservation: Mutex::new(None),
            cell_envelope: true,
        });
        self.arena.publish_error(
            &self.cell,
            self.slot,
            owner,
            NegativeDisposition::FlightOnly,
        );
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

    fn transition(&self, ownership: OwnershipClass) -> Result<(), AccessError> {
        let _transition = lock(&self.transition_gate);
        self.transition_charge(ownership)
    }

    fn transition_charge(&self, ownership: OwnershipClass) -> Result<(), AccessError> {
        let mut charge = lock(&self.charge);
        if let Some(charge) = charge.as_mut() {
            charge
                .transition(ownership)
                .map_err(|error| broker_error((0, 0), error))?;
            match ownership {
                OwnershipClass::Cache => self.cache_backed.store(true, Ordering::Release),
                OwnershipClass::Bypass => self.cache_backed.store(false, Ordering::Release),
                OwnershipClass::Pin => {}
            }
        }
        Ok(())
    }

    fn acquire_self_pin(&self, operation: &BrokerOperation) -> Result<(), AccessError> {
        let charge = lock(&self.charge);
        let Some(charge) = charge.as_ref() else {
            return Ok(());
        };
        let pin = charge
            .pin(operation, charge.bytes())
            .map_err(|error| broker_error((0, 0), error))?;
        *lock(&self.self_pin) = Some(pin);
        Ok(())
    }

    fn release_self_pin(&self) {
        drop(lock(&self.self_pin).take());
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
    fn transition(&self, ownership: OwnershipClass) -> Result<(), AccessError> {
        let mut charge = lock(&self.charge);
        if let Some(charge) = charge.as_mut() {
            charge
                .transition(ownership)
                .map_err(|error| broker_error((0, 0), error))?;
        }
        Ok(())
    }
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

impl Drop for ExternalPin {
    fn drop(&mut self) {
        let (Some(arena), Some(cell)) = (self.arena.upgrade(), self.cell.upgrade()) else {
            return;
        };
        arena.unpin(&cell, &self.owner, self.cached);
    }
}

impl ArenaInner {
    fn leader_terminal(&self, cell: &Arc<Cell>, leader_slot: usize) {
        let mut remove = false;
        let mut old_phase = None;
        let mut old_cancellation = None;
        let mut old_broker_cancellation = None;
        let mut old_permit = None;
        let mut removed_cell = None;
        {
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            let CellPhase::Loading(loading) = &state.phase else {
                return;
            };
            if loading.leader_slot != leader_slot {
                return;
            }
            let next = state
                .interests
                .iter()
                .enumerate()
                .filter(|(_, interest)| interest.active)
                .min_by_key(|(_, interest)| interest.ordinal)
                .map(|(slot, _)| slot);
            if let Some(slot) = next {
                let Some(generation) = loading.generation.checked_add(1) else {
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
                        CellPhase::Closed(Arc::new(cell_error(
                            cell.key.id,
                            AccessKind::ResourceLimit,
                            "object cell load generation overflow",
                        ))),
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
                for interest in state
                    .interests
                    .iter_mut()
                    .filter(|interest| interest.active)
                {
                    interest.generation = generation;
                }
                if let CellPhase::Loading(loading) = &mut state.phase {
                    loading.generation = generation;
                    loading.leader_slot = slot;
                    loading.leader_running = false;
                    old_cancellation = Some(std::mem::replace(
                        &mut loading.cancellation,
                        Arc::new(AtomicBool::new(false)),
                    ));
                    old_broker_cancellation = loading.broker_cancellation.take();
                    old_permit = loading.permit.take();
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
                    CellPhase::Closed(Arc::new(cell_error(
                        cell.key.id,
                        AccessKind::Backend,
                        "object cell flight was cancelled",
                    ))),
                ));
                remove = true;
            }
        }
        drop(old_phase);
        drop(old_cancellation);
        drop(old_broker_cancellation);
        drop(old_permit);
        drop(removed_cell);
        if remove {
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
        }
        cell.ready.notify_all();
    }

    fn request(self: &Arc<Self>, id: ObjectId) -> Result<ObjectCellRequest, AccessError> {
        self.request_representation(id, Representation::RawNormalObject)
    }

    fn request_representation(
        self: &Arc<Self>,
        id: ObjectId,
        representation: Representation,
    ) -> Result<ObjectCellRequest, AccessError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(cell_error(
                id,
                AccessKind::Backend,
                "object cell arena is closed",
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
            .map_err(|error| broker_error(id, error))?;
        let mut metadata = reservation
            .reconcile(CELL_METADATA_BYTES)
            .map_err(|error| broker_error(id, error))?;
        metadata
            .transition(OwnershipClass::Cache)
            .map_err(|error| broker_error(id, error))?;
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
                }),
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
                return Err(cell_error(
                    id,
                    AccessKind::Backend,
                    "object cell arena is closed",
                ));
            }
            if domain.cells.len() >= self.domain.config.max_cells {
                return Err(cell_error(
                    id,
                    AccessKind::CellFull,
                    "object cell domain is full",
                ));
            }
            if domain
                .loading_metadata_bytes
                .checked_add(CELL_METADATA_BYTES)
                .is_none_or(|bytes| bytes > MAX_LOADING_METADATA_BYTES)
            {
                return Err(cell_error(
                    id,
                    AccessKind::ResourceLimit,
                    "object cell loading metadata limit reached",
                ));
            }
            let cache_bytes = domain
                .cache_bytes
                .checked_add(CELL_METADATA_BYTES)
                .ok_or_else(|| {
                    cell_error(
                        id,
                        AccessKind::ResourceLimit,
                        "object cell metadata accounting overflow",
                    )
                })?;
            let loading_metadata_bytes = domain
                .loading_metadata_bytes
                .checked_add(CELL_METADATA_BYTES)
                .ok_or_else(|| {
                    cell_error(
                        id,
                        AccessKind::ResourceLimit,
                        "object cell loading metadata accounting overflow",
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

    fn join_key(self: &Arc<Self>, key: CellKey) -> Result<Option<ObjectCellRequest>, AccessError> {
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

    fn publish_ready(&self, cell: &Arc<Cell>, leader_slot: usize, owner: Arc<ResolvedObjectOwner>) {
        let incoming = owner.retained_bytes();
        let (cache, victims) = self.domain.prepare_publication(cell.key, incoming);
        drop(victims);
        if cache && owner.transition(OwnershipClass::Cache).is_err() {
            self.domain.release_publication(incoming);
            self.publish_error(
                cell,
                leader_slot,
                Arc::new(FailureOwner {
                    error: cell_error(
                        cell.key.id,
                        AccessKind::ResourceLimit,
                        "object cell ownership transition failed",
                    ),
                    charge: Mutex::new(None),
                    _reservation: Mutex::new(None),
                    cell_envelope: true,
                }),
                NegativeDisposition::FlightOnly,
            );
            return;
        }
        let mut domain = lock(&self.domain.state);
        let mut state = lock(&cell.state);
        if !self.publication_wins(&state, leader_slot) {
            drop(state);
            drop(domain);
            if cache {
                self.domain.release_publication(incoming);
                let _ = owner.transition(OwnershipClass::Bypass);
            }
            self.leader_terminal(cell, leader_slot);
            return;
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
            let old_phase = std::mem::replace(
                &mut state.phase,
                CellPhase::Closed(Arc::new(cell_error(
                    cell.key.id,
                    AccessKind::ResourceLimit,
                    "object cell touch sequence overflow",
                ))),
            );
            drop(state);
            drop(domain);
            drop(old_phase);
            drop(removed_cell);
            if cache {
                let _ = owner.transition(OwnershipClass::Bypass);
            }
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
            cell.ready.notify_all();
            return;
        };
        let removed_cell = if cache {
            let Some(weight) = state.completed_weight.checked_add(incoming) else {
                state.transitioning = false;
                domain.invariant_failed = true;
                drop(state);
                drop(domain);
                self.domain.release_publication(incoming);
                let _ = owner.transition(OwnershipClass::Bypass);
                self.leader_terminal(cell, leader_slot);
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
        let old_phase = std::mem::replace(&mut state.phase, CellPhase::Ready(owner));
        state.transitioning = false;
        domain.touch = next_touch;
        state.touch = next_touch;
        drop(state);
        drop(domain);
        drop(old_phase);
        drop(removed_cell);
        if !cache {
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
        }
        cell.ready.notify_all();
    }

    fn publish_error(
        &self,
        cell: &Arc<Cell>,
        leader_slot: usize,
        error: Arc<FailureOwner>,
        disposition: NegativeDisposition,
    ) {
        let incoming = ERROR_OWNER_BYTES.checked_add(error.error.detail.capacity() as u64);
        if error.cell_envelope && incoming.is_none_or(|bytes| bytes > CELL_ERROR_ENVELOPE_BYTES) {
            let mut domain = lock(&self.domain.state);
            domain.invariant_failed = true;
            drop(domain);
            let safe = Arc::new(FailureOwner {
                error: cell_error(
                    cell.key.id,
                    AccessKind::ResourceLimit,
                    "cell error accounting overflow",
                ),
                charge: Mutex::new(None),
                _reservation: Mutex::new(None),
                cell_envelope: true,
            });
            return self.publish_error(cell, leader_slot, safe, NegativeDisposition::FlightOnly);
        }
        let persistent = disposition == NegativeDisposition::Persistent
            && !error.cell_envelope
            && lock(&error.charge).is_some()
            && incoming.is_some();
        let incoming = incoming.unwrap_or(0);
        let (cache, victims) = if persistent {
            self.domain.prepare_publication(cell.key, incoming)
        } else {
            (false, Vec::new())
        };
        drop(victims);
        if cache {
            let transition_failed = {
                let mut charge = lock(&error.charge);
                charge
                    .as_mut()
                    .is_some_and(|charge| charge.transition(OwnershipClass::Cache).is_err())
            };
            if transition_failed {
                self.domain.release_publication(incoming);
                return self.publish_error(
                    cell,
                    leader_slot,
                    error,
                    NegativeDisposition::FlightOnly,
                );
            }
        }
        let mut domain = lock(&self.domain.state);
        let mut state = lock(&cell.state);
        if !self.publication_wins(&state, leader_slot) {
            drop(state);
            drop(domain);
            if cache {
                self.domain.release_publication(incoming);
            }
            self.leader_terminal(cell, leader_slot);
            return;
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
            let old_phase = std::mem::replace(
                &mut state.phase,
                CellPhase::Closed(Arc::new(cell_error(
                    cell.key.id,
                    AccessKind::ResourceLimit,
                    "object cell touch sequence overflow",
                ))),
            );
            drop(state);
            drop(domain);
            drop(old_phase);
            drop(removed_cell);
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
            cell.ready.notify_all();
            return;
        };
        if cache {
            let Some(weight) = state.completed_weight.checked_add(incoming) else {
                domain.invariant_failed = true;
                drop(state);
                drop(domain);
                self.domain.release_publication(incoming);
                return self.publish_error(
                    cell,
                    leader_slot,
                    error,
                    NegativeDisposition::FlightOnly,
                );
            };
            state.completed_weight = weight;
            if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                domain.invariant_failed = true;
            }
            let old_phase = std::mem::replace(&mut state.phase, CellPhase::Negative(error));
            state.transitioning = false;
            domain.touch = next_touch;
            state.touch = next_touch;
            drop(state);
            drop(domain);
            drop(old_phase);
            cell.ready.notify_all();
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
            let old_phase = std::mem::replace(&mut state.phase, CellPhase::FlightError(error));
            state.transitioning = false;
            domain.touch = next_touch;
            state.touch = next_touch;
            drop(state);
            drop(domain);
            drop(old_phase);
            drop(removed_cell);
            if let Some(metadata) = lock(&cell.metadata).as_mut() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
            cell.ready.notify_all();
            return;
        }
    }

    fn publication_wins(&self, state: &CellState, leader_slot: usize) -> bool {
        !self.closed.load(Ordering::Acquire)
            && state
                .interests
                .get(leader_slot)
                .is_some_and(|slot| slot.active)
            && matches!(&state.phase, CellPhase::Loading(loading) if loading.leader_slot == leader_slot && !loading.cancellation.load(Ordering::Acquire))
    }

    fn invalidate_payload_mismatch(&self, cell: &Arc<Cell>) -> AccessError {
        let failure = cell_error(
            cell.key.id,
            AccessKind::Backend,
            "object cell payload representation mismatch",
        );
        let closed = Arc::new(failure.clone());
        let (old_phase, removed_cell) = {
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            domain.invariant_failed = true;
            let removed_cell = domain
                .cells
                .get(&cell.key)
                .filter(|current| Arc::ptr_eq(current, cell))
                .is_some()
                .then(|| domain.cells.remove(&cell.key))
                .flatten();
            if state.cached && !checked_sub(&mut domain.cache_bytes, state.completed_weight) {
                domain.invariant_failed = true;
            }
            state.cached = false;
            let old_phase = std::mem::replace(&mut state.phase, CellPhase::Closed(closed));
            (old_phase, removed_cell)
        };
        if let CellPhase::Ready(owner) = &old_phase {
            let _ = owner.transition(OwnershipClass::Bypass);
        }
        drop(old_phase);
        drop(removed_cell);
        if let Some(metadata) = lock(&cell.metadata).as_mut() {
            let _ = metadata.transition(OwnershipClass::Bypass);
        }
        cell.ready.notify_all();
        failure
    }

    fn pin(
        self: &Arc<Self>,
        cell: &Arc<Cell>,
        owner: Arc<ResolvedObjectOwner>,
    ) -> Result<ResolvedCellPin, AccessError> {
        let _transition = lock(&owner.transition_gate);
        let cache_backed = owner.cache_backed.load(Ordering::Acquire);
        let admission = {
            let _domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            if self.closed.load(Ordering::Acquire)
                || !matches!(&state.phase, CellPhase::Ready(current) if Arc::ptr_eq(current, &owner))
            {
                Err(cell_error(
                    cell.key.id,
                    AccessKind::Backend,
                    "object cell arena closed before external pin admission",
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
                Err(cell_error(
                    cell.key.id,
                    AccessKind::ResourceLimit,
                    "object cell external pin overflow",
                ))
            }
        };
        let (cached, first) = admission?;
        if first {
            let transition = (if cache_backed {
                owner.transition_charge(OwnershipClass::Pin)
            } else {
                Ok(())
            })
            .and_then(|()| owner.acquire_self_pin(&self.operation));
            if let Err(error) = transition {
                owner.release_self_pin();
                if cache_backed {
                    let _ = owner.transition_charge(OwnershipClass::Cache);
                }
                let _domain = lock(&self.domain.state);
                let mut state = lock(&cell.state);
                state.external_pins -= 1;
                state.transitioning = false;
                return Err(error);
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
        let (last, transition_to_cache) = {
            let _domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            if state.external_pins == 0 {
                return;
            }
            state.external_pins -= 1;
            let last = state.external_pins == 0;
            let transition = last && was_cached && state.cached;
            if last {
                state.transitioning = true;
            }
            (last, transition)
        };
        if last {
            owner.release_self_pin();
            if transition_to_cache {
                let _ = owner.transition_charge(OwnershipClass::Cache);
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
        let (broker_cancel, permit, acknowledge_now) = {
            let mut domain = lock(&self.domain.state);
            let mut state = lock(&cell.state);
            if release == InterestRelease::Cancel && !matches!(state.phase, CellPhase::Loading(_)) {
                return;
            }
            let Some(interest) = state.interests.get_mut(slot) else {
                return;
            };
            if !interest.active || interest.ordinal != ordinal {
                return;
            }
            interest.active = false;
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
                        !loading.leader_running,
                    )
                }
                _ => (None, None, false),
            }
        };
        if let Some(cancel) = broker_cancel {
            cancel.cancel();
        }
        if let Some(permit) = permit {
            permit.cancel();
        }
        if acknowledge_now {
            self.leader_terminal(cell, slot);
        }
        cell.ready.notify_all();
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let (cells, removed_arena) = {
            let mut domain = lock(&self.domain.state);
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
                    let (broker_cancel, permit) = if let CellPhase::Loading(loading) =
                        &mut state.phase
                    {
                        if !checked_sub(&mut domain.loading_metadata_bytes, CELL_METADATA_BYTES) {
                            domain.invariant_failed = true;
                        }
                        loading.cancellation.store(true, Ordering::Release);
                        (loading.broker_cancellation.clone(), loading.permit.clone())
                    } else {
                        (None, None)
                    };
                    let external_pins = state.external_pins;
                    let old_phase = std::mem::replace(
                        &mut state.phase,
                        CellPhase::Closed(Arc::new(cell_error(
                            key.id,
                            AccessKind::Backend,
                            "object cell arena is closed",
                        ))),
                    );
                    drop(state);
                    cells.push((cell, broker_cancel, permit, old_phase, external_pins));
                }
            }
            (cells, removed_arena)
        };
        drop(removed_arena);
        #[cfg(test)]
        {
            let hooks = lock(&self.domain.close_hooks).clone();
            if let Some(hooks) = hooks {
                hooks.after_phase_replacement();
            }
        }
        for (cell, broker_cancel, permit, old_phase, external_pins) in cells {
            if let Some(cancel) = broker_cancel {
                cancel.cancel();
            }
            if let Some(permit) = permit {
                permit.cancel();
            }
            if let Some(mut metadata) = lock(&cell.metadata).take() {
                let _ = metadata.transition(OwnershipClass::Bypass);
            }
            match &old_phase {
                CellPhase::Ready(owner) if external_pins == 0 => {
                    let _ = owner.transition(OwnershipClass::Bypass);
                }
                CellPhase::Negative(error) => {
                    let _ = error.transition(OwnershipClass::Bypass);
                }
                _ => {}
            }
            drop(old_phase);
            cell.ready.notify_all();
        }
        if let Some(mut metadata) = lock(&self._metadata).take() {
            let _ = metadata.transition(OwnershipClass::Bypass);
        }
        self.operation.close();
    }
}

impl Drop for ArenaInner {
    fn drop(&mut self) {
        self.close();
    }
}

impl DomainInner {
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
                CellPhase::Loading(_) => snapshot.loading += 1,
                CellPhase::Ready(_) => snapshot.ready += 1,
                CellPhase::Negative(_) => snapshot.negative += 1,
                CellPhase::FlightError(_) | CellPhase::Closed(_) => {}
            }
        }
        snapshot
    }

    fn make_cell_room(&self, id: ObjectId, incoming: u64) -> Result<Vec<Arc<Cell>>, AccessError> {
        let mut domain = lock(&self.state);
        let mut victims = Vec::new();
        while domain.cells.len() >= self.config.max_cells {
            let Some(key) = oldest_evictable(&domain, None) else {
                return Err(cell_error(
                    id,
                    AccessKind::CellFull,
                    "object cell domain is full",
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
            return Err(cell_error(
                id,
                AccessKind::ResourceLimit,
                "object cell loading metadata limit reached",
            ));
        }
        Ok(victims)
    }

    fn reclaim_loader_headroom(
        &self,
        additional: u64,
        loader_estimate: u64,
        id: ObjectId,
    ) -> Result<(), AccessError> {
        loop {
            let broker = self.broker.normal_headroom();
            let drained_payload = broker
                .normal_payload_bytes
                .checked_sub(broker.normal_in_flight_estimate_bytes)
                .ok_or_else(|| {
                    cell_error(
                        id,
                        AccessKind::ResourceLimit,
                        "broker in-flight estimate accounting underflow",
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
                    return Err(cell_error(
                        id,
                        AccessKind::ResourceLimit,
                        "broker-global loader headroom is unavailable",
                    ));
                };
                remove_victim(&mut domain, key)
            };
            drop(victim);
        }
    }

    fn prepare_publication(&self, exclude: CellKey, incoming: u64) -> (bool, Vec<Arc<Cell>>) {
        let mut domain = lock(&self.state);
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
            let Some(key) = oldest_evictable(&domain, Some(exclude)) else {
                return (false, victims);
            };
            if let Some(cell) = remove_victim(&mut domain, key) {
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
) -> Result<(usize, u64), AccessError> {
    if domain.live_interests >= max_global_interests {
        return Err(cell_error(
            id,
            AccessKind::ResourceLimit,
            "object cell interest limit reached",
        ));
    }
    let slot = state
        .interests
        .iter()
        .position(|slot| !slot.active)
        .ok_or_else(|| {
            cell_error(
                id,
                AccessKind::ResourceLimit,
                "object cell has 64 live interests",
            )
        })?;
    let ordinal = state.next_interest_ordinal.checked_add(1).ok_or_else(|| {
        cell_error(
            id,
            AccessKind::ResourceLimit,
            "object cell interest ordinal overflow",
        )
    })?;
    let cell_interests = state
        .live_interests
        .checked_add(1)
        .ok_or_else(|| cell_error(id, AccessKind::ResourceLimit, "cell interest overflow"))?;
    let global_interests = domain
        .live_interests
        .checked_add(1)
        .ok_or_else(|| cell_error(id, AccessKind::ResourceLimit, "global interest overflow"))?;
    let calls = domain
        .calls
        .checked_add(1)
        .ok_or_else(|| cell_error(id, AccessKind::ResourceLimit, "cell call counter overflow"))?;
    let mut kind = domain.representations[representation.index()];
    kind.calls = kind
        .calls
        .checked_add(1)
        .ok_or_else(|| cell_error(id, AccessKind::ResourceLimit, "kind call counter overflow"))?;
    let mut waits = domain.waits;
    let mut hits = domain.hits;
    let mut negative_hits = domain.negative_hits;
    let mut transient_shares = domain.transient_shares;
    let touch = match &state.phase {
        CellPhase::Loading(_) if cell_interests > 1 => {
            waits = waits.checked_add(1).ok_or_else(|| {
                cell_error(id, AccessKind::ResourceLimit, "cell wait counter overflow")
            })?;
            kind.waits = kind.waits.checked_add(1).ok_or_else(|| {
                cell_error(id, AccessKind::ResourceLimit, "kind wait counter overflow")
            })?;
            None
        }
        CellPhase::Loading(_) => None,
        CellPhase::Ready(_) => {
            hits = hits.checked_add(1).ok_or_else(|| {
                cell_error(id, AccessKind::ResourceLimit, "cell hit counter overflow")
            })?;
            kind.hits = kind.hits.checked_add(1).ok_or_else(|| {
                cell_error(id, AccessKind::ResourceLimit, "kind hit counter overflow")
            })?;
            Some(domain.touch.checked_add(1).ok_or_else(|| {
                cell_error(
                    id,
                    AccessKind::ResourceLimit,
                    "object cell touch sequence overflow",
                )
            })?)
        }
        CellPhase::Negative(_) => {
            negative_hits = negative_hits.checked_add(1).ok_or_else(|| {
                cell_error(
                    id,
                    AccessKind::ResourceLimit,
                    "negative hit counter overflow",
                )
            })?;
            kind.negative_hits = kind.negative_hits.checked_add(1).ok_or_else(|| {
                cell_error(
                    id,
                    AccessKind::ResourceLimit,
                    "kind negative hit counter overflow",
                )
            })?;
            Some(domain.touch.checked_add(1).ok_or_else(|| {
                cell_error(
                    id,
                    AccessKind::ResourceLimit,
                    "object cell touch sequence overflow",
                )
            })?)
        }
        CellPhase::FlightError(_) => {
            transient_shares = transient_shares.checked_add(1).ok_or_else(|| {
                cell_error(
                    id,
                    AccessKind::ResourceLimit,
                    "transient share counter overflow",
                )
            })?;
            kind.transient_shares = kind.transient_shares.checked_add(1).ok_or_else(|| {
                cell_error(
                    id,
                    AccessKind::ResourceLimit,
                    "kind transient share counter overflow",
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
        active: true,
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{
        dictionary, BytesSource, Document, IndexedObjectLocation, IndexedReader,
        IndexedReaderOptions, Object, SaveOptions, ScalarResolutionPermit,
    };
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::{mpsc, Barrier};
    use std::thread;

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

    #[test]
    fn production_cell_precharges_cover_typed_owner_and_error_structures() {
        assert!(CELL_FIXED_STRUCTURAL_BYTES <= CELL_BASE_METADATA_BYTES as usize);
        assert!(
            CELL_FIXED_STRUCTURAL_BYTES
                > CELL_BASE_METADATA_BYTES as usize - CELL_ERROR_ENVELOPE_BYTES as usize
        );
        assert!(std::mem::size_of::<FailureOwner>() <= ERROR_OWNER_BYTES as usize);
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
                        .resolve_object_stream(id, |_| Err(failure()))
                        .unwrap_err();
                    arena
                        .resolve_object_stream(id, |_| Err(failure()))
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
                        .resolve_object_stream(|_| Err(transient(id, "mixed transient")))
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
                Representation::DeclaredObjStmContainer => cancelled
                    .resolve_object_stream(|_| panic!("cancelled loader must not run"))
                    .unwrap_err(),
                Representation::RawNormalObject | Representation::DeclaredObjStmMember => cancelled
                    .resolve(|_| panic!("cancelled loader must not run"))
                    .unwrap_err(),
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

    fn object_stream_reader() -> (Arc<IndexedReader>, ObjectId, ObjectId, u32) {
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
        let reader = Arc::new(IndexedReader::open(BytesSource::from(raw)).unwrap());
        let IndexedObjectLocation::Compressed { container, index } =
            reader.object_location(member).unwrap()
        else {
            panic!("fixture member must be declared compressed")
        };
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
            request.publish_broker_error(error);
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

    struct PausingCloseHooks {
        entered: Barrier,
        release: Barrier,
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
    fn close_cancels_loading_and_queued_flights_and_drains_external_owners() {
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
        let second_join = thread::spawn(move || {
            second.resolve(|_| panic!("queued loader must be cancelled before invocation"))
        });
        let mut observed_queue = false;
        for _ in 0..100_000 {
            if broker.snapshot().queued == 1 {
                observed_queue = true;
                break;
            }
            thread::yield_now();
        }
        assert!(observed_queue, "second flight never entered broker queue");
        arena.close();
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
            .expect_err("phase replacement must reject a previously observed ready owner");
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
}
