//! Process-wide weighted admission ledger for bounded resolver work.
//!
//! This module deliberately has no PDF dependency. Resolver cells consume it in
//! later slices; keeping the scheduler injectable makes its memory and fairness
//! invariants testable without allocating the production limits.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};

pub(crate) const NORMAL_LIMIT: u64 = 134_217_728;
pub(crate) const OVERSIZE_LIMIT: u64 = 67_108_864;
pub(crate) const COMPLETION_RESERVE_LIMIT: u64 = 33_554_432;
pub(crate) const QUEUE_METADATA_WEIGHT: u64 = 256;
pub(crate) const OPERATION_METADATA_WEIGHT: u64 = 2_048;
pub(crate) const MAX_ACTIVE_OPERATIONS: usize = 65_536;
pub(crate) const MAX_QUEUED_REQUESTS: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrokerConfig {
    pub(crate) normal_limit: u64,
    pub(crate) oversize_limit: u64,
    pub(crate) completion_reserve_limit: u64,
    pub(crate) queue_metadata_weight: u64,
    pub(crate) operation_metadata_weight: u64,
    pub(crate) max_active_operations: usize,
    pub(crate) max_queued_requests: usize,
}

impl BrokerConfig {
    pub(crate) const fn production() -> Self {
        Self {
            normal_limit: NORMAL_LIMIT,
            oversize_limit: OVERSIZE_LIMIT,
            completion_reserve_limit: COMPLETION_RESERVE_LIMIT,
            queue_metadata_weight: QUEUE_METADATA_WEIGHT,
            operation_metadata_weight: OPERATION_METADATA_WEIGHT,
            max_active_operations: MAX_ACTIVE_OPERATIONS,
            max_queued_requests: MAX_QUEUED_REQUESTS,
        }
    }

    fn validate(self) -> Result<Self, BrokerError> {
        if self.normal_limit == 0
            || self.oversize_limit == 0
            || self.completion_reserve_limit > self.normal_limit
            || self.queue_metadata_weight == 0
            || self.operation_metadata_weight == 0
            || self.max_active_operations == 0
            || self.max_queued_requests == 0
            || self.normal_limit.checked_add(self.oversize_limit).is_none()
        {
            return Err(BrokerError::InvalidConfig);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Lane {
    Normal { completion_reserve: u64 },
    Oversize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationClass {
    Normal,
    OversizeEligible { normal_retained_cap: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BrokerError {
    InvalidConfig,
    Closed,
    OperationClosed,
    Cancelled,
    ArithmeticOverflow,
    ResourceLimit,
    QueueFull,
    OperationFull,
    SelfPinned,
    ReconciliationLimit,
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConfig => "invalid broker configuration",
            Self::Closed => "budget broker is closed",
            Self::OperationClosed => "budget broker operation is closed",
            Self::Cancelled => "budget broker request was cancelled",
            Self::ArithmeticOverflow => "budget broker arithmetic overflow",
            Self::ResourceLimit => "budget broker resource limit exceeded",
            Self::QueueFull => "budget broker queue is full",
            Self::OperationFull => "budget broker operation table is full",
            Self::SelfPinned => "operation pins the capacity required by its request",
            Self::ReconciliationLimit => "actual retained weight exceeds the reservation",
        })
    }
}

impl std::error::Error for BrokerError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BrokerSnapshot {
    pub(crate) normal_limit_bytes: u64,
    pub(crate) normal_payload_bytes: u64,
    pub(crate) normal_in_flight_estimate_bytes: u64,
    pub(crate) metadata_bytes: u64,
    pub(crate) completion_reserve_bytes: u64,
    pub(crate) oversize_bytes: u64,
    pub(crate) aggregate_bytes: u64,
    pub(crate) peak_normal_bytes: u64,
    pub(crate) peak_completion_reserve_bytes: u64,
    pub(crate) peak_oversize_bytes: u64,
    pub(crate) peak_aggregate_bytes: u64,
    pub(crate) queued: usize,
    pub(crate) in_flight: usize,
    pub(crate) live_request_records: usize,
    pub(crate) error_metadata_bytes: u64,
    pub(crate) reservation_metadata_bytes: u64,
    pub(crate) active_operations: usize,
    pub(crate) grants: u64,
    pub(crate) denials: u64,
    pub(crate) cancellations: u64,
    pub(crate) reconciliations: u64,
    pub(crate) maximum_admissible_lag: u64,
    pub(crate) oversize_owners: u8,
    pub(crate) cache_bytes: u64,
    pub(crate) pin_bytes: u64,
    pub(crate) bypass_bytes: u64,
    pub(crate) peak_cache_bytes: u64,
    pub(crate) peak_pin_bytes: u64,
    pub(crate) peak_bypass_bytes: u64,
    pub(crate) operations: BTreeMap<u64, OperationSnapshot>,
    pub(crate) invariant_failed: bool,
    pub(crate) closed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NormalHeadroomSnapshot {
    pub(crate) normal_limit_bytes: u64,
    pub(crate) normal_payload_bytes: u64,
    pub(crate) normal_in_flight_estimate_bytes: u64,
    pub(crate) metadata_bytes: u64,
    pub(crate) completion_reserve_bytes: u64,
    pub(crate) queue_metadata_weight: u64,
    pub(crate) operation_metadata_weight: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OperationSnapshot {
    pub(crate) normal_retained_cap: u64,
    pub(crate) queued: usize,
    pub(crate) in_flight: usize,
    pub(crate) error_owners: usize,
    pub(crate) grants: u64,
    pub(crate) granted_bytes: u64,
    pub(crate) cache_bytes: u64,
    pub(crate) pin_bytes: u64,
    pub(crate) bypass_bytes: u64,
    pub(crate) self_pinned_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OwnershipClass {
    Cache,
    Pin,
    Bypass,
}

#[derive(Clone)]
pub(crate) struct BudgetBroker {
    inner: Arc<BrokerInner>,
}

struct BrokerInner {
    config: BrokerConfig,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    closed: bool,
    invariant_failed: bool,
    next_operation_id: u64,
    next_request_id: u64,
    next_grant_ordinal: u64,
    normal_payload: u64,
    normal_in_flight_estimates: u64,
    metadata: u64,
    completion_reserve: u64,
    oversize: u64,
    oversize_queue_metadata: u64,
    oversize_in_flight_metadata: u64,
    peak_normal: u64,
    peak_reserve: u64,
    peak_oversize: u64,
    peak_aggregate: u64,
    queued: usize,
    in_flight: usize,
    live_request_records: usize,
    error_metadata_count: usize,
    active_normal_loaders: usize,
    normal_in_flight_count: usize,
    grants: u64,
    denials: u64,
    cancellations: u64,
    reconciliations: u64,
    maximum_admissible_lag: u64,
    cache_bytes: u64,
    pin_bytes: u64,
    bypass_bytes: u64,
    peak_cache_bytes: u64,
    peak_pin_bytes: u64,
    peak_bypass_bytes: u64,
    operations: BTreeMap<u64, OperationRecord>,
    normal_round: Vec<u64>,
    normal_cursor: usize,
    normal_successor: Option<u64>,
    cohort_baseline: BTreeMap<u64, u64>,
    oversize_queue: VecDeque<QueuedRequest>,
    oversize_draining: bool,
    oversize_active: bool,
}

struct OperationRecord {
    normal_retained_cap: u64,
    normal: VecDeque<QueuedRequest>,
    in_flight: usize,
    error_owners: usize,
    closed: bool,
    grants: u64,
    granted_bytes: u64,
    cache_bytes: u64,
    pin_bytes: u64,
    bypass_bytes: u64,
    self_pinned_bytes: u64,
}

struct QueuedRequest {
    id: u64,
    operation_id: u64,
    estimate: u64,
    completion_reserve: u64,
    lane: Lane,
    cancellation: Arc<AtomicBool>,
    operation_closed: Weak<AtomicBool>,
    self_pinned: Arc<AtomicU64>,
    operation: Weak<OperationInner>,
    wait: Arc<WaitCell>,
}

struct WaitCell {
    outcome: Mutex<WaitOutcome>,
    ready: Condvar,
}

enum WaitOutcome {
    Pending,
    Granted(GrantLease),
    Error(RequestError),
}

struct RequestError {
    error: BrokerError,
    _metadata: RequestMetadataLease,
}

struct RequestMetadataLease {
    broker: Arc<BrokerInner>,
    operation_id: u64,
    lane: Lane,
    active: bool,
}

struct Notification {
    wait: Arc<WaitCell>,
    outcome: WaitOutcome,
}

struct OperationInner {
    id: u64,
    broker: Weak<BrokerInner>,
    closed: Arc<AtomicBool>,
    self_pinned: Arc<AtomicU64>,
    grants: AtomicU64,
    class: OperationClass,
}

#[derive(Clone)]
pub(crate) struct BrokerOperation(Arc<OperationInner>);

pub(crate) struct PendingReservation {
    broker: Arc<BrokerInner>,
    operation_id: u64,
    request_id: u64,
    cancellation: Arc<AtomicBool>,
    wait: Arc<WaitCell>,
    completed: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingReservationDiagnostic {
    Queued,
    Granted,
    Error,
}

#[derive(Clone)]
pub(crate) struct ReservationCancellation {
    broker: Weak<BrokerInner>,
    operation_id: u64,
    request_id: u64,
    cancellation: Weak<AtomicBool>,
}

pub(crate) struct SelfPinCharge {
    operation: Arc<OperationInner>,
    owner: Arc<RetainedInner>,
    bytes: u64,
    active: bool,
}

pub(crate) struct Reservation {
    lease: Option<GrantLease>,
}

struct GrantLease {
    broker: Arc<BrokerInner>,
    operation_id: u64,
    estimate: u64,
    completion_reserve: u64,
    lane: Lane,
    cancellation: Arc<AtomicBool>,
    operation_closed: Weak<AtomicBool>,
    grant_ordinal: u64,
    request_metadata_bytes: u64,
    active: bool,
}

pub(crate) struct RetainedCharge {
    inner: Arc<RetainedInner>,
}

struct RetainedInner {
    broker: Arc<BrokerInner>,
    operation_id: u64,
    lane: Lane,
    bytes: u64,
    ownership: Mutex<OwnershipClass>,
    self_pinned_bytes: AtomicU64,
}

impl BudgetBroker {
    pub(crate) fn new(config: BrokerConfig) -> Result<Self, BrokerError> {
        Ok(Self {
            inner: Arc::new(BrokerInner {
                config: config.validate()?,
                state: Mutex::new(State::default()),
            }),
        })
    }

    pub(crate) fn production() -> &'static Self {
        static BROKER: OnceLock<BudgetBroker> = OnceLock::new();
        BROKER.get_or_init(|| {
            Self::new(BrokerConfig::production()).expect("production broker config is valid")
        })
    }

    pub(crate) fn register_operation(&self) -> Result<BrokerOperation, BrokerError> {
        self.register_operation_with_class(OperationClass::Normal)
    }

    pub(crate) fn register_oversize_operation(
        &self,
        normal_retained_cap: u64,
    ) -> Result<BrokerOperation, BrokerError> {
        if normal_retained_cap >= self.inner.config.oversize_limit {
            return Err(BrokerError::ResourceLimit);
        }
        self.register_operation_with_class(OperationClass::OversizeEligible {
            normal_retained_cap,
        })
    }

    fn register_operation_with_class(
        &self,
        class: OperationClass,
    ) -> Result<BrokerOperation, BrokerError> {
        let mut state = self.inner.lock();
        if state.closed {
            return Err(BrokerError::Closed);
        }
        if state.operations.len() >= self.inner.config.max_active_operations {
            return Err(BrokerError::OperationFull);
        }
        let metadata = state
            .metadata
            .checked_add(self.inner.config.operation_metadata_weight)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        if state
            .normal_payload
            .checked_add(state.completion_reserve)
            .and_then(|bytes| bytes.checked_add(metadata))
            .is_none_or(|bytes| bytes > self.inner.config.normal_limit)
        {
            return Err(BrokerError::ResourceLimit);
        }
        let id = state
            .next_operation_id
            .checked_add(1)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        state.next_operation_id = id;
        state.metadata = metadata;
        state.operations.insert(
            id,
            OperationRecord {
                normal_retained_cap: match class {
                    OperationClass::Normal => self.inner.config.normal_limit,
                    OperationClass::OversizeEligible {
                        normal_retained_cap,
                    } => normal_retained_cap,
                },
                normal: VecDeque::new(),
                in_flight: 0,
                error_owners: 0,
                closed: false,
                grants: 0,
                granted_bytes: 0,
                cache_bytes: 0,
                pin_bytes: 0,
                bypass_bytes: 0,
                self_pinned_bytes: 0,
            },
        );
        state.update_peaks(&self.inner.config);
        let operation = BrokerOperation(Arc::new(OperationInner {
            id,
            broker: Arc::downgrade(&self.inner),
            closed: Arc::new(AtomicBool::new(false)),
            self_pinned: Arc::new(AtomicU64::new(0)),
            grants: AtomicU64::new(0),
            class,
        }));
        Ok(operation)
    }

    pub(crate) fn snapshot(&self) -> BrokerSnapshot {
        self.inner.snapshot()
    }

    pub(crate) fn normal_headroom(&self) -> NormalHeadroomSnapshot {
        let state = self.inner.lock();
        NormalHeadroomSnapshot {
            normal_limit_bytes: self.inner.config.normal_limit,
            normal_payload_bytes: state.normal_payload,
            normal_in_flight_estimate_bytes: state.normal_in_flight_estimates,
            metadata_bytes: state.metadata,
            completion_reserve_bytes: state.completion_reserve,
            queue_metadata_weight: self.inner.config.queue_metadata_weight,
            operation_metadata_weight: self.inner.config.operation_metadata_weight,
        }
    }

    pub(crate) fn close(&self) {
        self.inner.close_all();
    }
}

impl BrokerOperation {
    pub(crate) fn id(&self) -> u64 {
        self.0.id
    }

    pub(crate) fn grant_count(&self) -> u64 {
        self.0.grants.load(Ordering::Acquire)
    }

    pub(crate) fn request(
        &self,
        lane: Lane,
        estimate: u64,
    ) -> Result<PendingReservation, BrokerError> {
        let broker = self.0.broker.upgrade().ok_or(BrokerError::Closed)?;
        broker.enqueue(self, lane, estimate)
    }

    pub(crate) fn reserve(&self, lane: Lane, estimate: u64) -> Result<Reservation, BrokerError> {
        self.request(lane, estimate)?.wait()
    }

    pub(crate) fn try_reserve(
        &self,
        lane: Lane,
        estimate: u64,
    ) -> Result<Reservation, BrokerError> {
        self.request(lane, estimate)?
            .try_wait()?
            .ok_or(BrokerError::ResourceLimit)
    }

    pub(crate) fn close(&self) {
        if !self.0.closed.swap(true, Ordering::AcqRel) {
            if let Some(broker) = self.0.broker.upgrade() {
                broker.close_operation(self.0.id);
            }
        }
    }
}

impl Drop for SelfPinCharge {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(broker) = self.operation.broker.upgrade() {
            let notifications = {
                let mut state = broker.lock();
                let state_valid =
                    state
                        .operations
                        .get_mut(&self.operation.id)
                        .is_some_and(|record| {
                            State::decrease_u64(&mut record.self_pinned_bytes, self.bytes)
                        });
                let owner_valid = self
                    .owner
                    .self_pinned_bytes
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        current.checked_sub(self.bytes)
                    })
                    .is_ok();
                let atomic_valid = self
                    .operation
                    .self_pinned
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        current.checked_sub(self.bytes)
                    })
                    .is_ok();
                if !state_valid || !owner_valid || !atomic_valid {
                    self.operation.closed.store(true, Ordering::Release);
                    state.mark_invariant_failure();
                }
                state.schedule(&broker)
            };
            notify_all(notifications);
        } else {
            let owner_valid = self
                .owner
                .self_pinned_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_sub(self.bytes)
                })
                .is_ok();
            let atomic_valid = self
                .operation
                .self_pinned
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_sub(self.bytes)
                })
                .is_ok();
            if !owner_valid || !atomic_valid {
                self.operation.closed.store(true, Ordering::Release);
            }
        }
        self.active = false;
    }
}

impl Drop for OperationInner {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            if let Some(broker) = self.broker.upgrade() {
                broker.close_operation(self.id);
            }
        }
    }
}

impl PendingReservation {
    #[cfg(test)]
    pub(crate) fn diagnostic_state(&self) -> PendingReservationDiagnostic {
        match &*lock_recover(&self.wait.outcome) {
            WaitOutcome::Pending => PendingReservationDiagnostic::Queued,
            WaitOutcome::Granted(_) => PendingReservationDiagnostic::Granted,
            WaitOutcome::Error(_) => PendingReservationDiagnostic::Error,
        }
    }

    pub(crate) fn cancellation_handle(&self) -> ReservationCancellation {
        ReservationCancellation {
            broker: Arc::downgrade(&self.broker),
            operation_id: self.operation_id,
            request_id: self.request_id,
            cancellation: Arc::downgrade(&self.cancellation),
        }
    }

    pub(crate) fn cancel(&self) {
        self.broker
            .cancel_request(self.operation_id, self.request_id, &self.cancellation);
    }

    pub(crate) fn wait(mut self) -> Result<Reservation, BrokerError> {
        let mut outcome = lock_recover(&self.wait.outcome);
        loop {
            match std::mem::replace(&mut *outcome, WaitOutcome::Pending) {
                WaitOutcome::Pending => {
                    outcome = wait_recover(&self.wait.ready, outcome);
                }
                WaitOutcome::Granted(lease) => {
                    self.completed = true;
                    return Ok(Reservation { lease: Some(lease) });
                }
                WaitOutcome::Error(request_error) => {
                    self.completed = true;
                    let error = request_error.error.clone();
                    drop(request_error);
                    return Err(error);
                }
            }
        }
    }

    fn try_wait(mut self) -> Result<Option<Reservation>, BrokerError> {
        let current = {
            let mut outcome = lock_recover(&self.wait.outcome);
            std::mem::replace(&mut *outcome, WaitOutcome::Pending)
        };
        match current {
            WaitOutcome::Pending => {
                self.cancel();
                let cancelled = {
                    let mut outcome = lock_recover(&self.wait.outcome);
                    std::mem::replace(&mut *outcome, WaitOutcome::Pending)
                };
                self.completed = true;
                match cancelled {
                    WaitOutcome::Pending => Ok(None),
                    WaitOutcome::Granted(lease) => Ok(Some(Reservation { lease: Some(lease) })),
                    WaitOutcome::Error(request_error)
                        if request_error.error == BrokerError::Cancelled =>
                    {
                        Ok(None)
                    }
                    WaitOutcome::Error(request_error) => Err(request_error.error.clone()),
                }
            }
            WaitOutcome::Granted(lease) => {
                self.completed = true;
                Ok(Some(Reservation { lease: Some(lease) }))
            }
            WaitOutcome::Error(request_error) => {
                self.completed = true;
                Err(request_error.error.clone())
            }
        }
    }
}

impl ReservationCancellation {
    pub(crate) fn cancel(&self) {
        let Some(cancellation) = self.cancellation.upgrade() else {
            return;
        };
        if let Some(broker) = self.broker.upgrade() {
            broker.cancel_request(self.operation_id, self.request_id, &cancellation);
        } else {
            cancellation.store(true, Ordering::Release);
        }
    }
}

impl Drop for PendingReservation {
    fn drop(&mut self) {
        if !self.completed {
            self.cancel();
        }
    }
}

impl Reservation {
    pub(crate) fn estimate(&self) -> u64 {
        self.lease.as_ref().map_or(0, |lease| lease.estimate)
    }

    pub(crate) fn grant_ordinal(&self) -> u64 {
        self.lease.as_ref().map_or(0, |lease| lease.grant_ordinal)
    }

    pub(crate) fn request_metadata_bytes(&self) -> u64 {
        self.lease
            .as_ref()
            .map_or(0, |lease| lease.request_metadata_bytes)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.lease
            .as_ref()
            .is_none_or(|lease| lease.cancellation.load(Ordering::Acquire))
    }

    pub(crate) fn cancel(&self) {
        if let Some(lease) = &self.lease {
            lease.cancellation.store(true, Ordering::Release);
        }
    }

    pub(crate) fn reconcile(mut self, actual: u64) -> Result<RetainedCharge, BrokerError> {
        let mut lease = self.lease.take().expect("reservation reconciles once");
        let result = lease.reconcile(actual);
        lease.active = false;
        result
    }
}

impl GrantLease {
    fn reconcile(&mut self, actual: u64) -> Result<RetainedCharge, BrokerError> {
        self.broker.reconcile(self, actual)
    }
}

impl Drop for GrantLease {
    fn drop(&mut self) {
        if self.active {
            self.broker.release_reservation(self);
            self.active = false;
        }
    }
}

impl Drop for RequestMetadataLease {
    fn drop(&mut self) {
        if self.active {
            self.broker
                .release_error_metadata(self.operation_id, self.lane);
            self.active = false;
        }
    }
}

impl RetainedCharge {
    pub(crate) fn bytes(&self) -> u64 {
        self.inner.bytes
    }

    pub(crate) fn transition(&mut self, ownership: OwnershipClass) -> Result<(), BrokerError> {
        let mut current = lock_recover(&self.inner.ownership);
        if *current == ownership {
            return Ok(());
        }
        self.inner.broker.transition_ownership(
            self.inner.operation_id,
            *current,
            ownership,
            self.inner.bytes,
        )?;
        *current = ownership;
        Ok(())
    }

    pub(crate) fn pin(
        &self,
        operation: &BrokerOperation,
        bytes: u64,
    ) -> Result<SelfPinCharge, BrokerError> {
        if operation.id() != self.inner.operation_id || bytes > self.inner.bytes {
            return Err(BrokerError::ResourceLimit);
        }
        let broker = operation.0.broker.upgrade().ok_or(BrokerError::Closed)?;
        let mut state = broker.lock();
        let record = state
            .operations
            .get_mut(&operation.0.id)
            .filter(|record| !record.closed)
            .ok_or(BrokerError::OperationClosed)?;
        let owner_pinned = self
            .inner
            .self_pinned_bytes
            .load(Ordering::Acquire)
            .checked_add(bytes)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        if owner_pinned > self.inner.bytes {
            return Err(BrokerError::ResourceLimit);
        }
        let pinned = record
            .self_pinned_bytes
            .checked_add(bytes)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        let owned = record
            .cache_bytes
            .checked_add(record.pin_bytes)
            .and_then(|value| value.checked_add(record.bypass_bytes))
            .ok_or(BrokerError::ArithmeticOverflow)?;
        if pinned > owned {
            return Err(BrokerError::ResourceLimit);
        }
        operation
            .0
            .self_pinned
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(bytes)
            })
            .map_err(|_| BrokerError::ArithmeticOverflow)?;
        self.inner
            .self_pinned_bytes
            .store(owner_pinned, Ordering::Release);
        record.self_pinned_bytes = pinned;
        Ok(SelfPinCharge {
            operation: Arc::clone(&operation.0),
            owner: Arc::clone(&self.inner),
            bytes,
            active: true,
        })
    }
}

impl Drop for RetainedInner {
    fn drop(&mut self) {
        let ownership = *lock_recover(&self.ownership);
        self.broker
            .release_retained(self.operation_id, self.lane, self.bytes, ownership);
    }
}

impl BrokerInner {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock_recover(&self.state)
    }

    fn enqueue(
        self: &Arc<Self>,
        operation: &BrokerOperation,
        lane: Lane,
        estimate: u64,
    ) -> Result<PendingReservation, BrokerError> {
        let reserve = match lane {
            Lane::Normal { completion_reserve } => completion_reserve,
            Lane::Oversize => 0,
        };
        estimate
            .checked_add(reserve)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        match lane {
            Lane::Normal { .. }
                if estimate > self.config.normal_limit - self.config.completion_reserve_limit =>
            {
                return Err(BrokerError::ResourceLimit);
            }
            Lane::Normal { .. } if reserve > self.config.completion_reserve_limit => {
                return Err(BrokerError::ResourceLimit);
            }
            Lane::Oversize if estimate == 0 || estimate > self.config.oversize_limit => {
                return Err(BrokerError::ResourceLimit);
            }
            Lane::Oversize => match operation.0.class {
                OperationClass::OversizeEligible {
                    normal_retained_cap,
                } if estimate > normal_retained_cap => {}
                _ => return Err(BrokerError::ResourceLimit),
            },
            _ => {}
        }
        if operation.0.closed.load(Ordering::Acquire) {
            return Err(BrokerError::OperationClosed);
        }

        let mut state = self.lock();
        if state.closed {
            return Err(BrokerError::Closed);
        }
        if operation.0.closed.load(Ordering::Acquire)
            || state
                .operations
                .get(&operation.0.id)
                .is_none_or(|record| record.closed)
        {
            return Err(BrokerError::OperationClosed);
        }
        if matches!(lane, Lane::Normal { .. })
            && state.operations[&operation.0.id].normal_retained_cap < estimate
        {
            return self.deny_enqueue(state, BrokerError::ResourceLimit);
        }
        if state.live_request_records >= self.config.max_queued_requests {
            return self.deny_enqueue(state, BrokerError::QueueFull);
        }
        let metadata = match lane {
            Lane::Normal { .. } => {
                let metadata = state
                    .metadata
                    .checked_add(self.config.queue_metadata_weight)
                    .ok_or(BrokerError::ArithmeticOverflow)?;
                if state
                    .normal_payload
                    .checked_add(state.completion_reserve)
                    .and_then(|bytes| bytes.checked_add(metadata))
                    .is_none_or(|bytes| bytes > self.config.normal_limit)
                {
                    return self.deny_enqueue(state, BrokerError::ResourceLimit);
                }
                Some(metadata)
            }
            Lane::Oversize => {
                let queued = state
                    .oversize_queue_metadata
                    .checked_add(self.config.queue_metadata_weight)
                    .ok_or(BrokerError::ArithmeticOverflow)?;
                if state
                    .oversize
                    .checked_add(queued)
                    .is_none_or(|bytes| bytes > self.config.oversize_limit)
                {
                    return self.deny_enqueue(state, BrokerError::ResourceLimit);
                }
                let head_estimate = state
                    .oversize_queue
                    .front()
                    .map_or(estimate, |request| request.estimate);
                let metadata_after_head_grant = queued
                    .checked_sub(self.config.queue_metadata_weight)
                    .ok_or(BrokerError::ArithmeticOverflow)?;
                if head_estimate
                    .checked_add(metadata_after_head_grant)
                    .is_none_or(|bytes| bytes > self.config.oversize_limit)
                {
                    return self.deny_enqueue(state, BrokerError::ResourceLimit);
                }
                state.oversize_queue_metadata = queued;
                None
            }
        };
        let request_id = state
            .next_request_id
            .checked_add(1)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        state.next_request_id = request_id;
        // Charge the lane-local queued-request metadata before its backing
        // queue node, cancellation flag, or waiter is allocated.
        if let Some(metadata) = metadata {
            state.metadata = metadata;
        }
        state.queued = state
            .queued
            .checked_add(1)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        state.live_request_records = state
            .live_request_records
            .checked_add(1)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        let cancellation = Arc::new(AtomicBool::new(false));
        let wait = Arc::new(WaitCell {
            outcome: Mutex::new(WaitOutcome::Pending),
            ready: Condvar::new(),
        });
        let queued = QueuedRequest {
            id: request_id,
            operation_id: operation.0.id,
            estimate,
            completion_reserve: reserve,
            lane,
            cancellation: Arc::clone(&cancellation),
            operation_closed: Arc::downgrade(&operation.0.closed),
            self_pinned: Arc::clone(&operation.0.self_pinned),
            operation: Arc::downgrade(&operation.0),
            wait: Arc::clone(&wait),
        };
        match lane {
            Lane::Normal { .. } => state
                .operations
                .get_mut(&operation.0.id)
                .expect("inserted operation")
                .normal
                .push_back(queued),
            Lane::Oversize => {
                state.oversize_queue.push_back(queued);
                state.oversize_draining = true;
            }
        }
        state.update_peaks(&self.config);
        let notifications = state.schedule(self);
        drop(state);
        notify_all(notifications);
        Ok(PendingReservation {
            broker: Arc::clone(self),
            operation_id: operation.0.id,
            request_id,
            cancellation,
            wait,
            completed: false,
        })
    }

    fn deny_enqueue(
        self: &Arc<Self>,
        mut state: MutexGuard<'_, State>,
        error: BrokerError,
    ) -> Result<PendingReservation, BrokerError> {
        if let Some(denials) = state.denials.checked_add(1) {
            state.denials = denials;
            return Err(error);
        }
        state.mark_invariant_failure();
        let notifications = state.drain_queued_with_error(self, BrokerError::ArithmeticOverflow);
        drop(state);
        notify_all(notifications);
        Err(BrokerError::ArithmeticOverflow)
    }

    fn cancel_request(
        self: &Arc<Self>,
        operation_id: u64,
        request_id: u64,
        cancellation: &Arc<AtomicBool>,
    ) {
        let notifications = {
            let mut state = self.lock();
            let exists =
                state.operations.get(&operation_id).is_some_and(|record| {
                    record.normal.iter().any(|request| request.id == request_id)
                }) || state.oversize_queue.iter().any(|request| {
                    request.operation_id == operation_id && request.id == request_id
                });
            if exists && state.cancellations == u64::MAX {
                state.mark_invariant_failure();
                state.drain_queued_with_error(self, BrokerError::ArithmeticOverflow)
            } else if let Some(request) = {
                cancellation.store(true, Ordering::Release);
                state.remove_queued(operation_id, request_id, &self.config)
            } {
                state.cancellations += 1;
                let mut notifications =
                    vec![state.request_error(request, BrokerError::Cancelled, self)];
                notifications.extend(state.schedule(self));
                notifications
            } else {
                cancellation.store(true, Ordering::Release);
                Vec::new()
            }
        };
        notify_all(notifications);
    }

    fn close_operation(self: &Arc<Self>, operation_id: u64) {
        let notifications = {
            let mut state = self.lock();
            let mut notifications = Vec::new();
            if let Some(record) = state.operations.get_mut(&operation_id) {
                record.closed = true;
            }
            let normal_requests = state
                .operations
                .get_mut(&operation_id)
                .map(|record| {
                    let requests = record.normal.drain(..).collect::<Vec<_>>();
                    record.normal = VecDeque::new();
                    requests
                })
                .unwrap_or_default();
            for request in normal_requests {
                notifications.push(state.request_error(
                    request,
                    BrokerError::OperationClosed,
                    self,
                ));
            }
            let mut index = 0;
            while index < state.oversize_queue.len() {
                if state.oversize_queue[index].operation_id == operation_id {
                    let request = state.oversize_queue.remove(index).expect("indexed request");
                    notifications.push(state.request_error(
                        request,
                        BrokerError::OperationClosed,
                        self,
                    ));
                } else {
                    index += 1;
                }
            }
            state.prune_operation(operation_id, &self.config);
            state.compact_empty_queues();
            state.refresh_admissible_cohort(&self.config);
            if state.oversize_queue.is_empty() && !state.oversize_active {
                state.oversize_draining = false;
            }
            notifications.extend(state.schedule(self));
            notifications
        };
        notify_all(notifications);
    }

    fn close_all(self: &Arc<Self>) {
        let notifications = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.drain_queued_with_error(self, BrokerError::Closed)
        };
        notify_all(notifications);
    }

    fn reconcile(
        self: &Arc<Self>,
        lease: &GrantLease,
        actual: u64,
    ) -> Result<RetainedCharge, BrokerError> {
        let mut state = self.lock();
        let operation_closed = lease
            .operation_closed
            .upgrade()
            .is_none_or(|closed| closed.load(Ordering::Acquire))
            || state
                .operations
                .get(&lease.operation_id)
                .is_none_or(|record| record.closed);
        let rejected =
            state.closed || operation_closed || lease.cancellation.load(Ordering::Acquire);
        let rejection_error = if state.closed {
            BrokerError::Closed
        } else if operation_closed {
            BrokerError::OperationClosed
        } else {
            BrokerError::Cancelled
        };
        let result = match lease.lane {
            Lane::Normal { .. } => {
                let maximum = lease
                    .estimate
                    .checked_add(lease.completion_reserve)
                    .ok_or(BrokerError::ArithmeticOverflow)?;
                let retained_cap = state
                    .operations
                    .get(&lease.operation_id)
                    .map_or(0, |record| record.normal_retained_cap);
                if rejected || actual > maximum || actual > retained_cap {
                    Err(if rejected {
                        rejection_error.clone()
                    } else {
                        BrokerError::ReconciliationLimit
                    })
                } else {
                    let accounting = state
                        .reconciliations
                        .checked_add(1)
                        .zip(state.bypass_bytes.checked_add(actual))
                        .zip(
                            state
                                .normal_payload
                                .checked_sub(lease.estimate)
                                .and_then(|bytes| bytes.checked_add(actual)),
                        )
                        .zip(
                            state
                                .operations
                                .get(&lease.operation_id)
                                .and_then(|record| record.bypass_bytes.checked_add(actual)),
                        );
                    if let Some((((reconciliations, bypass), retained), operation_bypass)) =
                        accounting.filter(|_| state.can_release_normal_flight(lease, &self.config))
                    {
                        state.normal_payload = retained;
                        state.normal_in_flight_estimates -= lease.estimate;
                        state.metadata -= lease.request_metadata_bytes;
                        state.normal_in_flight_count -= 1;
                        state.completion_reserve -= lease.completion_reserve;
                        state.active_normal_loaders -= 1;
                        state.in_flight -= 1;
                        state.live_request_records -= 1;
                        state
                            .operations
                            .get_mut(&lease.operation_id)
                            .expect("active operation")
                            .bypass_bytes = operation_bypass;
                        state.finish_operation_flight(lease.operation_id, &self.config);
                        state.reconciliations = reconciliations;
                        state.bypass_bytes = bypass;
                        state.update_peaks(&self.config);
                        Ok(RetainedCharge {
                            inner: Arc::new(RetainedInner {
                                broker: Arc::clone(self),
                                operation_id: lease.operation_id,
                                lane: lease.lane,
                                bytes: actual,
                                ownership: Mutex::new(OwnershipClass::Bypass),
                                self_pinned_bytes: AtomicU64::new(0),
                            }),
                        })
                    } else {
                        state.mark_invariant_failure();
                        Err(BrokerError::ArithmeticOverflow)
                    }
                }
            }
            Lane::Oversize => {
                if rejected || actual > lease.estimate {
                    Err(if rejected {
                        rejection_error
                    } else {
                        BrokerError::ReconciliationLimit
                    })
                } else {
                    let accounting = state
                        .reconciliations
                        .checked_add(1)
                        .zip(state.bypass_bytes.checked_add(actual))
                        .zip(
                            state
                                .operations
                                .get(&lease.operation_id)
                                .and_then(|record| record.bypass_bytes.checked_add(actual)),
                        );
                    if let Some(((reconciliations, bypass), operation_bypass)) =
                        accounting.filter(|_| {
                            state.oversize >= lease.estimate
                                && state.in_flight > 0
                                && state.live_request_records > 0
                                && state.oversize_in_flight_metadata == lease.request_metadata_bytes
                        })
                    {
                        state.oversize = actual;
                        state.oversize_in_flight_metadata = 0;
                        state.in_flight -= 1;
                        state.live_request_records -= 1;
                        state
                            .operations
                            .get_mut(&lease.operation_id)
                            .expect("active operation")
                            .bypass_bytes = operation_bypass;
                        state.finish_operation_flight(lease.operation_id, &self.config);
                        state.reconciliations = reconciliations;
                        state.bypass_bytes = bypass;
                        state.update_peaks(&self.config);
                        Ok(RetainedCharge {
                            inner: Arc::new(RetainedInner {
                                broker: Arc::clone(self),
                                operation_id: lease.operation_id,
                                lane: lease.lane,
                                bytes: actual,
                                ownership: Mutex::new(OwnershipClass::Bypass),
                                self_pinned_bytes: AtomicU64::new(0),
                            }),
                        })
                    } else {
                        state.mark_invariant_failure();
                        Err(BrokerError::ArithmeticOverflow)
                    }
                }
            }
        };
        if result.is_err() {
            state.release_grant(lease, &self.config);
        }
        let notifications = if state.invariant_failed {
            state.drain_queued_with_error(self, BrokerError::ArithmeticOverflow)
        } else {
            state.schedule(self)
        };
        drop(state);
        notify_all(notifications);
        result
    }

    fn release_reservation(self: &Arc<Self>, lease: &GrantLease) {
        let notifications = {
            let mut state = self.lock();
            state.release_grant(lease, &self.config);
            state.schedule(self)
        };
        notify_all(notifications);
    }

    fn release_error_metadata(self: &Arc<Self>, operation_id: u64, lane: Lane) {
        let notifications = {
            let mut state = self.lock();
            let mut valid = state.release_request_metadata(lane, &self.config);
            valid &= State::decrease_usize(&mut state.live_request_records, 1);
            valid &= State::decrease_usize(&mut state.error_metadata_count, 1);
            if let Some(record) = state.operations.get_mut(&operation_id) {
                valid &= State::decrease_usize(&mut record.error_owners, 1);
            } else {
                valid = false;
            }
            if !valid {
                state.mark_invariant_failure();
            }
            state.prune_operation(operation_id, &self.config);
            state.schedule(self)
        };
        notify_all(notifications);
    }

    fn transition_ownership(
        self: &Arc<Self>,
        operation_id: u64,
        from: OwnershipClass,
        to: OwnershipClass,
        bytes: u64,
    ) -> Result<(), BrokerError> {
        let mut state = self.lock();
        state.move_ownership(operation_id, from, to, bytes)?;
        state.update_peaks(&self.config);
        Ok(())
    }

    fn release_retained(
        self: &Arc<Self>,
        operation_id: u64,
        lane: Lane,
        bytes: u64,
        ownership: OwnershipClass,
    ) {
        let notifications = {
            let mut state = self.lock();
            state.release_ownership(operation_id, ownership, bytes);
            match lane {
                Lane::Normal { .. } => {
                    if !State::decrease_u64(&mut state.normal_payload, bytes) {
                        state.mark_invariant_failure();
                    }
                }
                Lane::Oversize => {
                    if !State::decrease_u64(&mut state.oversize, bytes) {
                        state.mark_invariant_failure();
                    }
                    state.oversize_active = false;
                    if state.oversize_queue.is_empty() {
                        state.oversize_draining = false;
                    }
                }
            }
            state.prune_operation(operation_id, &self.config);
            state.schedule(self)
        };
        notify_all(notifications);
    }

    fn snapshot(&self) -> BrokerSnapshot {
        let state = self.lock();
        state.snapshot(&self.config)
    }
}

impl State {
    fn request_error(
        &mut self,
        request: QueuedRequest,
        error: BrokerError,
        broker: &Arc<BrokerInner>,
    ) -> Notification {
        if !Self::decrease_usize(&mut self.queued, 1) {
            self.mark_invariant_failure();
        }
        self.error_metadata_count += 1;
        if let Some(record) = self.operations.get_mut(&request.operation_id) {
            record.error_owners += 1;
        } else {
            self.mark_invariant_failure();
        }
        Notification {
            wait: request.wait,
            outcome: WaitOutcome::Error(RequestError {
                error,
                _metadata: RequestMetadataLease {
                    broker: Arc::clone(broker),
                    operation_id: request.operation_id,
                    lane: request.lane,
                    active: true,
                },
            }),
        }
    }

    fn mark_invariant_failure(&mut self) {
        self.invariant_failed = true;
        self.closed = true;
    }

    fn decrease_u64(value: &mut u64, amount: u64) -> bool {
        if let Some(remaining) = value.checked_sub(amount) {
            *value = remaining;
            true
        } else {
            false
        }
    }

    fn decrease_usize(value: &mut usize, amount: usize) -> bool {
        if let Some(remaining) = value.checked_sub(amount) {
            *value = remaining;
            true
        } else {
            false
        }
    }

    fn can_release_normal_flight(&self, lease: &GrantLease, config: &BrokerConfig) -> bool {
        self.normal_in_flight_estimates >= lease.estimate
            && lease.request_metadata_bytes == config.queue_metadata_weight
            && self.metadata >= lease.request_metadata_bytes
            && self.normal_in_flight_count > 0
            && self.completion_reserve >= lease.completion_reserve
            && self.active_normal_loaders > 0
            && self.in_flight > 0
            && self.live_request_records > 0
            && self
                .operations
                .get(&lease.operation_id)
                .is_some_and(|record| record.in_flight > 0)
    }

    fn ownership_mut(&mut self, ownership: OwnershipClass) -> &mut u64 {
        match ownership {
            OwnershipClass::Cache => &mut self.cache_bytes,
            OwnershipClass::Pin => &mut self.pin_bytes,
            OwnershipClass::Bypass => &mut self.bypass_bytes,
        }
    }

    fn operation_ownership_mut(
        &mut self,
        operation_id: u64,
        ownership: OwnershipClass,
    ) -> Option<&mut u64> {
        let record = self.operations.get_mut(&operation_id)?;
        Some(match ownership {
            OwnershipClass::Cache => &mut record.cache_bytes,
            OwnershipClass::Pin => &mut record.pin_bytes,
            OwnershipClass::Bypass => &mut record.bypass_bytes,
        })
    }

    fn move_ownership(
        &mut self,
        operation_id: u64,
        from: OwnershipClass,
        to: OwnershipClass,
        bytes: u64,
    ) -> Result<(), BrokerError> {
        let source = *self.ownership_mut(from);
        let destination = *self.ownership_mut(to);
        let operation_source = *self
            .operation_ownership_mut(operation_id, from)
            .ok_or(BrokerError::OperationClosed)?;
        let operation_destination = *self
            .operation_ownership_mut(operation_id, to)
            .ok_or(BrokerError::OperationClosed)?;
        let source = source
            .checked_sub(bytes)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        let destination = destination
            .checked_add(bytes)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        let operation_source = operation_source
            .checked_sub(bytes)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        let operation_destination = operation_destination
            .checked_add(bytes)
            .ok_or(BrokerError::ArithmeticOverflow)?;
        *self.ownership_mut(from) = source;
        *self.ownership_mut(to) = destination;
        *self
            .operation_ownership_mut(operation_id, from)
            .expect("validated operation") = operation_source;
        *self
            .operation_ownership_mut(operation_id, to)
            .expect("validated operation") = operation_destination;
        Ok(())
    }

    fn release_ownership(&mut self, operation_id: u64, ownership: OwnershipClass, bytes: u64) {
        let global = *self.ownership_mut(ownership);
        let operation = self
            .operation_ownership_mut(operation_id, ownership)
            .map(|value| *value);
        let remaining = global
            .checked_sub(bytes)
            .zip(operation.and_then(|operation| operation.checked_sub(bytes)));
        if let Some((global, operation)) = remaining {
            *self.ownership_mut(ownership) = global;
            *self
                .operation_ownership_mut(operation_id, ownership)
                .expect("validated operation") = operation;
        } else {
            self.mark_invariant_failure();
        }
    }

    fn schedule(&mut self, broker: &Arc<BrokerInner>) -> Vec<Notification> {
        let mut notifications = Vec::new();
        let mut completed_without_progress = false;
        if self.closed {
            if self.invariant_failed && self.queued > 0 {
                return self.drain_queued_with_error(broker, BrokerError::ArithmeticOverflow);
            }
            return notifications;
        }
        loop {
            if self.oversize_draining {
                if self.oversize_active || self.active_normal_loaders > 0 {
                    break;
                }
                let Some(request) = self.oversize_queue.pop_front() else {
                    self.oversize_draining = false;
                    continue;
                };
                let operation_closed = request
                    .operation_closed
                    .upgrade()
                    .is_none_or(|closed| closed.load(Ordering::Acquire));
                if request.cancellation.load(Ordering::Acquire) || operation_closed {
                    notifications.push(self.request_error(
                        request,
                        if operation_closed {
                            BrokerError::OperationClosed
                        } else {
                            BrokerError::Cancelled
                        },
                        broker,
                    ));
                    continue;
                }
                let remaining_queue_metadata = match self
                    .oversize_queue_metadata
                    .checked_sub(broker.config.queue_metadata_weight)
                {
                    Some(bytes) => bytes,
                    None => {
                        self.mark_invariant_failure();
                        notifications.push(self.request_error(
                            request,
                            BrokerError::ArithmeticOverflow,
                            broker,
                        ));
                        notifications.extend(
                            self.drain_queued_with_error(broker, BrokerError::ArithmeticOverflow),
                        );
                        break;
                    }
                };
                if request
                    .estimate
                    .checked_add(remaining_queue_metadata)
                    .is_none_or(|bytes| bytes > broker.config.oversize_limit)
                {
                    self.oversize_queue.push_front(request);
                    break;
                }
                if !self.grant_counters_available(&request) {
                    notifications.push(self.request_error(
                        request,
                        BrokerError::ArithmeticOverflow,
                        broker,
                    ));
                    if self.oversize_queue.is_empty() {
                        self.oversize_draining = false;
                    }
                    continue;
                }
                self.dequeue_request();
                if !self.release_request_metadata(Lane::Oversize, &broker.config) {
                    self.mark_invariant_failure();
                }
                self.oversize = request.estimate;
                self.oversize_active = true;
                self.grant_request(request, broker, &mut notifications);
                self.compact_empty_queues();
                break;
            }

            if self.normal_round.is_empty() || self.normal_cursor >= self.normal_round.len() {
                if completed_without_progress {
                    break;
                }
                self.normal_round = self
                    .operations
                    .iter()
                    .filter_map(|(id, record)| (!record.normal.is_empty()).then_some(*id))
                    .collect();
                if let Some(successor) = self.normal_successor {
                    if let Some(position) = self.normal_round.iter().position(|id| *id == successor)
                    {
                        self.normal_round.rotate_left(position);
                    }
                }
                self.normal_cursor = 0;
                self.refresh_admissible_cohort(&broker.config);
                completed_without_progress = true;
                if self.normal_round.is_empty() {
                    break;
                }
            }
            let operation_id = self.normal_round[self.normal_cursor];
            self.normal_cursor += 1;
            let Some(request) = self
                .operations
                .get(&operation_id)
                .and_then(|record| record.normal.front())
            else {
                continue;
            };
            let operation_closed = request
                .operation_closed
                .upgrade()
                .is_none_or(|closed| closed.load(Ordering::Acquire));
            if request.cancellation.load(Ordering::Acquire) || operation_closed {
                let request = self.pop_normal_front(operation_id).expect("round request");
                notifications.push(self.request_error(
                    request,
                    if operation_closed {
                        BrokerError::OperationClosed
                    } else {
                        BrokerError::Cancelled
                    },
                    broker,
                ));
                self.refresh_admissible_cohort(&broker.config);
                completed_without_progress = false;
                continue;
            }
            if !self.grant_counters_available(request) {
                let request = self
                    .pop_normal_front(operation_id)
                    .expect("overflowing request");
                notifications.push(self.request_error(
                    request,
                    BrokerError::ArithmeticOverflow,
                    broker,
                ));
                self.refresh_admissible_cohort(&broker.config);
                completed_without_progress = false;
                continue;
            }
            let queue_after = self.metadata;
            let aggregate = self
                .normal_payload
                .checked_add(request.estimate)
                .and_then(|bytes| bytes.checked_add(self.completion_reserve))
                .and_then(|bytes| bytes.checked_add(request.completion_reserve))
                .and_then(|bytes| bytes.checked_add(queue_after));
            let estimates_fit = self
                .normal_in_flight_estimate()
                .checked_add(request.estimate)
                .is_some_and(|bytes| {
                    bytes <= broker.config.normal_limit - broker.config.completion_reserve_limit
                });
            let reserve_fits = self
                .completion_reserve
                .checked_add(request.completion_reserve)
                .is_some_and(|bytes| bytes <= broker.config.completion_reserve_limit);
            if aggregate.is_none_or(|bytes| bytes > broker.config.normal_limit)
                || !estimates_fit
                || !reserve_fits
            {
                let pinned = request
                    .self_pinned
                    .load(Ordering::Acquire)
                    .min(self.normal_payload);
                let without_own_pins = (self.normal_payload - pinned)
                    .checked_add(request.estimate)
                    .and_then(|bytes| bytes.checked_add(self.completion_reserve))
                    .and_then(|bytes| bytes.checked_add(request.completion_reserve))
                    .and_then(|bytes| bytes.checked_add(queue_after));
                if pinned > 0
                    && estimates_fit
                    && reserve_fits
                    && without_own_pins.is_some_and(|bytes| bytes <= broker.config.normal_limit)
                {
                    let Some(denials) = self.denials.checked_add(1) else {
                        self.mark_invariant_failure();
                        notifications.extend(
                            self.drain_queued_with_error(broker, BrokerError::ArithmeticOverflow),
                        );
                        break;
                    };
                    let request = self
                        .pop_normal_front(operation_id)
                        .expect("self-pinned request");
                    self.denials = denials;
                    notifications.push(self.request_error(
                        request,
                        BrokerError::SelfPinned,
                        broker,
                    ));
                    self.refresh_admissible_cohort(&broker.config);
                    completed_without_progress = false;
                }
                continue;
            }
            let request = self
                .pop_normal_front(operation_id)
                .expect("admitted request");
            self.dequeue_request();
            self.normal_payload += request.estimate;
            self.normal_in_flight_estimates += request.estimate;
            self.normal_in_flight_count += 1;
            self.completion_reserve += request.completion_reserve;
            self.active_normal_loaders += 1;
            self.normal_successor = self
                .normal_round
                .get(self.normal_cursor)
                .copied()
                .or_else(|| self.normal_round.first().copied());
            self.grant_request(request, broker, &mut notifications);
            self.refresh_admissible_cohort(&broker.config);
            self.measure_admissible_cohort();
            completed_without_progress = false;
        }
        self.update_peaks(&broker.config);
        notifications
    }

    fn grant_request(
        &mut self,
        request: QueuedRequest,
        broker: &Arc<BrokerInner>,
        notifications: &mut Vec<Notification>,
    ) {
        self.next_grant_ordinal += 1;
        if let Some(operation) = request.operation.upgrade() {
            operation.grants.fetch_add(1, Ordering::AcqRel);
        }
        self.grants += 1;
        self.in_flight += 1;
        let record = self
            .operations
            .get_mut(&request.operation_id)
            .expect("active operation");
        record.in_flight += 1;
        record.grants += 1;
        record.granted_bytes += request.estimate;
        if request.lane == Lane::Oversize {
            self.oversize_in_flight_metadata = broker.config.queue_metadata_weight;
        }
        notifications.push(Notification {
            wait: request.wait,
            outcome: WaitOutcome::Granted(GrantLease {
                broker: Arc::clone(broker),
                operation_id: request.operation_id,
                estimate: request.estimate,
                completion_reserve: request.completion_reserve,
                lane: request.lane,
                cancellation: request.cancellation,
                operation_closed: request.operation_closed,
                grant_ordinal: self.next_grant_ordinal,
                request_metadata_bytes: broker.config.queue_metadata_weight,
                active: true,
            }),
        });
    }

    fn normal_in_flight_estimate(&self) -> u64 {
        // Retained and in-flight normal bytes share the same hard B ledger. The
        // aggregate B check is authoritative; this conservative value also
        // prevents estimates alone from consuming completion reserve R.
        self.normal_in_flight_estimates
    }

    fn grant_counters_available(&self, request: &QueuedRequest) -> bool {
        self.next_grant_ordinal < u64::MAX
            && self.grants < u64::MAX
            && self.in_flight < usize::MAX
            && request
                .operation
                .upgrade()
                .is_some_and(|operation| operation.grants.load(Ordering::Acquire) < u64::MAX)
            && self
                .operations
                .get(&request.operation_id)
                .is_some_and(|record| {
                    record.in_flight < usize::MAX
                        && record.grants < u64::MAX
                        && record.granted_bytes.checked_add(request.estimate).is_some()
                })
    }

    fn normal_request_admissible(&self, request: &QueuedRequest, config: &BrokerConfig) -> bool {
        let aggregate = self
            .normal_payload
            .checked_add(request.estimate)
            .and_then(|bytes| bytes.checked_add(self.completion_reserve))
            .and_then(|bytes| bytes.checked_add(request.completion_reserve))
            .and_then(|bytes| bytes.checked_add(self.metadata));
        let estimates_fit = self
            .normal_in_flight_estimate()
            .checked_add(request.estimate)
            .is_some_and(|bytes| bytes <= config.normal_limit - config.completion_reserve_limit);
        let reserve_fits = self
            .completion_reserve
            .checked_add(request.completion_reserve)
            .is_some_and(|bytes| bytes <= config.completion_reserve_limit);
        aggregate.is_some_and(|bytes| bytes <= config.normal_limit) && estimates_fit && reserve_fits
    }

    fn refresh_admissible_cohort(&mut self, config: &BrokerConfig) {
        let admissible = self
            .operations
            .iter()
            .filter_map(|(operation_id, record)| {
                if record.closed {
                    return None;
                }
                let request = record.normal.front()?;
                let operation_closed = request
                    .operation_closed
                    .upgrade()
                    .is_none_or(|closed| closed.load(Ordering::Acquire));
                if operation_closed || request.cancellation.load(Ordering::Acquire) {
                    return None;
                }
                self.normal_request_admissible(request, config)
                    .then_some((*operation_id, record.grants))
            })
            .collect::<BTreeMap<_, _>>();
        self.cohort_baseline
            .retain(|operation_id, _| admissible.contains_key(operation_id));
        for (operation_id, grants) in admissible {
            self.cohort_baseline.entry(operation_id).or_insert(grants);
        }
    }

    fn measure_admissible_cohort(&mut self) {
        let mut minimum = None;
        let mut maximum = None;
        for (operation_id, baseline) in &self.cohort_baseline {
            let Some(grants) = self
                .operations
                .get(operation_id)
                .map(|record| record.grants)
            else {
                continue;
            };
            let Some(delta) = grants.checked_sub(*baseline) else {
                self.mark_invariant_failure();
                return;
            };
            minimum = Some(minimum.map_or(delta, |value: u64| value.min(delta)));
            maximum = Some(maximum.map_or(delta, |value: u64| value.max(delta)));
        }
        if let Some(lag) = minimum
            .zip(maximum)
            .and_then(|(minimum, maximum)| maximum.checked_sub(minimum))
        {
            self.maximum_admissible_lag = self.maximum_admissible_lag.max(lag);
        }
    }

    fn dequeue_request(&mut self) {
        if !Self::decrease_usize(&mut self.queued, 1) {
            self.mark_invariant_failure();
        }
    }

    fn release_request_metadata(&mut self, lane: Lane, config: &BrokerConfig) -> bool {
        match lane {
            Lane::Normal { .. } => {
                Self::decrease_u64(&mut self.metadata, config.queue_metadata_weight)
            }
            Lane::Oversize => Self::decrease_u64(
                &mut self.oversize_queue_metadata,
                config.queue_metadata_weight,
            ),
        }
    }

    fn pop_normal_front(&mut self, operation_id: u64) -> Option<QueuedRequest> {
        let record = self.operations.get_mut(&operation_id)?;
        let request = record.normal.pop_front();
        if record.normal.is_empty() {
            record.normal = VecDeque::new();
        } else {
            record.normal.shrink_to_fit();
        }
        request
    }

    fn remove_queued(
        &mut self,
        operation_id: u64,
        request_id: u64,
        config: &BrokerConfig,
    ) -> Option<QueuedRequest> {
        let normal_index = self.operations.get(&operation_id).and_then(|record| {
            record
                .normal
                .iter()
                .position(|request| request.id == request_id)
        });
        let removed = if let Some(index) = normal_index {
            self.operations
                .get_mut(&operation_id)
                .unwrap()
                .normal
                .remove(index)
        } else {
            self.oversize_queue
                .iter()
                .position(|request| {
                    request.id == request_id && request.operation_id == operation_id
                })
                .and_then(|index| self.oversize_queue.remove(index))
        };
        if removed.is_some() {
            self.compact_empty_queues();
            self.refresh_admissible_cohort(config);
            if self.oversize_queue.is_empty() && !self.oversize_active {
                self.oversize_draining = false;
            }
        }
        removed
    }

    fn drain_queued_with_error(
        &mut self,
        broker: &Arc<BrokerInner>,
        error: BrokerError,
    ) -> Vec<Notification> {
        let mut requests = Vec::new();
        for record in self.operations.values_mut() {
            record.closed = true;
            requests.extend(record.normal.drain(..));
            record.normal = VecDeque::new();
        }
        requests.extend(self.oversize_queue.drain(..));
        self.oversize_queue = VecDeque::new();
        let mut notifications = Vec::with_capacity(requests.len());
        for request in requests {
            notifications.push(self.request_error(request, error.clone(), broker));
        }
        self.normal_round.clear();
        self.cohort_baseline.clear();
        self.oversize_draining = false;
        let operation_ids = self.operations.keys().copied().collect::<Vec<_>>();
        for operation_id in operation_ids {
            self.prune_operation(operation_id, &broker.config);
        }
        notifications
    }

    fn finish_operation_flight(&mut self, operation_id: u64, config: &BrokerConfig) {
        if let Some(record) = self.operations.get_mut(&operation_id) {
            if !Self::decrease_usize(&mut record.in_flight, 1) {
                self.mark_invariant_failure();
            }
        } else {
            self.mark_invariant_failure();
        }
        self.prune_operation(operation_id, config);
    }

    fn prune_operation(&mut self, operation_id: u64, config: &BrokerConfig) {
        let removable = self.operations.get(&operation_id).is_some_and(|record| {
            record.closed
                && record.normal.is_empty()
                && record.in_flight == 0
                && record.error_owners == 0
                && record.cache_bytes == 0
                && record.pin_bytes == 0
                && record.bypass_bytes == 0
                && record.self_pinned_bytes == 0
        }) && !self
            .oversize_queue
            .iter()
            .any(|request| request.operation_id == operation_id);
        if removable {
            self.operations.remove(&operation_id);
            if !Self::decrease_u64(&mut self.metadata, config.operation_metadata_weight) {
                self.mark_invariant_failure();
            }
        }
    }

    fn compact_empty_queues(&mut self) {
        if self.oversize_queue.is_empty() {
            self.oversize_queue = VecDeque::new();
        } else {
            self.oversize_queue.shrink_to_fit();
        }
        for record in self.operations.values_mut() {
            if record.normal.is_empty() {
                record.normal = VecDeque::new();
            } else {
                record.normal.shrink_to_fit();
            }
        }
    }

    fn release_grant(&mut self, lease: &GrantLease, config: &BrokerConfig) {
        let mut valid = match lease.lane {
            Lane::Normal { .. } => {
                let mut valid = Self::decrease_u64(&mut self.normal_payload, lease.estimate);
                valid &= Self::decrease_u64(&mut self.normal_in_flight_estimates, lease.estimate);
                valid &= Self::decrease_u64(&mut self.completion_reserve, lease.completion_reserve);
                valid &= Self::decrease_usize(&mut self.active_normal_loaders, 1);
                valid &= Self::decrease_u64(&mut self.metadata, lease.request_metadata_bytes);
                valid &= Self::decrease_usize(&mut self.normal_in_flight_count, 1);
                valid
            }
            Lane::Oversize => {
                let valid = Self::decrease_u64(&mut self.oversize, lease.estimate);
                let metadata_valid =
                    self.oversize_in_flight_metadata == lease.request_metadata_bytes;
                self.oversize_in_flight_metadata = 0;
                self.oversize_active = false;
                if self.oversize_queue.is_empty() {
                    self.oversize_draining = false;
                }
                valid && metadata_valid
            }
        };
        valid &= Self::decrease_usize(&mut self.in_flight, 1);
        valid &= Self::decrease_usize(&mut self.live_request_records, 1);
        if !valid {
            self.mark_invariant_failure();
        }
        self.finish_operation_flight(lease.operation_id, config);
    }

    fn update_peaks(&mut self, config: &BrokerConfig) {
        let Some(oversize) = self.oversize.checked_add(self.oversize_queue_metadata) else {
            self.mark_invariant_failure();
            return;
        };
        let accounting = self
            .normal_payload
            .checked_add(self.metadata)
            .and_then(|normal| {
                normal
                    .checked_add(self.completion_reserve)
                    .and_then(|with_reserve| {
                        with_reserve
                            .checked_add(oversize)
                            .map(|aggregate| (normal, with_reserve, aggregate))
                    })
            });
        let Some((normal, with_reserve, aggregate)) = accounting else {
            self.mark_invariant_failure();
            return;
        };
        let total_limit = config
            .normal_limit
            .checked_add(config.oversize_limit)
            .expect("validated config");
        if with_reserve > config.normal_limit
            || self.completion_reserve > config.completion_reserve_limit
            || oversize > config.oversize_limit
            || aggregate > total_limit
        {
            self.mark_invariant_failure();
            return;
        }
        debug_assert!(with_reserve <= config.normal_limit);
        debug_assert!(self.completion_reserve <= config.completion_reserve_limit);
        debug_assert!(oversize <= config.oversize_limit);
        debug_assert!(aggregate <= total_limit);
        self.peak_normal = self.peak_normal.max(normal);
        self.peak_reserve = self.peak_reserve.max(self.completion_reserve);
        self.peak_oversize = self.peak_oversize.max(oversize);
        self.peak_aggregate = self.peak_aggregate.max(aggregate);
        self.peak_cache_bytes = self.peak_cache_bytes.max(self.cache_bytes);
        self.peak_pin_bytes = self.peak_pin_bytes.max(self.pin_bytes);
        self.peak_bypass_bytes = self.peak_bypass_bytes.max(self.bypass_bytes);
    }

    fn snapshot(&self, config: &BrokerConfig) -> BrokerSnapshot {
        let mut diagnostic_overflow = false;
        let normal = self
            .normal_payload
            .checked_add(self.metadata)
            .unwrap_or_else(|| {
                diagnostic_overflow = true;
                0
            });
        let oversize = self
            .oversize
            .checked_add(self.oversize_queue_metadata)
            .unwrap_or_else(|| {
                diagnostic_overflow = true;
                0
            });
        let aggregate = normal
            .checked_add(self.completion_reserve)
            .and_then(|bytes| bytes.checked_add(oversize))
            .unwrap_or_else(|| {
                diagnostic_overflow = true;
                0
            });
        let reservation_metadata_bytes = (self.normal_in_flight_count as u64)
            .checked_mul(config.queue_metadata_weight)
            .and_then(|bytes| bytes.checked_add(self.oversize_in_flight_metadata))
            .unwrap_or_else(|| {
                diagnostic_overflow = true;
                0
            });
        let error_metadata_bytes = u64::try_from(self.error_metadata_count)
            .ok()
            .and_then(|count| count.checked_mul(config.queue_metadata_weight))
            .unwrap_or_else(|| {
                diagnostic_overflow = true;
                0
            });
        let mut operations = self
            .operations
            .iter()
            .map(|(operation_id, record)| {
                (
                    *operation_id,
                    OperationSnapshot {
                        normal_retained_cap: record.normal_retained_cap,
                        queued: record.normal.len(),
                        in_flight: record.in_flight,
                        error_owners: record.error_owners,
                        grants: record.grants,
                        granted_bytes: record.granted_bytes,
                        cache_bytes: record.cache_bytes,
                        pin_bytes: record.pin_bytes,
                        bypass_bytes: record.bypass_bytes,
                        self_pinned_bytes: record.self_pinned_bytes,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        for request in &self.oversize_queue {
            if let Some(operation) = operations.get_mut(&request.operation_id) {
                if let Some(queued) = operation.queued.checked_add(1) {
                    operation.queued = queued;
                } else {
                    diagnostic_overflow = true;
                }
            }
        }
        BrokerSnapshot {
            normal_limit_bytes: config.normal_limit,
            normal_payload_bytes: self.normal_payload,
            normal_in_flight_estimate_bytes: self.normal_in_flight_estimates,
            metadata_bytes: self.metadata,
            completion_reserve_bytes: self.completion_reserve,
            oversize_bytes: oversize,
            aggregate_bytes: aggregate,
            peak_normal_bytes: self.peak_normal,
            peak_completion_reserve_bytes: self.peak_reserve,
            peak_oversize_bytes: self.peak_oversize,
            peak_aggregate_bytes: self.peak_aggregate,
            queued: self.queued,
            in_flight: self.in_flight,
            live_request_records: self.live_request_records,
            error_metadata_bytes,
            reservation_metadata_bytes,
            active_operations: self.operations.len(),
            grants: self.grants,
            denials: self.denials,
            cancellations: self.cancellations,
            reconciliations: self.reconciliations,
            maximum_admissible_lag: self.maximum_admissible_lag,
            oversize_owners: u8::from(self.oversize_active),
            cache_bytes: self.cache_bytes,
            pin_bytes: self.pin_bytes,
            bypass_bytes: self.bypass_bytes,
            peak_cache_bytes: self.peak_cache_bytes,
            peak_pin_bytes: self.peak_pin_bytes,
            peak_bypass_bytes: self.peak_bypass_bytes,
            operations,
            invariant_failed: self.invariant_failed || diagnostic_overflow,
            closed: self.closed || diagnostic_overflow,
        }
    }
}

fn notify_all(notifications: Vec<Notification>) {
    for notification in notifications {
        let mut outcome = lock_recover(&notification.wait.outcome);
        *outcome = notification.outcome;
        notification.wait.ready.notify_all();
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_recover<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::thread;

    fn config() -> BrokerConfig {
        BrokerConfig {
            normal_limit: 8_192,
            oversize_limit: 4_096,
            completion_reserve_limit: 2_048,
            queue_metadata_weight: 256,
            operation_metadata_weight: 256,
            max_active_operations: 16,
            max_queued_requests: 64,
        }
    }

    fn broker() -> BudgetBroker {
        BudgetBroker::new(config()).unwrap()
    }

    fn assert_drained(broker: &BudgetBroker) {
        let snapshot = broker.snapshot();
        assert_eq!(snapshot.normal_payload_bytes, 0);
        assert_eq!(snapshot.metadata_bytes, 0);
        assert_eq!(snapshot.completion_reserve_bytes, 0);
        assert_eq!(snapshot.oversize_bytes, 0);
        assert_eq!(snapshot.aggregate_bytes, 0);
        assert_eq!(snapshot.queued, 0);
        assert_eq!(snapshot.in_flight, 0);
        assert_eq!(snapshot.live_request_records, 0);
        assert_eq!(snapshot.error_metadata_bytes, 0);
        assert_eq!(snapshot.reservation_metadata_bytes, 0);
        assert_eq!(snapshot.active_operations, 0);
        assert_eq!(snapshot.cache_bytes, 0);
        assert_eq!(snapshot.pin_bytes, 0);
        assert_eq!(snapshot.bypass_bytes, 0);
        assert!(snapshot.operations.is_empty());
        assert!(!snapshot.invariant_failed);
    }

    #[test]
    fn production_constants_and_config_are_exact() {
        let config = BrokerConfig::production();
        assert_eq!(config.normal_limit, 134_217_728);
        assert_eq!(config.oversize_limit, 67_108_864);
        assert_eq!(config.completion_reserve_limit, 33_554_432);
        assert_eq!(config.normal_limit + config.oversize_limit, 201_326_592);
        assert_eq!(config.queue_metadata_weight, 256);
        assert_eq!(config.operation_metadata_weight, 2_048);
        assert_eq!(config.max_active_operations, 65_536);
        assert_eq!(config.max_queued_requests, 65_536);
        assert!(BudgetBroker::new(config).is_ok());
        assert_eq!(BudgetBroker::production().inner.config, config);
    }

    #[test]
    fn invalid_and_overflowing_requests_fail_without_state() {
        let invalid = BrokerConfig {
            completion_reserve_limit: 9,
            normal_limit: 8,
            ..config()
        };
        assert!(matches!(
            BudgetBroker::new(invalid),
            Err(BrokerError::InvalidConfig)
        ));

        let broker = broker();
        let operation = broker.register_operation().unwrap();
        assert!(matches!(
            operation.request(
                Lane::Normal {
                    completion_reserve: u64::MAX
                },
                1
            ),
            Err(BrokerError::ArithmeticOverflow)
        ));
        assert!(matches!(
            operation.request(Lane::Oversize, 4_097),
            Err(BrokerError::ResourceLimit)
        ));
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn normal_reservation_reconciles_atomically_from_its_own_reserve() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let reservation = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 1_024,
                },
                4_096,
            )
            .unwrap();
        assert_eq!(reservation.estimate(), 4_096);
        let held = broker.snapshot();
        assert_eq!(held.normal_payload_bytes, 4_096);
        assert_eq!(held.completion_reserve_bytes, 1_024);
        assert_eq!(held.metadata_bytes, 512);
        assert_eq!(held.in_flight, 1);

        let retained = reservation.reconcile(4_608).unwrap();
        assert_eq!(retained.bytes(), 4_608);
        let published = broker.snapshot();
        assert_eq!(published.normal_payload_bytes, 4_608);
        assert_eq!(published.completion_reserve_bytes, 0);
        assert_eq!(published.metadata_bytes, 256);
        assert_eq!(published.bypass_bytes, 4_608);
        assert_eq!(published.in_flight, 0);
        assert!(published.peak_aggregate_bytes <= 8_192);
        drop(retained);
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn reconciliation_above_own_reserve_publishes_nothing() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let reservation = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 512,
                },
                1_024,
            )
            .unwrap();
        assert!(matches!(
            reservation.reconcile(1_537),
            Err(BrokerError::ReconciliationLimit)
        ));
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn exact_and_one_short_admission_refuse_before_loader_entry() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let exact = operation
            .try_reserve(
                Lane::Normal {
                    completion_reserve: 1_536,
                },
                6_144,
            )
            .unwrap();
        assert_eq!(broker.snapshot().aggregate_bytes, 8_192);
        drop(exact);

        let entered = AtomicU64::new(0);
        let one_short = operation.try_reserve(
            Lane::Normal {
                completion_reserve: 1_537,
            },
            6_144,
        );
        if one_short.is_ok() {
            entered.fetch_add(1, Ordering::Relaxed);
        }
        assert!(matches!(one_short, Err(BrokerError::ResourceLimit)));
        assert_eq!(entered.load(Ordering::Relaxed), 0);
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn queued_metadata_is_charged_and_cancel_drains_idle_capacity() {
        let broker = broker();
        let blocker_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let waiting_operation = broker.register_operation().unwrap();
        let pending = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();
        let queued = broker.snapshot();
        assert_eq!(queued.queued, 1);
        assert_eq!(queued.active_operations, 2);
        assert_eq!(queued.metadata_bytes, 4 * 256);
        pending.cancel();
        assert!(matches!(pending.wait(), Err(BrokerError::Cancelled)));
        assert_eq!(broker.snapshot().metadata_bytes, 3 * 256);
        drop(blocker);
        blocker_operation.close();
        waiting_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn cancellation_handle_cancels_a_queued_request_through_the_broker() {
        let broker = broker();
        let blocker_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let waiting_operation = broker.register_operation().unwrap();
        let pending = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();
        let cancellation = pending.cancellation_handle();
        let cloned = cancellation.clone();

        cloned.cancel();
        let retained_error = broker.snapshot();
        assert_eq!(retained_error.queued, 0);
        assert_eq!(retained_error.cancellations, 1);
        assert_eq!(retained_error.error_metadata_bytes, 256);
        assert!(matches!(pending.wait(), Err(BrokerError::Cancelled)));
        assert_eq!(broker.snapshot().error_metadata_bytes, 0);

        drop(cancellation);
        drop(blocker);
        blocker_operation.close();
        waiting_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn cancellation_handle_after_grant_rejects_reconciliation() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let pending = operation
            .request(
                Lane::Normal {
                    completion_reserve: 128,
                },
                512,
            )
            .unwrap();
        let cancellation = pending.cancellation_handle();
        let reservation = pending.wait().unwrap();

        cancellation.cancel();
        assert!(reservation.is_cancelled());
        assert!(matches!(
            reservation.reconcile(512),
            Err(BrokerError::Cancelled)
        ));

        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn dropping_cancellation_handle_does_not_cancel_queued_request() {
        let broker = broker();
        let blocker_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let waiting_operation = broker.register_operation().unwrap();
        let pending = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();
        let cancellation = pending.cancellation_handle();

        drop(cancellation);
        assert_eq!(broker.snapshot().queued, 1);
        drop(blocker);
        let reservation = pending.wait().unwrap();
        assert!(!reservation.is_cancelled());
        drop(reservation);

        blocker_operation.close();
        waiting_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn broker_close_keeps_queued_error_typed_when_handle_cancels_later() {
        let broker = broker();
        let blocker_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let waiting_operation = broker.register_operation().unwrap();
        let pending = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();
        let cancellation = pending.cancellation_handle();

        broker.close();
        cancellation.cancel();
        assert_eq!(broker.snapshot().error_metadata_bytes, 256);
        assert!(matches!(pending.wait(), Err(BrokerError::Closed)));
        assert_eq!(broker.snapshot().error_metadata_bytes, 0);

        drop(blocker);
        drop(cancellation);
        drop(blocker_operation);
        drop(waiting_operation);
        assert_drained(&broker);
    }

    #[test]
    fn live_request_and_active_operation_caps_refuse_before_mutation() {
        let capped = BrokerConfig {
            max_active_operations: 2,
            max_queued_requests: 2,
            ..config()
        };
        let broker = BudgetBroker::new(capped).unwrap();
        let blocker = broker
            .register_operation()
            .unwrap()
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let second = broker.register_operation().unwrap();
        let pending = second
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();
        assert!(matches!(
            broker.register_operation(),
            Err(BrokerError::OperationFull)
        ));
        let same_operation_second = second.request(
            Lane::Normal {
                completion_reserve: 0,
            },
            2_048,
        );
        assert!(matches!(same_operation_second, Err(BrokerError::QueueFull)));
        pending.cancel();
        let _ = pending.wait();
        drop(blocker);
        second.close();
        assert_drained(&broker);
    }

    #[test]
    fn cancelled_error_owner_stays_charged_and_capped_until_consumed() {
        let capped = BrokerConfig {
            max_queued_requests: 2,
            ..config()
        };
        let broker = BudgetBroker::new(capped).unwrap();
        let blocker_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let waiting_operation = broker.register_operation().unwrap();
        let pending = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();

        pending.cancel();
        let retained_error = broker.snapshot();
        assert_eq!(retained_error.queued, 0);
        assert_eq!(retained_error.in_flight, 1);
        assert_eq!(retained_error.live_request_records, 2);
        assert_eq!(retained_error.error_metadata_bytes, 256);
        assert_eq!(
            retained_error.operations[&waiting_operation.id()].error_owners,
            1
        );
        assert!(matches!(
            waiting_operation.request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1
            ),
            Err(BrokerError::QueueFull)
        ));

        assert!(matches!(pending.wait(), Err(BrokerError::Cancelled)));
        let consumed = broker.snapshot();
        assert_eq!(consumed.live_request_records, 1);
        assert_eq!(consumed.error_metadata_bytes, 0);
        assert_eq!(consumed.operations[&waiting_operation.id()].error_owners, 0);
        let replacement = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();
        replacement.cancel();
        assert_eq!(broker.snapshot().live_request_records, 2);
        assert!(matches!(
            waiting_operation.request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048
            ),
            Err(BrokerError::QueueFull)
        ));
        assert!(matches!(replacement.wait(), Err(BrokerError::Cancelled)));
        drop(blocker);
        blocker_operation.close();
        waiting_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn closed_operation_error_owner_survives_handle_close_until_pending_drop() {
        let broker = broker();
        let blocker_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let waiting_operation = broker.register_operation().unwrap();
        let waiting_id = waiting_operation.id();
        let pending = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();

        waiting_operation.close();
        let retained_error = broker.snapshot();
        assert_eq!(retained_error.error_metadata_bytes, 256);
        assert_eq!(retained_error.operations[&waiting_id].error_owners, 1);
        drop(pending);
        assert!(!broker.snapshot().operations.contains_key(&waiting_id));

        drop(blocker);
        blocker_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn oversize_cancelled_error_metadata_stays_in_o_until_consumed() {
        let capped = BrokerConfig {
            max_queued_requests: 2,
            ..config()
        };
        let broker = BudgetBroker::new(capped).unwrap();
        let blocker_operation = broker.register_oversize_operation(0).unwrap();
        let waiting_operation = broker.register_oversize_operation(0).unwrap();
        let blocker = blocker_operation.reserve(Lane::Oversize, 1_024).unwrap();
        let pending = waiting_operation.request(Lane::Oversize, 1_024).unwrap();

        pending.cancel();
        let retained_error = broker.snapshot();
        assert_eq!(retained_error.queued, 0);
        assert_eq!(retained_error.live_request_records, 2);
        assert_eq!(retained_error.error_metadata_bytes, 256);
        assert_eq!(retained_error.oversize_bytes, 1_280);
        assert!(matches!(
            waiting_operation.request(Lane::Oversize, 1_024),
            Err(BrokerError::QueueFull)
        ));
        assert!(matches!(pending.wait(), Err(BrokerError::Cancelled)));
        let consumed = broker.snapshot();
        assert_eq!(consumed.live_request_records, 1);
        assert_eq!(consumed.error_metadata_bytes, 0);
        assert_eq!(consumed.oversize_bytes, 1_024);

        drop(blocker);
        blocker_operation.close();
        waiting_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn oversize_drain_waits_for_loaders_and_persists_through_owner_drop() {
        let broker = broker();
        let normal_operation = broker.register_operation().unwrap();
        let normal_loader = normal_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_024,
            )
            .unwrap();
        let oversize_operation = broker.register_oversize_operation(2_048).unwrap();
        let oversize = oversize_operation.request(Lane::Oversize, 3_000).unwrap();
        let second_oversize_operation = broker.register_oversize_operation(1_024).unwrap();
        let second_oversize = second_oversize_operation
            .request(Lane::Oversize, 2_048)
            .unwrap();
        let later_operation = broker.register_operation().unwrap();
        let later_normal = later_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                512,
            )
            .unwrap();
        assert_eq!(broker.snapshot().queued, 3);
        drop(normal_loader);
        let oversize = oversize.wait().unwrap();
        let first_ordinal = oversize.grant_ordinal();
        assert_eq!(broker.snapshot().oversize_owners, 1);
        assert_eq!(broker.snapshot().queued, 2);
        let oversize = oversize.reconcile(3_000).unwrap();
        assert_eq!(broker.snapshot().queued, 2);
        drop(oversize);
        let second_oversize = second_oversize.wait().unwrap();
        assert!(second_oversize.grant_ordinal() > first_ordinal);
        assert_eq!(broker.snapshot().queued, 1);
        let second_oversize = second_oversize.reconcile(2_048).unwrap();
        assert_eq!(broker.snapshot().queued, 1);
        drop(second_oversize);
        let later_normal = later_normal.wait().unwrap();
        assert!(later_normal.grant_ordinal() > 0);
        drop(later_normal);
        normal_operation.close();
        oversize_operation.close();
        second_oversize_operation.close();
        later_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn cancellation_and_close_before_reconcile_publish_nothing() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let reservation = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 128,
                },
                512,
            )
            .unwrap();
        reservation.cancel();
        assert!(reservation.is_cancelled());
        assert!(matches!(
            reservation.reconcile(512),
            Err(BrokerError::Cancelled)
        ));
        operation.close();
        assert_drained(&broker);

        let operation = broker.register_operation().unwrap();
        let reservation = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 128,
                },
                512,
            )
            .unwrap();
        broker.close();
        assert!(matches!(
            reservation.reconcile(512),
            Err(BrokerError::Closed)
        ));
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn self_pin_refusal_never_promotes_to_oversize() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let retained = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap()
            .reconcile(6_000)
            .unwrap();
        assert!(matches!(
            retained.pin(&operation, 6_001),
            Err(BrokerError::ResourceLimit)
        ));
        let pin = retained.pin(&operation, 6_000).unwrap();
        let pending = operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();
        assert!(matches!(pending.wait(), Err(BrokerError::SelfPinned)));
        assert_eq!(broker.snapshot().oversize_owners, 0);
        drop(pin);
        drop(retained);
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn unwind_and_simultaneous_retained_drops_release_once() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let reservation = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 256,
                },
                1_024,
            )
            .unwrap();
        let unwind = catch_unwind(AssertUnwindSafe(move || {
            let _reservation = reservation;
            panic!("synthetic loader unwind");
        }));
        assert!(unwind.is_err());
        assert_eq!(broker.snapshot().normal_payload_bytes, 0);

        let first = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_024,
            )
            .unwrap()
            .reconcile(1_024)
            .unwrap();
        let second = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_024,
            )
            .unwrap()
            .reconcile(1_024)
            .unwrap();
        let first_drop = thread::spawn(move || drop(first));
        let second_drop = thread::spawn(move || drop(second));
        first_drop.join().unwrap();
        second_drop.join().unwrap();
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn coherent_sampler_never_observes_reconciliation_gap_or_breach() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let reservation = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 1_024,
                },
                4_096,
            )
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let sampler_broker = broker.clone();
        let sampler_stop = Arc::clone(&stop);
        let sampler = thread::spawn(move || {
            let mut samples = 0;
            while !sampler_stop.load(Ordering::Acquire) {
                let snapshot = sampler_broker.snapshot();
                assert!(
                    snapshot.normal_payload_bytes
                        + snapshot.metadata_bytes
                        + snapshot.completion_reserve_bytes
                        <= 8_192
                );
                assert!(snapshot.completion_reserve_bytes <= 2_048);
                assert!(snapshot.oversize_bytes <= 4_096);
                assert!(snapshot.aggregate_bytes <= 12_288);
                samples += 1;
            }
            samples
        });
        let retained = reservation.reconcile(5_120).unwrap();
        stop.store(true, Ordering::Release);
        let _ = sampler.join().unwrap();
        assert_eq!(broker.snapshot().normal_payload_bytes, 5_120);
        drop(retained);
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn one_thousand_reversed_arrival_rounds_follow_registration_order() {
        let broker = BudgetBroker::new(BrokerConfig {
            normal_limit: 4_000_000,
            oversize_limit: 2_000_000,
            completion_reserve_limit: 400_000,
            queue_metadata_weight: 256,
            operation_metadata_weight: 256,
            max_active_operations: 8,
            max_queued_requests: 2_100,
        })
        .unwrap();
        let blocker_operation = broker.register_operation().unwrap();
        let first = broker.register_operation().unwrap();
        let second = broker.register_operation().unwrap();
        assert!(first.id() < second.id());
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_800_000,
            )
            .unwrap();
        let mut first_requests = Vec::with_capacity(1_000);
        let mut second_requests = Vec::with_capacity(1_000);
        for _ in 0..1_000 {
            second_requests.push(Some(
                second
                    .request(
                        Lane::Normal {
                            completion_reserve: 0,
                        },
                        1_200_000,
                    )
                    .unwrap(),
            ));
            first_requests.push(Some(
                first
                    .request(
                        Lane::Normal {
                            completion_reserve: 0,
                        },
                        1_200_000,
                    )
                    .unwrap(),
            ));
        }
        assert_eq!(broker.snapshot().queued, 2_000);
        drop(blocker);

        for round in 0..1_000 {
            let first_reservation = first_requests[round].take().unwrap().wait().unwrap();
            let second_reservation = second_requests[round].take().unwrap().wait().unwrap();
            assert_eq!(first_reservation.grant_ordinal(), 2 + (round as u64 * 2));
            assert_eq!(second_reservation.grant_ordinal(), 3 + (round as u64 * 2));
            drop(first_reservation.reconcile(0).unwrap());
            drop(second_reservation.reconcile(0).unwrap());
        }
        assert_eq!(first.grant_count(), 1_000);
        assert_eq!(second.grant_count(), 1_000);
        let final_snapshot = broker.snapshot();
        assert_eq!(final_snapshot.maximum_admissible_lag, 1);
        blocker_operation.close();
        first.close();
        second.close();
        assert_drained(&broker);
    }

    #[test]
    fn four_operation_round_and_cursor_cancellation_are_deterministic() {
        let broker = BudgetBroker::new(BrokerConfig {
            normal_limit: 32_768,
            oversize_limit: 16_384,
            completion_reserve_limit: 4_096,
            queue_metadata_weight: 256,
            operation_metadata_weight: 256,
            max_active_operations: 16,
            max_queued_requests: 32,
        })
        .unwrap();
        let blocker_operation = broker.register_operation().unwrap();
        let operations: Vec<_> = (0..4)
            .map(|_| broker.register_operation().unwrap())
            .collect();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                26_000,
            )
            .unwrap();
        let pending: Vec<_> = operations
            .iter()
            .map(|operation| {
                operation
                    .request(
                        Lane::Normal {
                            completion_reserve: 0,
                        },
                        6_000,
                    )
                    .unwrap()
            })
            .collect();
        assert_eq!(broker.snapshot().queued, 4);
        drop(blocker);
        for (index, pending) in pending.into_iter().enumerate() {
            let reservation = pending.wait().unwrap();
            assert_eq!(reservation.grant_ordinal(), 2 + index as u64);
            drop(reservation.reconcile(0).unwrap());
        }
        assert_eq!(broker.snapshot().normal_payload_bytes, 0);

        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                26_000,
            )
            .unwrap();
        let mut pending: Vec<_> = operations
            .iter()
            .map(|operation| {
                operation
                    .request(
                        Lane::Normal {
                            completion_reserve: 0,
                        },
                        6_000,
                    )
                    .unwrap()
            })
            .collect();
        pending[0].cancel();
        let cancelled = pending.remove(0);
        assert!(matches!(cancelled.wait(), Err(BrokerError::Cancelled)));
        drop(blocker);
        let mut ordinals = Vec::new();
        for pending in pending {
            let reservation = pending.wait().unwrap();
            ordinals.push(reservation.grant_ordinal());
            drop(reservation.reconcile(0).unwrap());
        }
        assert!(ordinals.windows(2).all(|pair| pair[0] < pair[1]));
        blocker_operation.close();
        for operation in &operations {
            operation.close();
        }
        assert_drained(&broker);
    }

    #[test]
    fn operation_close_cancels_queued_requests_but_not_live_owners() {
        let broker = broker();
        let blocker_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let operation = broker.register_operation().unwrap();
        let pending = operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_048,
            )
            .unwrap();
        operation.close();
        assert!(matches!(pending.wait(), Err(BrokerError::OperationClosed)));
        assert_eq!(broker.snapshot().in_flight, 1);
        drop(blocker);
        blocker_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn oversize_requires_explicit_operation_class_and_exact_o_coexists_with_exact_b() {
        let broker = broker();
        let normal = broker.register_operation().unwrap();
        let filler = broker.register_operation().unwrap();
        let oversize = broker.register_oversize_operation(2_048).unwrap();

        assert!(matches!(
            normal.request(Lane::Oversize, 1),
            Err(BrokerError::ResourceLimit)
        ));
        assert!(matches!(
            oversize.request(Lane::Oversize, 2_048),
            Err(BrokerError::ResourceLimit)
        ));
        assert!(matches!(
            oversize.request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                2_049,
            ),
            Err(BrokerError::ResourceLimit)
        ));
        let capped_normal = oversize
            .reserve(
                Lane::Normal {
                    completion_reserve: 128,
                },
                2_048,
            )
            .unwrap();
        assert!(matches!(
            capped_normal.reconcile(2_176),
            Err(BrokerError::ReconciliationLimit)
        ));

        let retained = filler
            .reserve(
                Lane::Normal {
                    completion_reserve: 1_024,
                },
                6_144,
            )
            .unwrap()
            .reconcile(7_168)
            .unwrap();
        let blocked = normal
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1,
            )
            .unwrap();
        assert_eq!(broker.snapshot().aggregate_bytes, 8_192);

        let exact_o = oversize.reserve(Lane::Oversize, 4_096).unwrap();
        let snapshot = broker.snapshot();
        assert_eq!(snapshot.aggregate_bytes, 12_288);
        assert_eq!(snapshot.oversize_bytes, 4_096);
        assert_eq!(snapshot.oversize_owners, 1);
        drop(exact_o);
        blocked.cancel();
        assert!(matches!(blocked.wait(), Err(BrokerError::Cancelled)));
        drop(retained);
        normal.close();
        filler.close();
        oversize.close();
        assert_drained(&broker);
    }

    #[test]
    fn retained_ownership_and_per_operation_diagnostics_are_exact() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let mut retained = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 128,
                },
                512,
            )
            .unwrap()
            .reconcile(600)
            .unwrap();
        let operation_snapshot = broker.snapshot().operations[&operation.id()].clone();
        assert_eq!(operation_snapshot.queued, 0);
        assert_eq!(operation_snapshot.in_flight, 0);
        assert_eq!(operation_snapshot.grants, 1);
        assert_eq!(operation_snapshot.granted_bytes, 512);
        assert_eq!(broker.snapshot().bypass_bytes, 600);

        retained.transition(OwnershipClass::Cache).unwrap();
        assert_eq!(broker.snapshot().cache_bytes, 600);
        retained.transition(OwnershipClass::Pin).unwrap();
        let pinned = broker.snapshot();
        assert_eq!(pinned.pin_bytes, 600);
        assert_eq!(pinned.peak_bypass_bytes, 600);
        assert_eq!(pinned.peak_cache_bytes, 600);
        assert_eq!(pinned.peak_pin_bytes, 600);
        assert_eq!(pinned.operations[&operation.id()].pin_bytes, 600);
        operation.close();
        assert_eq!(broker.snapshot().active_operations, 1);
        drop(retained);
        assert_drained(&broker);
    }

    #[test]
    fn zero_estimate_flight_retains_charged_request_metadata() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let reservation = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                0,
            )
            .unwrap();
        let snapshot = broker.snapshot();
        assert_eq!(snapshot.metadata_bytes, 512);
        assert_eq!(snapshot.in_flight, 1);
        assert_eq!(snapshot.reservation_metadata_bytes, 256);
        assert_eq!(reservation.request_metadata_bytes(), 256);
        drop(reservation);
        assert_eq!(broker.snapshot().metadata_bytes, 256);
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn exact_o_head_preserves_fifo_progress_and_owns_live_request_metadata() {
        let broker = broker();
        let first_operation = broker.register_oversize_operation(512).unwrap();
        let exact_operation = broker.register_oversize_operation(2_048).unwrap();
        let later_operation = broker.register_oversize_operation(0).unwrap();
        let first = first_operation.reserve(Lane::Oversize, 1_024).unwrap();
        let exact = exact_operation.request(Lane::Oversize, 4_096).unwrap();
        assert_eq!(broker.snapshot().oversize_bytes, 1_280);
        assert!(matches!(
            later_operation.request(Lane::Oversize, 1),
            Err(BrokerError::ResourceLimit)
        ));
        drop(first);
        let exact = exact.wait().unwrap();
        assert_eq!(exact.request_metadata_bytes(), 256);
        let snapshot = broker.snapshot();
        assert_eq!(snapshot.oversize_bytes, 4_096);
        assert_eq!(snapshot.reservation_metadata_bytes, 256);
        drop(exact);
        first_operation.close();
        exact_operation.close();
        later_operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn retained_owner_outlives_its_handle_until_the_linearized_pin_drops() {
        let broker = broker();
        let operation = broker.register_operation().unwrap();
        let first = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                100,
            )
            .unwrap()
            .reconcile(100)
            .unwrap();
        let second = operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                100,
            )
            .unwrap()
            .reconcile(100)
            .unwrap();
        let pin = first.pin(&operation, 100).unwrap();
        assert!(matches!(
            first.pin(&operation, 100),
            Err(BrokerError::ResourceLimit)
        ));
        drop(first);
        drop(second);
        let held = broker.snapshot();
        assert_eq!(held.normal_payload_bytes, 100);
        assert_eq!(held.operations[&operation.id()].self_pinned_bytes, 100);
        drop(pin);
        assert_eq!(broker.snapshot().normal_payload_bytes, 0);
        operation.close();
        assert_drained(&broker);
    }

    #[test]
    fn close_reconcile_race_has_one_locked_winner_and_never_publishes_after_close() {
        for _ in 0..128 {
            let broker = broker();
            let operation = broker.register_operation().unwrap();
            let reservation = operation
                .reserve(
                    Lane::Normal {
                        completion_reserve: 128,
                    },
                    512,
                )
                .unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let thread_barrier = Arc::clone(&barrier);
            let reconciler = thread::spawn(move || {
                thread_barrier.wait();
                reservation.reconcile(512)
            });
            barrier.wait();
            operation.close();
            match reconciler.join().unwrap() {
                Ok(retained) => drop(retained),
                Err(BrokerError::OperationClosed) => {}
                Err(error) => panic!("unexpected close/reconcile winner: {error:?}"),
            }
            assert_drained(&broker);
        }
    }

    #[test]
    fn cancellation_counter_overflow_fails_before_queue_mutation_and_wakes_every_waiter() {
        let broker = broker();
        let blocker_operation = broker.register_operation().unwrap();
        let first_operation = broker.register_operation().unwrap();
        let second_operation = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let first = first_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_024,
            )
            .unwrap();
        let second = second_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_024,
            )
            .unwrap();
        broker.inner.lock().cancellations = u64::MAX;
        first.cancel();
        let retained_errors = broker.snapshot();
        assert!(retained_errors.closed);
        assert!(retained_errors.invariant_failed);
        assert_eq!(retained_errors.queued, 0);
        assert_eq!(retained_errors.live_request_records, 3);
        assert_eq!(retained_errors.error_metadata_bytes, 512);
        assert!(matches!(first.wait(), Err(BrokerError::ArithmeticOverflow)));
        assert_eq!(broker.snapshot().error_metadata_bytes, 256);
        assert!(matches!(
            second.wait(),
            Err(BrokerError::ArithmeticOverflow)
        ));
        let failed = broker.snapshot();
        assert!(failed.closed);
        assert!(failed.invariant_failed);
        assert_eq!(failed.queued, 0);
        drop(blocker);
        blocker_operation.close();
        first_operation.close();
        second_operation.close();
        let drained = broker.snapshot();
        assert_eq!(drained.aggregate_bytes, 0);
        assert_eq!(drained.active_operations, 0);
    }

    #[test]
    fn denial_and_reconciliation_overflow_close_and_drain_without_orphans() {
        let denial_broker = broker();
        let blocker_operation = denial_broker.register_operation().unwrap();
        let waiting_operation = denial_broker.register_operation().unwrap();
        let capped_operation = denial_broker.register_oversize_operation(100).unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let waiting = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_024,
            )
            .unwrap();
        denial_broker.inner.lock().denials = u64::MAX;
        assert!(matches!(
            capped_operation.request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                101,
            ),
            Err(BrokerError::ArithmeticOverflow)
        ));
        assert!(matches!(
            waiting.wait(),
            Err(BrokerError::ArithmeticOverflow)
        ));
        drop(blocker);
        blocker_operation.close();
        waiting_operation.close();
        capped_operation.close();
        assert_eq!(denial_broker.snapshot().aggregate_bytes, 0);

        let broker = broker();
        let loading_operation = broker.register_operation().unwrap();
        let waiting_operation = broker.register_operation().unwrap();
        let loading = loading_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                6_000,
            )
            .unwrap();
        let waiting = waiting_operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_024,
            )
            .unwrap();
        broker.inner.lock().reconciliations = u64::MAX;
        assert!(matches!(
            loading.reconcile(6_000),
            Err(BrokerError::ArithmeticOverflow)
        ));
        assert!(matches!(
            waiting.wait(),
            Err(BrokerError::ArithmeticOverflow)
        ));
        loading_operation.close();
        waiting_operation.close();
        assert_eq!(broker.snapshot().aggregate_bytes, 0);
    }

    #[test]
    fn close_enqueue_race_has_one_typed_winner_and_no_orphan_charge() {
        for _ in 0..128 {
            let broker = broker();
            let operation = broker.register_operation().unwrap();
            let contender = operation.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let thread_barrier = Arc::clone(&barrier);
            let requester = thread::spawn(move || {
                thread_barrier.wait();
                contender.request(
                    Lane::Normal {
                        completion_reserve: 0,
                    },
                    0,
                )
            });
            barrier.wait();
            operation.close();
            match requester.join().unwrap() {
                Ok(pending) => match pending.wait() {
                    Ok(reservation) => drop(reservation),
                    Err(BrokerError::OperationClosed) => {}
                    Err(error) => panic!("unexpected close/enqueue winner: {error:?}"),
                },
                Err(BrokerError::OperationClosed) => {}
                Err(error) => panic!("unexpected close/enqueue rejection: {error:?}"),
            }
            assert_drained(&broker);
        }
    }

    #[test]
    fn cancel_grant_race_has_one_typed_winner_and_no_orphan_charge() {
        for _ in 0..128 {
            let broker = broker();
            let blocker_operation = broker.register_operation().unwrap();
            let operation = broker.register_operation().unwrap();
            let blocker = blocker_operation
                .reserve(
                    Lane::Normal {
                        completion_reserve: 0,
                    },
                    6_000,
                )
                .unwrap();
            let pending = operation
                .request(
                    Lane::Normal {
                        completion_reserve: 0,
                    },
                    1_024,
                )
                .unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let thread_barrier = Arc::clone(&barrier);
            let canceller = thread::spawn(move || {
                thread_barrier.wait();
                pending.cancel();
                pending.wait()
            });
            barrier.wait();
            drop(blocker);
            match canceller.join().unwrap() {
                Ok(reservation) => drop(reservation),
                Err(BrokerError::Cancelled) => {}
                Err(error) => panic!("unexpected cancel/grant winner: {error:?}"),
            }
            blocker_operation.close();
            operation.close();
            assert_drained(&broker);
        }
    }

    #[test]
    fn unequal_temporarily_inadmissible_and_late_operations_keep_deterministic_rounds() {
        let broker = BudgetBroker::new(BrokerConfig {
            normal_limit: 16_384,
            oversize_limit: 8_192,
            completion_reserve_limit: 2_048,
            queue_metadata_weight: 256,
            operation_metadata_weight: 256,
            max_active_operations: 8,
            max_queued_requests: 32,
        })
        .unwrap();
        let blocker_operation = broker.register_operation().unwrap();
        let small = broker.register_operation().unwrap();
        let large = broker.register_operation().unwrap();
        let blocker = blocker_operation
            .reserve(
                Lane::Normal {
                    completion_reserve: 0,
                },
                10_000,
            )
            .unwrap();
        let small_first = small
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_000,
            )
            .unwrap();
        let large_first = large
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                4_000,
            )
            .unwrap();
        let small_second = small
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                1_000,
            )
            .unwrap();
        drop(blocker);
        let small_first = small_first.wait().unwrap();
        let small_ordinal = small_first.grant_ordinal();
        drop(small_first.reconcile(0).unwrap());
        let large_first = large_first.wait().unwrap();
        assert!(large_first.grant_ordinal() > small_ordinal);
        drop(large_first.reconcile(0).unwrap());

        let late = broker.register_operation().unwrap();
        let late_request = late
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                500,
            )
            .unwrap();
        let small_second = small_second.wait().unwrap();
        let late_request = late_request.wait().unwrap();
        assert!(late_request.grant_ordinal() > small_second.grant_ordinal());
        drop(small_second.reconcile(0).unwrap());
        drop(late_request.reconcile(0).unwrap());
        assert!(broker.snapshot().maximum_admissible_lag <= 1);
        blocker_operation.close();
        small.close();
        large.close();
        late.close();
        assert_drained(&broker);
    }

    #[test]
    fn identifier_and_grant_counter_overflow_fail_before_work() {
        let broker = broker();
        {
            let mut state = broker.inner.lock();
            state.next_operation_id = u64::MAX;
        }
        assert!(matches!(
            broker.register_operation(),
            Err(BrokerError::ArithmeticOverflow)
        ));
        {
            let mut state = broker.inner.lock();
            state.next_operation_id = 0;
        }
        let operation = broker.register_operation().unwrap();
        {
            let mut state = broker.inner.lock();
            state.next_request_id = u64::MAX;
        }
        assert!(matches!(
            operation.request(
                Lane::Normal {
                    completion_reserve: 0
                },
                0
            ),
            Err(BrokerError::ArithmeticOverflow)
        ));
        {
            let mut state = broker.inner.lock();
            state.next_request_id = 0;
            state.next_grant_ordinal = u64::MAX;
        }
        let pending = operation
            .request(
                Lane::Normal {
                    completion_reserve: 0,
                },
                0,
            )
            .unwrap();
        assert!(matches!(
            pending.wait(),
            Err(BrokerError::ArithmeticOverflow)
        ));
        operation.close();
        assert_drained(&broker);
    }
}
