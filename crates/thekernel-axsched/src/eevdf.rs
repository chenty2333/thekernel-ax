//! EEVDF task representation and single-runqueue scheduler core.
//!
//! The task boundary owns the atomically published scheduling tuple,
//! intrusive node, and scheduler-owned state slot.  [`EEVDFScheduler`] adds
//! the one-tree ready path and lifecycle policy around that representation.

use core::{
    cell::UnsafeCell,
    fmt,
    ops::Deref,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use crate::{
    allocate_scheduler_id,
    cfs::{RR_TIMESLICE_TICKS, RT_PRIORITY_MAX, RT_PRIORITY_MIN},
    eevdf_model::{bias_i128, credit_cap, Clock, Entity, MigrationSnapshot, ModelError, ONE},
    eevdf_tree::{EevdfNode, EevdfTree, EevdfTreeError},
    BaseScheduler, DeactivateReason, EnqueueReason, RuntimeDelta, SchedulerError, CONFIGURING,
    UNOWNED,
};

use alloc::sync::Arc;

const NICE_RANGE_POS: i8 = 19;
const NICE_MIN: i8 = -20;
const NICE_MAX: i8 = 19;

// These are the Linux fair-class weights used by CFS as well.  Keeping the
// conversion local means the task representation does not depend on CFS's
// private implementation details or on a scheduler core.
const NICE2WEIGHT_POS: [u128; 20] = [
    1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
];
const NICE2WEIGHT_NEG: [u128; 21] = [
    1024, 1277, 1586, 1991, 2501, 3121, 3906, 4904, 6100, 7620, 9548, 11916, 14949, 18705, 23254,
    29154, 36291, 46273, 56483, 71755, 88761,
];

/// Runtime scheduling class represented by an EEVDF task.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EevdfTaskClass {
    /// Ordinary fair scheduling.
    Normal = 0,
    /// Fair scheduling biased toward throughput over latency.
    Batch = 1,
    /// Lowest-precedence fair scheduling.
    Idle = 2,
    /// Fixed-priority, time-sliced scheduling represented by the task layer.
    RoundRobin = 3,
    /// Fixed-priority scheduling without time slicing represented by the task
    /// layer.
    Fifo = 4,
}

/// Runtime scheduling parameters for an EEVDF task.
///
/// The tuple intentionally has the same layout and class-specific
/// canonicalization rules as [`crate::CfsTaskParams`].  The EEVDF scheduler
/// core may later reject non-fair classes at admission, but the task boundary
/// must preserve the generic nice and real-time domains consistently.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct EevdfTaskParams {
    /// Scheduling class.
    pub class: EevdfTaskClass,
    /// Fair-class weight selector in the inclusive range `-20..=19`.
    pub nice: i8,
    /// Real-time priority in the inclusive range
    /// [`RT_PRIORITY_MIN`]`..=`[`RT_PRIORITY_MAX`].
    pub rt_priority: u8,
}

impl Default for EevdfTaskParams {
    fn default() -> Self {
        Self {
            class: EevdfTaskClass::Normal,
            nice: 0,
            rt_priority: 0,
        }
    }
}

impl EevdfTaskParams {
    fn validated(mut self) -> Option<Self> {
        match self.class {
            EevdfTaskClass::Idle => {
                self.nice = NICE_RANGE_POS;
                self.rt_priority = 0;
            }
            EevdfTaskClass::Normal | EevdfTaskClass::Batch => {
                self.rt_priority = 0;
            }
            EevdfTaskClass::RoundRobin | EevdfTaskClass::Fifo => {
                self.nice = 0;
            }
        }
        let valid = match self.class {
            EevdfTaskClass::RoundRobin | EevdfTaskClass::Fifo => {
                (RT_PRIORITY_MIN..=RT_PRIORITY_MAX).contains(&self.rt_priority)
            }
            EevdfTaskClass::Normal | EevdfTaskClass::Batch | EevdfTaskClass::Idle => {
                (NICE_MIN..=NICE_MAX).contains(&self.nice)
            }
        };
        valid.then_some(self)
    }

    const fn packed(self) -> u32 {
        (self.class as u32) | ((self.nice as u8 as u32) << 8) | ((self.rt_priority as u32) << 16)
    }

    fn from_packed(value: u32) -> Self {
        let class = match value as u8 {
            0 => EevdfTaskClass::Normal,
            1 => EevdfTaskClass::Batch,
            2 => EevdfTaskClass::Idle,
            3 => EevdfTaskClass::RoundRobin,
            4 => EevdfTaskClass::Fifo,
            // The publication word is private and every safe writer stores
            // a validated tuple.  Contain a corrupted word by returning the
            // canonical default instead of indexing a weight table out of
            // bounds.
            _ => EevdfTaskClass::Normal,
        };
        Self {
            class,
            nice: ((value >> 8) as u8) as i8,
            rt_priority: (value >> 16) as u8,
        }
        .validated()
        .unwrap_or_default()
    }
}

/// Result of a successful runtime parameter update.
///
/// The update itself is committed before this value is produced.  A
/// `PreemptCurrent` result asks the caller to reschedule the returned current
/// task; it does not mutate the scheduler or consume a tick/budget.
#[must_use]
pub enum EevdfParamUpdate<T> {
    /// The post-update ready set does not outrank the current task.
    NoPreemption,
    /// The current task should be preempted after this update.
    PreemptCurrent(Arc<EEVDFTask<T>>),
}

impl<T> EevdfParamUpdate<T> {
    /// Return the current task that should be preempted, if any.
    pub fn preempt_current(&self) -> Option<&Arc<EEVDFTask<T>>> {
        match self {
            Self::NoPreemption => None,
            Self::PreemptCurrent(task) => Some(task),
        }
    }

    /// Consume the outcome and return the current task that should be
    /// preempted, if any.
    pub fn into_preempt_current(self) -> Option<Arc<EEVDFTask<T>>> {
        match self {
            Self::NoPreemption => None,
            Self::PreemptCurrent(task) => Some(task),
        }
    }

    /// Whether the update requires a reschedule of the current task.
    pub const fn requests_preemption(&self) -> bool {
        matches!(self, Self::PreemptCurrent(_))
    }
}

impl<T> core::fmt::Debug for EevdfParamUpdate<T> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoPreemption => formatter.write_str("NoPreemption"),
            Self::PreemptCurrent(_) => formatter.write_str("PreemptCurrent(..)"),
        }
    }
}

impl<T> PartialEq for EevdfParamUpdate<T> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::NoPreemption, Self::NoPreemption) => true,
            (Self::PreemptCurrent(left), Self::PreemptCurrent(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl<T> Eq for EevdfParamUpdate<T> {}

/// Convert a fair nice value to its local scheduler weight table.
///
/// Invalid values are rejected instead of being clamped or indexing outside
/// the table.  Callers that have already validated a parameter tuple should
/// propagate or explicitly handle the `None` case at their boundary.
pub(crate) fn eevdf_weight_for_nice(nice: i8) -> Option<u128> {
    if !(NICE_MIN..=NICE_MAX).contains(&nice) {
        return None;
    }

    if nice >= 0 {
        NICE2WEIGHT_POS.get(nice as usize).copied()
    } else {
        // The range check above excludes i8::MIN, so negation cannot overflow.
        NICE2WEIGHT_NEG.get(nice.checked_neg()? as usize).copied()
    }
}

/// Return the fair weight represented by a validated parameter tuple.
pub(crate) fn eevdf_weight_for(params: EevdfTaskParams) -> Option<u128> {
    let params = params.validated()?;
    match params.class {
        EevdfTaskClass::Idle => eevdf_weight_for_nice(NICE_RANGE_POS),
        EevdfTaskClass::Normal | EevdfTaskClass::Batch => eevdf_weight_for_nice(params.nice),
        EevdfTaskClass::RoundRobin | EevdfTaskClass::Fifo => eevdf_weight_for_nice(0),
    }
}

/// Stable ordering identity for a ready EEVDF task.
///
/// The fields are private so a scheduler cannot mutate an ordering key while
/// the corresponding node is linked.  A run-queue owner stages a fresh key
/// through the crate-private helper after removing the node.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct EevdfReadyKey {
    class_rank: u8,
    order: u128,
    sequence: i128,
}

/// Opaque continuation state for a bounded ready-tree query.
///
/// Idle stealing keeps one value per source CPU.  Keeping the structural key
/// inside the scheduler crate means the task layer cannot accidentally build a
/// cursor that does not obey the tree's total ordering, while still allowing
/// it to retain progress between bounded, try-lock-only attempts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct EevdfReadyCursor(Option<EevdfReadyKey>);

impl EevdfReadyCursor {
    /// Construct a cursor positioned before the first key.
    pub const fn new() -> Self {
        Self(None)
    }

    /// Forget continuation state and restart at the first ready key.
    pub fn reset(&mut self) {
        self.0 = None;
    }
}

/// A ready task selected by a bounded, read-only run-queue query.
///
/// The task remains linked in its source scheduler.  The candidate owns an
/// additional Arc reference so the caller can, while retaining the same
/// run-queue lock, pass [`Self::task`] to [`EEVDFScheduler::begin_ready_migration`]
/// or discard it and leave the queue untouched.
pub struct EevdfReadyCandidate<T> {
    task: Arc<EEVDFTask<T>>,
    key: EevdfReadyKey,
    class: EevdfTaskClass,
    visited: usize,
}

impl<T> EevdfReadyCandidate<T> {
    /// Return the selected task without detaching it from the source queue.
    pub fn task(&self) -> &Arc<EEVDFTask<T>> {
        &self.task
    }

    /// Consume the candidate and return its retained task reference.
    pub fn into_task(self) -> Arc<EEVDFTask<T>> {
        self.task
    }

    /// Number of key-ordered nodes whose predicate was evaluated.
    pub const fn visited(&self) -> usize {
        self.visited
    }

    /// Structural key observed while the source run queue was excluded.
    pub const fn key(&self) -> EevdfReadyKey {
        self.key
    }

    /// Scheduling class observed at the same query boundary.
    pub const fn class(&self) -> EevdfTaskClass {
        self.class
    }
}

/// Opaque fair-state seed for a newly forked task.
///
/// A seed contains only a bounded, materialized lag.  It never contains the
/// parent's active request, so admission always gives the child a fresh
/// request and quantum.  Values can only be produced by
/// [`EEVDFScheduler::fork_seed`].
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EevdfForkSeed {
    class: crate::eevdf_model::RequestClass,
    lag: i128,
}

impl EevdfReadyKey {
    pub(crate) const fn new(class_rank: u8, order: u128, sequence: i128) -> Self {
        Self {
            class_rank,
            order,
            sequence,
        }
    }

    pub(crate) const fn class_rank(self) -> u8 {
        self.class_rank
    }

    pub(crate) const fn order(self) -> u128 {
        self.order
    }

    pub(crate) const fn sequence(self) -> i128 {
        self.sequence
    }
}

/// Task payload stored in the intrusive node.
///
/// The state slot is intentionally crate-private.  It gives the future EEVDF
/// core a typed place for entity and migration state without exposing those
/// internals as part of the public task API.
#[doc(hidden)]
pub struct EevdfTaskPayload<T> {
    inner: T,
    params: AtomicU32,
    queue_owner: AtomicUsize,
    rr_remaining: AtomicUsize,
    state: UnsafeCell<EevdfOwnedState>,
}

/// Whether a migration was detached from a ready task or from the current
/// task.  Both origins are published as ready tasks when they are committed
/// or rolled back; the origin is retained only for source-aware recovery and
/// diagnostics.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EevdfMigrationOrigin {
    /// The task was linked in the source scheduler's ready tree.
    Ready,
    /// The task was the source scheduler's current task.
    Running,
}

/// Complete source-side metadata for one in-flight migration.
///
/// This is deliberately a single task-owned value.  The public migration
/// token is only a capability for the task and does not copy any of this
/// rollback state, so a parked task cannot retain two disagreeing snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MigrationState {
    source_scheduler_id: usize,
    params: EevdfTaskParams,
    snapshot: Option<MigrationSnapshot>,
    /// Exact source clock before detaching this task.
    source_clock: Clock,
    /// Exact source clock after detaching this task.
    detached_clock: Clock,
    /// Exact source entity before detaching this task.  When the source clock
    /// has not advanced, rollback can restore this value bit-for-bit instead
    /// of reconstructing it from the migration snapshot.
    source_entity: Option<Entity>,
    source_key: EevdfReadyKey,
    source_eligible_at: u128,
    rr_remaining: usize,
    /// Runtime below one scheduler period retained across CPU migration.
    runtime_remainder_ns: u64,
    /// Conversion residue from Q32 fractional service settlement.  It is
    /// bounded by `runtime_fraction_period_ns` and is not a deferred wall
    /// runtime token or a scheduler clock snapshot.
    runtime_fraction_remainder_ns: u64,
    runtime_fraction_period_ns: u64,
    runtime_fraction_class: EevdfTaskClass,
    runtime_fraction_weight: u128,
    rr_fraction_q: u64,
    dormant_fair: Option<Entity>,
    origin: EevdfMigrationOrigin,
}

/// Scheduler-owned fair state.  Access requires the queue-owner/rq-lock
/// contract described by [`EevdfTaskPayload::owned_state_mut`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EevdfOwnedState {
    pub(crate) entity: Option<Entity>,
    /// Runtime below one scheduler period retained for the current task.
    ///
    /// The task layer supplies elapsed nanoseconds at every ownership
    /// boundary. Keeping this remainder with the task, rather than with a
    /// run queue, prevents a yield or migration at half a period from losing
    /// service or charging the next task.
    pub(crate) runtime_remainder_ns: u64,
    pub(crate) runtime_fraction_remainder_ns: u64,
    pub(crate) runtime_fraction_period_ns: u64,
    pub(crate) runtime_fraction_class: EevdfTaskClass,
    pub(crate) runtime_fraction_weight: u128,
    pub(crate) rr_fraction_q: u64,
    /// A task has either no migration or exactly one complete migration
    /// record.  `Some` is also the lifecycle marker while owner is UNOWNED.
    migration: Option<MigrationState>,
    /// Fair state retained while a task is temporarily in an RT class.
    pub(crate) dormant_fair: Option<Entity>,
    /// A sleeping RT task has no fair `Entity`; retain an explicit lifecycle
    /// bit so it cannot be mistaken for a virgin task after owner release.
    pub(crate) rt_sleeping: bool,
    /// Optional fork seed retained while the task remains virgin.
    pub(crate) fork_seed: Option<EevdfForkSeed>,
    /// Explicit RT sleep anchor; fair entities retain the same anchor in the
    /// model itself, while RT tasks have no Entity to carry it.
    pub(crate) sleep_v: Option<i128>,
}

impl<T> EevdfTaskPayload<T> {
    pub(crate) const fn new(inner: T) -> Self {
        Self {
            inner,
            params: AtomicU32::new(
                EevdfTaskParams {
                    class: EevdfTaskClass::Normal,
                    nice: 0,
                    rt_priority: 0,
                }
                .packed(),
            ),
            queue_owner: AtomicUsize::new(UNOWNED),
            rr_remaining: AtomicUsize::new(0),
            state: UnsafeCell::new(EevdfOwnedState {
                entity: None,
                runtime_remainder_ns: 0,
                runtime_fraction_remainder_ns: 0,
                runtime_fraction_period_ns: 0,
                runtime_fraction_class: EevdfTaskClass::Normal,
                runtime_fraction_weight: 0,
                rr_fraction_q: 0,
                migration: None,
                dormant_fair: None,
                rt_sleeping: false,
                fork_seed: None,
                sleep_v: None,
            }),
        }
    }

    pub(crate) const fn inner(&self) -> &T {
        &self.inner
    }

    fn load_sched_params(&self) -> EevdfTaskParams {
        EevdfTaskParams::from_packed(self.params.load(Ordering::Acquire))
    }

    fn apply_validated(&self, params: EevdfTaskParams) {
        let rr_remaining = match params.class {
            EevdfTaskClass::RoundRobin => RR_TIMESLICE_TICKS,
            EevdfTaskClass::Fifo
            | EevdfTaskClass::Normal
            | EevdfTaskClass::Batch
            | EevdfTaskClass::Idle => 0,
        };
        self.rr_remaining.store(rr_remaining, Ordering::Release);
        self.params.store(params.packed(), Ordering::Release);
    }

    fn publish_validated(&self, params: EevdfTaskParams, rr_remaining: usize) {
        self.rr_remaining.store(rr_remaining, Ordering::Release);
        self.params.store(params.packed(), Ordering::Release);
    }

    pub(crate) fn claim(&self, owner: usize) -> Result<(), SchedulerError> {
        if owner == UNOWNED || owner == CONFIGURING {
            return Err(SchedulerError::InconsistentState);
        }
        self.queue_owner
            .compare_exchange(UNOWNED, owner, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|current| match current {
                current if current == owner => SchedulerError::AlreadyQueued,
                CONFIGURING => SchedulerError::TaskBusy,
                _ => SchedulerError::ForeignQueue,
            })
    }

    pub(crate) fn transfer_owner(&self, from: usize, to: usize) -> Result<(), SchedulerError> {
        self.queue_owner
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| SchedulerError::InconsistentState)
    }

    pub(crate) fn claim_migration(&self) -> Result<(), SchedulerError> {
        self.queue_owner
            .compare_exchange(UNOWNED, CONFIGURING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|owner| match owner {
                CONFIGURING => SchedulerError::TaskBusy,
                _ => SchedulerError::AlreadyQueued,
            })?;
        // SAFETY: CONFIGURING is the exclusive state-owner claim.
        if !unsafe { self.owned_state().migration.is_some() } {
            self.transfer_owner(CONFIGURING, UNOWNED)
                .expect("EEVDF migration claim owner changed while releasing claim");
            return Err(SchedulerError::InconsistentState);
        }
        Ok(())
    }

    pub(crate) fn owner(&self) -> usize {
        self.queue_owner.load(Ordering::Acquire)
    }

    pub(crate) fn claim_configuration(
        &self,
    ) -> Result<EevdfConfigurationClaim<'_, T>, SchedulerError> {
        self.queue_owner
            .compare_exchange(UNOWNED, CONFIGURING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|owner| match owner {
                CONFIGURING => SchedulerError::TaskBusy,
                _ => SchedulerError::AlreadyQueued,
            })?;

        // A future scheduler owns entity/migration publication under its run
        // queue lock.  Configuration is intentionally restricted to the
        // virgin representation so it cannot race or rewrite live model
        // state behind that core's back.
        // SAFETY: the successful CONFIGURING claim above is the exclusive
        // owner exclusion required by `is_virgin`; no other state access can
        // occur until this claim is released.
        if !unsafe { self.is_virgin() } {
            self.transfer_owner(CONFIGURING, UNOWNED)
                .expect("EEVDF configuration claim owner changed while releasing claim");
            return Err(SchedulerError::AlreadyQueued);
        }
        Ok(EevdfConfigurationClaim {
            task: self,
            active: true,
        })
    }

    pub(crate) fn claim_reservation(&self) -> Result<(), SchedulerError> {
        self.queue_owner
            .compare_exchange(UNOWNED, CONFIGURING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|owner| match owner {
                CONFIGURING => SchedulerError::TaskBusy,
                _ => SchedulerError::AlreadyQueued,
            })?;
        // SAFETY: the successful claim is the exclusive state-owner claim.
        if !unsafe { self.is_virgin() } {
            self.transfer_owner(CONFIGURING, UNOWNED)
                .expect("EEVDF reservation claim owner changed while releasing claim");
            return Err(SchedulerError::AlreadyQueued);
        }
        Ok(())
    }

    pub(crate) fn claim_reconfiguration(
        &self,
    ) -> Result<EevdfConfigurationClaim<'_, T>, SchedulerError> {
        self.queue_owner
            .compare_exchange(UNOWNED, CONFIGURING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|owner| match owner {
                CONFIGURING => SchedulerError::TaskBusy,
                _ => SchedulerError::AlreadyQueued,
            })?;
        Ok(EevdfConfigurationClaim {
            task: self,
            active: true,
        })
    }

    /// Return whether the scheduler-owned state is still uninitialized.
    ///
    /// # Safety
    ///
    /// The caller must hold this task's `CONFIGURING` queue-owner claim or
    /// an equivalent exclusive run-queue ownership/lock for the complete
    /// duration of the call.  That exclusion must cover every read and write
    /// of the state slot, because this helper reads the `UnsafeCell` directly.
    pub(crate) unsafe fn is_virgin(&self) -> bool {
        unsafe {
            let state = &*self.state.get();
            state.entity.is_none()
                && state.migration.is_none()
                && state.dormant_fair.is_none()
                && !state.rt_sleeping
        }
    }

    /// Borrow scheduler-owned state under the run-queue lock.
    ///
    /// # Safety
    ///
    /// The caller must hold the task's queue-owner claim and the run-queue
    /// lock (or the equivalent single-owner exclusion) for the entire borrow.
    pub(crate) unsafe fn owned_state(&self) -> &EevdfOwnedState {
        &*self.state.get()
    }

    /// Mutably borrow scheduler-owned state under the run-queue lock.
    ///
    /// # Safety
    ///
    /// The caller must hold the task's queue-owner claim and the run-queue
    /// lock, and must not expose this reference across that exclusion.
    pub(crate) unsafe fn owned_state_mut(&self) -> &mut EevdfOwnedState {
        &mut *self.state.get()
    }

    pub(crate) fn rr_remaining(&self) -> usize {
        self.rr_remaining.load(Ordering::Acquire)
    }

    pub(crate) fn set_rr_remaining(&self, remaining: usize) {
        self.rr_remaining.store(remaining, Ordering::Release);
    }

    pub(crate) fn install_fork_seed(&self, seed: EevdfForkSeed) -> Result<(), SchedulerError> {
        let claim = self.claim_configuration()?;
        // SAFETY: the configuration claim excludes every state access.
        unsafe {
            let state = self.owned_state_mut();
            if state.fork_seed.is_some() {
                return Err(SchedulerError::AlreadyQueued);
            }
            state.fork_seed = Some(seed);
        }
        claim.finish()
    }
}

// `UnsafeCell<EevdfOwnedState>` prevents these auto-traits.  The task payload
// is nevertheless movable/shared when T is movable/shared: parameter and
// queue-owner publication uses atomics, while mutable entity/migration state
// is only accessed by a claimed owner under the run-queue lock.  The only
// state helpers are unsafe and carry that exclusion requirement, so no safe
// API can race an `UnsafeCell` access.  Requiring `T: Send + Sync` covers the
// caller-owned payload exposed through `inner`; the intrusive node adds only
// its own structural tree proof.
unsafe impl<T: Send + Sync> Send for EevdfTaskPayload<T> {}
unsafe impl<T: Send + Sync> Sync for EevdfTaskPayload<T> {}

/// An allocation-free intrusive EEVDF task node.
pub type EEVDFTask<T> = EevdfNode<EevdfReadyKey, EevdfTaskPayload<T>>;

pub(crate) struct EevdfConfigurationClaim<'a, T> {
    task: &'a EevdfTaskPayload<T>,
    active: bool,
}

impl<T> EevdfConfigurationClaim<'_, T> {
    fn finish(mut self) -> Result<(), SchedulerError> {
        self.task.transfer_owner(CONFIGURING, UNOWNED)?;
        self.active = false;
        Ok(())
    }
}

impl<T> Drop for EevdfConfigurationClaim<'_, T> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.task.transfer_owner(CONFIGURING, UNOWNED);
        }
    }
}

impl<T> EevdfNode<EevdfReadyKey, EevdfTaskPayload<T>> {
    /// Construct an unlinked EEVDF task with default scheduling parameters.
    pub const fn new(inner: T) -> Self {
        EevdfNode::new_unlinked(EevdfReadyKey::new(1, 0, 0), 0, EevdfTaskPayload::new(inner))
    }

    /// Return the caller-owned task value.
    pub fn inner(&self) -> &T {
        self.value().inner()
    }

    /// Consume an unlinked task and return its caller-owned value.
    pub fn into_inner(self) -> T {
        self.into_value().inner
    }

    /// Return the current atomically published scheduling parameters.
    pub fn sched_params(&self) -> EevdfTaskParams {
        self.value().load_sched_params()
    }

    pub(crate) fn rr_remaining(&self) -> usize {
        self.value().rr_remaining()
    }

    pub(crate) fn set_rr_remaining(&self, remaining: usize) {
        self.value().set_rr_remaining(remaining)
    }

    /// Configure an unowned, virgin task.
    pub fn configure(&self, params: EevdfTaskParams) -> Result<(), SchedulerError> {
        let params = params
            .validated()
            .ok_or(SchedulerError::InvalidParameters)?;
        let claim = self.value().claim_configuration()?;
        self.value().apply_validated(params);
        claim.finish()
    }

    /// Install a scheduler-produced fork seed on a still-virgin child.
    pub fn install_fork_seed(&self, seed: EevdfForkSeed) -> Result<(), SchedulerError> {
        self.value().install_fork_seed(seed)
    }

    pub(crate) fn claim(&self, owner: usize) -> Result<(), SchedulerError> {
        self.value().claim(owner)
    }

    pub(crate) fn transfer_owner(&self, from: usize, to: usize) -> Result<(), SchedulerError> {
        self.value().transfer_owner(from, to)
    }

    pub(crate) fn owner(&self) -> usize {
        self.value().owner()
    }

    /// Return whether the scheduler-owned state is still uninitialized.
    ///
    /// # Safety
    ///
    /// The caller must hold this task's `CONFIGURING` queue-owner claim or
    /// an equivalent exclusive run-queue ownership/lock for the complete
    /// duration of the call.  This requirement is forwarded to the payload
    /// helper because the state slot is stored in an `UnsafeCell`.
    pub(crate) unsafe fn is_virgin(&self) -> bool {
        unsafe { self.value().is_virgin() }
    }

    pub(crate) unsafe fn owned_state(&self) -> &EevdfOwnedState {
        self.value().owned_state()
    }

    pub(crate) unsafe fn owned_state_mut(&self) -> &mut EevdfOwnedState {
        self.value().owned_state_mut()
    }
}

impl<T> Deref for EEVDFTask<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner()
    }
}

/// One unpublished EEVDF ready-queue admission.
///
/// The token owns a CONFIGURING claim and one already-reserved ordering
/// sequence.  It contains no heap allocation and its sequence is never
/// reacquired during commit.
#[must_use = "dropping the reservation cancels runnable-task publication"]
pub struct EevdfTaskReservation<T> {
    task: Option<Arc<EEVDFTask<T>>>,
    scheduler_id: usize,
    sequence: i128,
    params: EevdfTaskParams,
}

impl<T> EevdfTaskReservation<T> {
    pub fn task(&self) -> &Arc<EEVDFTask<T>> {
        self.task
            .as_ref()
            .expect("live EEVDF reservation always owns its task")
    }

    /// Cancel publication and return the exact task.  A fork seed, if any,
    /// remains installed because cancellation returns the task to its
    /// virgin/fork-seeded state.
    pub fn cancel(mut self) -> Result<Arc<EEVDFTask<T>>, SchedulerError> {
        let task = self
            .task
            .as_ref()
            .expect("live EEVDF reservation always owns its task");
        if task.owner() != CONFIGURING {
            return Err(SchedulerError::InconsistentState);
        }
        task.transfer_owner(CONFIGURING, UNOWNED)?;
        Ok(self
            .task
            .take()
            .expect("live EEVDF reservation always owns its task"))
    }
}

impl<T> fmt::Debug for EevdfTaskReservation<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EevdfTaskReservation")
            .field("scheduler_id", &self.scheduler_id)
            .field("sequence", &self.sequence)
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl<T> Drop for EevdfTaskReservation<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.transfer_owner(CONFIGURING, UNOWNED);
        }
    }
}

/// Failed commit of an EEVDF admission reservation.  The reservation remains
/// live and can be retried on the owning scheduler or explicitly cancelled.
pub struct EevdfReservationCommitError<T> {
    kind: SchedulerError,
    reservation: EevdfTaskReservation<T>,
}

impl<T> EevdfReservationCommitError<T> {
    pub const fn kind(&self) -> SchedulerError {
        self.kind
    }

    pub const fn reservation(&self) -> &EevdfTaskReservation<T> {
        &self.reservation
    }

    pub fn into_reservation(self) -> EevdfTaskReservation<T> {
        self.reservation
    }
}

impl<T> fmt::Debug for EevdfReservationCommitError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EevdfReservationCommitError")
            .field("kind", &self.kind)
            .field("reservation", &self.reservation)
            .finish()
    }
}

/// A typed, allocation-free handoff of one EEVDF task between run queues.
///
/// A live token owns a CONFIGURING claim.  All rollback metadata remains in
/// the task's single private [`MigrationState`]; this capability carries only
/// the task, immutable source identity, and whether it is already parked.
#[must_use = "dropping a migration parks the task for explicit recovery"]
pub struct EevdfMigration<T> {
    task: Option<Arc<EEVDFTask<T>>>,
    source_scheduler_id: usize,
    origin: EevdfMigrationOrigin,
    parked: bool,
}

impl<T> EevdfMigration<T> {
    pub fn task(&self) -> &Arc<EEVDFTask<T>> {
        self.task
            .as_ref()
            .expect("live EEVDF migration always owns its task")
    }

    pub const fn source_scheduler_id(&self) -> usize {
        self.source_scheduler_id
    }

    /// Return whether this migration originated from a ready or running task.
    pub const fn origin(&self) -> EevdfMigrationOrigin {
        self.origin
    }

    /// Park the task in UNOWNED state while retaining its migration snapshot.
    /// The returned task can later be passed to `resume_migration`.
    pub fn park(mut self) -> Result<Arc<EEVDFTask<T>>, SchedulerError> {
        let task = self
            .task
            .as_ref()
            .expect("live EEVDF migration always owns its task");
        if task.owner() != CONFIGURING {
            return Err(SchedulerError::InconsistentState);
        }
        task.transfer_owner(CONFIGURING, UNOWNED)?;
        Ok(self
            .task
            .take()
            .expect("live EEVDF migration always owns its task"))
    }

    pub fn cancel(self) -> Result<Arc<EEVDFTask<T>>, SchedulerError> {
        self.park()
    }
}

impl<T> fmt::Debug for EevdfMigration<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EevdfMigration")
            .field("source_scheduler_id", &self.source_scheduler_id)
            .field("origin", &self.origin)
            .field("parked", &self.parked)
            .finish_non_exhaustive()
    }
}

impl<T> Drop for EevdfMigration<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Preserve the migration metadata in the task state.  This is a
            // deliberate park, not a silent loss of the task/snapshot.
            if task.owner() == CONFIGURING {
                let _ = task.transfer_owner(CONFIGURING, UNOWNED);
            }
        }
    }
}

/// Failed migration commit retaining the complete live migration token.
pub struct EevdfMigrationCommitError<T> {
    kind: SchedulerError,
    migration: EevdfMigration<T>,
}

impl<T> EevdfMigrationCommitError<T> {
    pub const fn kind(&self) -> SchedulerError {
        self.kind
    }

    pub const fn migration(&self) -> &EevdfMigration<T> {
        &self.migration
    }

    pub fn into_migration(self) -> EevdfMigration<T> {
        self.migration
    }
}

impl<T> fmt::Debug for EevdfMigrationCommitError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EevdfMigrationCommitError")
            .field("kind", &self.kind)
            .field("migration", &self.migration)
            .finish()
    }
}

const FAIR_CLASS_RANK: u8 = 1;
const RT_CLASS_RANK: u8 = 0;

fn is_rt_class(class: EevdfTaskClass) -> bool {
    matches!(class, EevdfTaskClass::RoundRobin | EevdfTaskClass::Fifo)
}

fn fair_class(class: EevdfTaskClass) -> Option<crate::eevdf_model::RequestClass> {
    match class {
        EevdfTaskClass::Normal => Some(crate::eevdf_model::RequestClass::Normal),
        EevdfTaskClass::Batch => Some(crate::eevdf_model::RequestClass::Batch),
        EevdfTaskClass::Idle => Some(crate::eevdf_model::RequestClass::Idle),
        EevdfTaskClass::RoundRobin | EevdfTaskClass::Fifo => None,
    }
}

/// Rebound a fork seed at the child's admission class.
///
/// The parent and child may have different fair classes while the child is
/// still virgin.  A seed is only a lag hint, so it must not carry a larger
/// sleeper-credit cap from the parent's class into the child's request.
fn fork_lag_for_class(
    seed: Option<EevdfForkSeed>,
    child_class: crate::eevdf_model::RequestClass,
) -> i128 {
    let cap = credit_cap(child_class);
    seed.map_or(0, |seed| seed.lag.clamp(-cap, cap))
}

fn model_error(error: ModelError) -> SchedulerError {
    match error {
        ModelError::ArithmeticExhausted => SchedulerError::ArithmeticExhausted,
        ModelError::InvalidWeight | ModelError::InvalidState => SchedulerError::InconsistentState,
    }
}

fn tree_error(error: EevdfTreeError) -> SchedulerError {
    match error {
        EevdfTreeError::AlreadyLinked => SchedulerError::AlreadyQueued,
        EevdfTreeError::DuplicateKey | EevdfTreeError::ForeignNode => {
            SchedulerError::InconsistentState
        }
    }
}

fn rt_order(priority: u8) -> u128 {
    (RT_PRIORITY_MAX - priority) as u128
}

/// An allocation-free, single-runqueue EEVDF scheduler.
///
/// The ready set is one augmented intrusive tree.  The current task remains
/// owned by this scheduler while it runs; this makes task ownership and the
/// `UnsafeCell` state slot stable across pick/put transitions.
pub struct EEVDFScheduler<T> {
    ready_tree: EevdfTree<EevdfReadyKey, EevdfTaskPayload<T>>,
    running: Option<Arc<EEVDFTask<T>>>,
    clock: Clock,
    id: usize,
    fair_sequence: i128,
    rt_front_sequence: i128,
    rt_back_sequence: i128,
}

impl<T> EEVDFScheduler<T> {
    /// Creates an empty EEVDF runqueue.
    pub const fn new() -> Self {
        Self {
            ready_tree: EevdfTree::new(),
            running: None,
            clock: Clock::new(0),
            id: UNOWNED,
            fair_sequence: 0,
            rt_front_sequence: 0,
            rt_back_sequence: 0,
        }
    }

    /// Returns the scheduler name.
    pub fn scheduler_name() -> &'static str {
        "EEVDF"
    }

    /// Returns a copy of the queue-local clock for diagnostics and tests.
    pub const fn clock(&self) -> Clock {
        self.clock
    }

    /// Returns the number of ready tasks.  The running task is not linked.
    pub const fn ready_len(&self) -> usize {
        self.ready_tree.len()
    }

    /// Return whether `task` is the current owner of this run queue.
    ///
    /// The task layer uses this under the scheduler lock to account a pending
    /// wall-clock sample before applying a remote scheduling-parameter
    /// transaction.  Looking at task state alone is insufficient because a
    /// migration or switch can leave that publication briefly stale.
    pub fn is_running_task(&self, task: &Arc<EEVDFTask<T>>) -> bool {
        self.running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, task))
    }

    /// Select the first ready task accepted by `predicate` in structural key
    /// order, inspecting at most `limit` tasks.
    ///
    /// This is intentionally a read-only candidate query.  The returned task
    /// is still linked and owned by this scheduler; callers that hold the
    /// scheduler's run-queue exclusion may use the retained Arc immediately
    /// with [`Self::begin_ready_migration`], or drop it without changing the
    /// queue.  The bound includes the complete RT prefix because RT keys sort
    /// before fair keys.  `limit == 0` and an empty queue perform no predicate
    /// access and return `None`.
    pub fn bounded_ready_candidate<P>(
        &self,
        limit: usize,
        mut predicate: P,
    ) -> Option<EevdfReadyCandidate<T>>
    where
        P: FnMut(&EEVDFTask<T>) -> bool,
    {
        let (task, visited) = self
            .ready_tree
            .find_bounded(limit, |node| predicate(node))?;
        let key = unsafe { task.key() };
        let class = task.sched_params().class;
        Some(EevdfReadyCandidate {
            task,
            key,
            class,
            visited,
        })
    }

    /// Select one ready task after a caller-owned continuation key.
    ///
    /// The cursor advances through every inspected structural node, including
    /// rejected nodes.  Once the end is reached it wraps on the next call.
    /// This makes an idle puller fair to later tree entries without allowing a
    /// single attempt to exceed the same bounded predicate work as the normal
    /// query.
    pub fn bounded_ready_candidate_from<P>(
        &self,
        cursor: &mut EevdfReadyCursor,
        limit: usize,
        mut predicate: P,
    ) -> Option<EevdfReadyCandidate<T>>
    where
        P: FnMut(&EEVDFTask<T>) -> bool,
    {
        let (candidate, last_key) = self
            .ready_tree
            .find_bounded_after(cursor.0, limit, |node| predicate(node));
        cursor.0 = last_key;
        let (task, visited) = candidate?;
        let key = unsafe { task.key() };
        let class = task.sched_params().class;
        Some(EevdfReadyCandidate {
            task,
            key,
            class,
            visited,
        })
    }

    /// Produce a bounded fair fork seed while holding the scheduler's
    /// exclusive run-queue borrow.  The seed intentionally contains no
    /// active request or remaining-credit field.
    pub fn fork_seed(
        &mut self,
        parent: &Arc<EEVDFTask<T>>,
    ) -> Result<EevdfForkSeed, SchedulerError> {
        if self.id == UNOWNED || parent.owner() != self.id {
            return Err(SchedulerError::ForeignQueue);
        }
        let entity =
            unsafe { parent.owned_state().entity }.ok_or(SchedulerError::IncompatibleClass)?;
        if entity.is_sleeping() {
            return Err(SchedulerError::IncompatibleClass);
        }
        let lag = entity.lag_at(self.clock.v).map_err(model_error)?;
        let cap = credit_cap(entity.class);
        let lag = lag.clamp(-cap, cap);
        Ok(EevdfForkSeed {
            class: entity.class,
            lag,
        })
    }

    pub fn install_fork_seed(
        &mut self,
        child: &Arc<EEVDFTask<T>>,
        seed: EevdfForkSeed,
    ) -> Result<(), SchedulerError> {
        child.install_fork_seed(seed)
    }

    fn ensure_id(&mut self) -> Result<usize, SchedulerError> {
        if self.id == UNOWNED {
            self.id = allocate_scheduler_id()?;
        }
        Ok(self.id)
    }

    fn next_fair_sequence(&mut self) -> Result<i128, SchedulerError> {
        let sequence = self.fair_sequence;
        self.fair_sequence = sequence
            .checked_add(1)
            .ok_or(SchedulerError::SequenceExhausted)?;
        Ok(sequence)
    }

    fn next_rt_sequence(&mut self, front: bool) -> Result<i128, SchedulerError> {
        if front {
            self.rt_front_sequence = self
                .rt_front_sequence
                .checked_sub(1)
                .ok_or(SchedulerError::SequenceExhausted)?;
            Ok(self.rt_front_sequence)
        } else {
            let sequence = self.rt_back_sequence;
            self.rt_back_sequence = sequence
                .checked_add(1)
                .ok_or(SchedulerError::SequenceExhausted)?;
            Ok(sequence)
        }
    }

    fn peek_fair_sequence(&self) -> Result<i128, SchedulerError> {
        if self.fair_sequence == i128::MAX {
            Err(SchedulerError::SequenceExhausted)
        } else {
            Ok(self.fair_sequence)
        }
    }

    fn peek_rt_sequence(&self, front: bool) -> Result<i128, SchedulerError> {
        if front {
            self.rt_front_sequence
                .checked_sub(1)
                .ok_or(SchedulerError::SequenceExhausted)
        } else {
            self.rt_back_sequence
                .checked_add(1)
                .map(|_| self.rt_back_sequence)
                .ok_or(SchedulerError::SequenceExhausted)
        }
    }

    fn commit_fair_sequence(&mut self) {
        self.fair_sequence = self.fair_sequence.saturating_add(1);
    }

    fn commit_rt_sequence(&mut self, front: bool) {
        if front {
            self.rt_front_sequence = self.rt_front_sequence.saturating_sub(1);
        } else {
            self.rt_back_sequence = self.rt_back_sequence.saturating_add(1);
        }
    }

    fn stage_key(task: &Arc<EEVDFTask<T>>, key: EevdfReadyKey, eligible_at: u128) {
        // SAFETY: every caller holds this scheduler's logical task ownership,
        // and the task is unlinked while the scheduler lock/exclusion is held.
        unsafe { task.stage_unlinked(key, eligible_at) };
    }

    fn fair_weight(params: EevdfTaskParams) -> Result<u128, SchedulerError> {
        eevdf_weight_for(params).ok_or(SchedulerError::InvalidParameters)
    }

    /// Return the tuple component which owns a wall-to-Q32 conversion residue.
    /// RT classes do not use a fair weight, but retaining a zero value keeps
    /// the ownership check explicit for every class.
    fn runtime_fraction_weight(params: EevdfTaskParams) -> Result<u128, SchedulerError> {
        if is_rt_class(params.class) {
            Ok(0)
        } else {
            Self::fair_weight(params)
        }
    }

    /// A non-zero conversion residue is valid only while the published class
    /// and effective weight still match the tuple which produced it. The
    /// residue is below one Q32 service unit; it is not safe to feed it to a
    /// different tuple merely because the task's packed parameters changed.
    fn validate_runtime_fraction_owner(
        task: &Arc<EEVDFTask<T>>,
        state: EevdfOwnedState,
    ) -> Result<(), SchedulerError> {
        if state.runtime_remainder_ns == 0 && state.runtime_fraction_remainder_ns == 0 {
            return Ok(());
        }
        let params = task.sched_params();
        let weight = Self::runtime_fraction_weight(params)?;
        if state.runtime_fraction_class != params.class
            || state.runtime_fraction_weight != weight
            || state.runtime_fraction_period_ns == 0
        {
            return Err(SchedulerError::InconsistentState);
        }
        Ok(())
    }

    /// Clear a conversion residue at an explicit scheduling-tuple boundary.
    /// The preceding settlement has already materialized every representable
    /// whole/Q32 service unit under the old class and weight. What remains is
    /// strictly below one Q32 unit (`remainder < period`), which this model
    /// cannot represent exactly; dropping it here prevents charging that
    /// old-tuple precision token under the new class or weight.
    fn clear_runtime_fraction(state: &mut EevdfOwnedState) {
        state.runtime_fraction_remainder_ns = 0;
        state.runtime_fraction_period_ns = 0;
        state.runtime_fraction_class = EevdfTaskClass::Normal;
        state.runtime_fraction_weight = 0;
    }

    fn publish_runtime_fraction_owner(
        state: &mut EevdfOwnedState,
        params: EevdfTaskParams,
        weight: u128,
        remainder: u64,
        period_ns: u64,
    ) {
        if remainder == 0 {
            state.runtime_fraction_remainder_ns = 0;
            state.runtime_fraction_period_ns = if state.runtime_remainder_ns == 0 {
                0
            } else {
                period_ns
            };
            if state.runtime_remainder_ns == 0 {
                state.runtime_fraction_class = EevdfTaskClass::Normal;
                state.runtime_fraction_weight = 0;
            } else {
                state.runtime_fraction_class = params.class;
                state.runtime_fraction_weight = weight;
            }
        } else {
            state.runtime_fraction_remainder_ns = remainder;
            state.runtime_fraction_period_ns = period_ns;
            state.runtime_fraction_class = params.class;
            state.runtime_fraction_weight = weight;
        }
    }

    fn fair_key(entity: &Entity, sequence: i128) -> Result<(EevdfReadyKey, u128), SchedulerError> {
        let deadline = entity.request.deadline;
        let eligible_at = entity.eligible_at().map_err(model_error)?;
        Ok((
            EevdfReadyKey::new(FAIR_CLASS_RANK, bias_i128(deadline), sequence),
            bias_i128(eligible_at),
        ))
    }

    fn rt_key(params: EevdfTaskParams, sequence: i128) -> EevdfReadyKey {
        EevdfReadyKey::new(RT_CLASS_RANK, rt_order(params.rt_priority), sequence)
    }

    /// Renew the integer and Q32 portions of an RR slice as one scheduler
    /// state transition.  A slice boundary must never leave fractional
    /// progress from the previous slice attached to the new budget.
    fn reset_rr_slice(task: &Arc<EEVDFTask<T>>) {
        task.set_rr_remaining(RR_TIMESLICE_TICKS);
        // SAFETY: every caller owns the task under the scheduler exclusion.
        unsafe { task.owned_state_mut().rr_fraction_q = 0 };
    }

    fn release_claim(&self, task: &Arc<EEVDFTask<T>>) {
        task.transfer_owner(self.id, UNOWNED)
            .expect("EEVDF task owner changed while a claim was held");
    }

    unsafe fn stage_migration_state(
        task: &Arc<EEVDFTask<T>>,
        source_scheduler_id: usize,
        params: EevdfTaskParams,
        snapshot: Option<MigrationSnapshot>,
        source_clock: Clock,
        detached_clock: Clock,
        source_entity: Option<Entity>,
        source_key: EevdfReadyKey,
        source_eligible_at: u128,
        rr_remaining: usize,
        runtime_remainder_ns: u64,
        runtime_fraction_remainder_ns: u64,
        runtime_fraction_period_ns: u64,
        runtime_fraction_class: EevdfTaskClass,
        runtime_fraction_weight: u128,
        rr_fraction_q: u64,
        dormant_fair: Option<Entity>,
        origin: EevdfMigrationOrigin,
    ) {
        let state = task.owned_state_mut();
        state.entity = None;
        state.runtime_remainder_ns = 0;
        state.migration = Some(MigrationState {
            source_scheduler_id,
            params,
            snapshot,
            source_clock,
            detached_clock,
            source_entity,
            source_key,
            source_eligible_at,
            rr_remaining,
            runtime_remainder_ns,
            runtime_fraction_remainder_ns,
            runtime_fraction_period_ns,
            runtime_fraction_class,
            runtime_fraction_weight,
            rr_fraction_q,
            dormant_fair,
            origin,
        });
        state.dormant_fair = None;
        state.rt_sleeping = false;
        state.sleep_v = None;
    }

    fn insert_staged(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
        key: EevdfReadyKey,
        eligible_at: u128,
        old_key: EevdfReadyKey,
        old_eligible_at: u128,
    ) -> Result<(), SchedulerError> {
        Self::stage_key(task, key, eligible_at);
        match self.ready_tree.insert(Arc::clone(task)) {
            Ok(()) => Ok(()),
            Err(error) => {
                Self::stage_key(task, old_key, old_eligible_at);
                let kind = error.kind();
                drop(error.into_node());
                Err(tree_error(kind))
            }
        }
    }

    fn enqueue_new(&mut self, task: Arc<EEVDFTask<T>>) -> Result<(), SchedulerError> {
        let scheduler_id = self.ensure_id()?;
        task.claim(scheduler_id)?;

        // A New task must be virgin.  The check is under the just-acquired
        // owner exclusion before touching any scheduler-owned state.
        // SAFETY: the task owner claim excludes all state-slot access here.
        if !unsafe { task.is_virgin() } {
            self.release_claim(&task);
            return Err(SchedulerError::AlreadyQueued);
        }

        let params = task.sched_params();
        // A fork seed affects only the child's initial lag.  Admission still
        // constructs a fresh request from the child's current parameters.
        let fork_seed = unsafe { task.owned_state().fork_seed };
        if is_rt_class(params.class) && fork_seed.is_some() {
            self.release_claim(&task);
            return Err(SchedulerError::IncompatibleClass);
        }
        let (next_clock, entity, key, eligible_at) = if is_rt_class(params.class) {
            let sequence = match params.class {
                EevdfTaskClass::Fifo | EevdfTaskClass::RoundRobin => self.next_rt_sequence(false),
                EevdfTaskClass::Normal | EevdfTaskClass::Batch | EevdfTaskClass::Idle => {
                    unreachable!("validated realtime class mismatch")
                }
            };
            let sequence = match sequence {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            (self.clock, None, Self::rt_key(params, sequence), 0)
        } else {
            let weight = match Self::fair_weight(params) {
                Ok(weight) => weight,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            let mut next_clock = self.clock;
            if let Err(error) = next_clock.checked_add_weight(weight).map_err(model_error) {
                self.release_claim(&task);
                return Err(error);
            }
            let class = fair_class(params.class).expect("fair class conversion failed");
            let entity = match Entity::with_lag(
                class,
                weight,
                next_clock.total_weight,
                next_clock.v,
                fork_lag_for_class(fork_seed, class),
            )
            .map_err(model_error)
            {
                Ok(entity) => entity,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            let sequence = match self.next_fair_sequence() {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            let (key, eligible_at) = match Self::fair_key(&entity, sequence) {
                Ok(key) => key,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            (next_clock, Some(entity), key, eligible_at)
        };

        // New nodes start with the constructor key/eligibility.  Retaining
        // those values lets insertion failure restore the exact unlinked
        // representation without publishing owner or model state.
        let old_key = unsafe { task.key() };
        if let Err(error) = self.insert_staged(&task, key, eligible_at, old_key, 0) {
            self.release_claim(&task);
            return Err(error);
        }

        // SAFETY: the task is owned by this scheduler and remains behind the
        // runqueue exclusion while its entity is published.
        unsafe {
            task.owned_state_mut().entity = entity;
            task.owned_state_mut().runtime_remainder_ns = 0;
            task.owned_state_mut().runtime_fraction_remainder_ns = 0;
            task.owned_state_mut().runtime_fraction_period_ns = 0;
            task.owned_state_mut().runtime_fraction_class = EevdfTaskClass::Normal;
            task.owned_state_mut().runtime_fraction_weight = 0;
            task.owned_state_mut().rr_fraction_q = 0;
            task.owned_state_mut().migration = None;
            task.owned_state_mut().dormant_fair = None;
            task.owned_state_mut().rt_sleeping = false;
            task.owned_state_mut().fork_seed = None;
            task.owned_state_mut().sleep_v = None;
        }
        if matches!(params.class, EevdfTaskClass::RoundRobin) {
            Self::reset_rr_slice(&task);
        }
        self.clock = next_clock;
        Ok(())
    }

    /// Reserve publication of one brand-new task without linking it.
    pub fn reserve_new_task(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
    ) -> Result<EevdfTaskReservation<T>, SchedulerError> {
        let scheduler_id = self.ensure_id()?;
        task.value().claim_reservation()?;
        let params = task.sched_params();
        let sequence = if is_rt_class(params.class) {
            self.next_rt_sequence(false)
        } else {
            self.next_fair_sequence()
        };
        let sequence = match sequence {
            Ok(sequence) => sequence,
            Err(error) => {
                task.transfer_owner(CONFIGURING, UNOWNED)
                    .expect("EEVDF reservation owner changed while releasing failed reservation");
                return Err(error);
            }
        };
        Ok(EevdfTaskReservation {
            task: Some(Arc::clone(task)),
            scheduler_id,
            sequence,
            params,
        })
    }

    /// Commit an admission reservation created by this exact scheduler.
    /// No scheduler identity or ordering sequence is acquired here.
    pub fn commit_reserved_task(
        &mut self,
        mut reservation: EevdfTaskReservation<T>,
    ) -> Result<Arc<EEVDFTask<T>>, EevdfReservationCommitError<T>> {
        if reservation.scheduler_id != self.id || self.id == UNOWNED {
            return Err(EevdfReservationCommitError {
                kind: SchedulerError::ForeignQueue,
                reservation,
            });
        }
        let task = Arc::clone(reservation.task());
        if task.owner() != CONFIGURING
            || task.sched_params() != reservation.params
            || !unsafe { task.is_virgin() }
        {
            return Err(EevdfReservationCommitError {
                kind: SchedulerError::InconsistentState,
                reservation,
            });
        }
        let params = reservation.params;
        let fork_seed = unsafe { task.owned_state().fork_seed };
        if is_rt_class(params.class) && fork_seed.is_some() {
            return Err(EevdfReservationCommitError {
                kind: SchedulerError::IncompatibleClass,
                reservation,
            });
        }
        let (next_clock, entity, key, eligible_at) = if is_rt_class(params.class) {
            (
                self.clock,
                None,
                Self::rt_key(params, reservation.sequence),
                0,
            )
        } else {
            let weight = match Self::fair_weight(params) {
                Ok(weight) => weight,
                Err(kind) => return Err(EevdfReservationCommitError { kind, reservation }),
            };
            let mut next_clock = self.clock;
            if let Err(error) = next_clock.checked_add_weight(weight).map_err(model_error) {
                return Err(EevdfReservationCommitError {
                    kind: error,
                    reservation,
                });
            }
            let entity = match Entity::with_lag(
                fair_class(params.class).expect("fair class conversion failed"),
                weight,
                next_clock.total_weight,
                next_clock.v,
                fork_lag_for_class(
                    fork_seed,
                    fair_class(params.class).expect("fair class conversion failed"),
                ),
            )
            .map_err(model_error)
            {
                Ok(entity) => entity,
                Err(kind) => return Err(EevdfReservationCommitError { kind, reservation }),
            };
            let (key, eligible_at) = match Self::fair_key(&entity, reservation.sequence) {
                Ok(value) => value,
                Err(kind) => return Err(EevdfReservationCommitError { kind, reservation }),
            };
            (next_clock, Some(entity), key, eligible_at)
        };
        let old_key = unsafe { task.key() };
        if let Err(kind) = self.insert_staged(&task, key, eligible_at, old_key, 0) {
            return Err(EevdfReservationCommitError { kind, reservation });
        }
        if matches!(params.class, EevdfTaskClass::RoundRobin) {
            Self::reset_rr_slice(&task);
        }
        // SAFETY: the reservation's CONFIGURING claim excludes all state
        // access until this publication completes.
        unsafe {
            let state = task.owned_state_mut();
            state.entity = entity;
            state.runtime_remainder_ns = 0;
            state.runtime_fraction_remainder_ns = 0;
            state.runtime_fraction_period_ns = 0;
            state.runtime_fraction_class = EevdfTaskClass::Normal;
            state.runtime_fraction_weight = 0;
            state.rr_fraction_q = 0;
            state.migration = None;
            state.dormant_fair = None;
            state.rt_sleeping = false;
            state.fork_seed = None;
            state.sleep_v = None;
        }
        task.transfer_owner(CONFIGURING, self.id)
            .expect("EEVDF reservation owner changed before commit publication");
        self.clock = next_clock;
        let task = reservation
            .task
            .take()
            .expect("live EEVDF reservation always owns its task");
        Ok(task)
    }

    fn migration_token_from_task(
        task: Arc<EEVDFTask<T>>,
        parked: bool,
    ) -> Option<EevdfMigration<T>> {
        // SAFETY: callers hold the task's CONFIGURING claim (or an equivalent
        // scheduler owner exclusion) while reading the one task-owned record.
        unsafe {
            let metadata = task.owned_state().migration?;
            Some(EevdfMigration {
                task: Some(task),
                source_scheduler_id: metadata.source_scheduler_id,
                origin: metadata.origin,
                parked,
            })
        }
    }

    /// Detach a ready task and create a typed migration token.
    pub fn begin_ready_migration(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
    ) -> Result<EevdfMigration<T>, SchedulerError> {
        if self.id == UNOWNED || task.owner() != self.id {
            return Err(SchedulerError::ForeignQueue);
        }
        if self
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, task))
        {
            return Err(SchedulerError::InconsistentState);
        }
        if !task.is_linked() {
            return Err(SchedulerError::InconsistentState);
        }
        let source_clock = self.clock;
        let params = task.sched_params();
        let source_key = unsafe { task.key() };
        let source_eligible_at = unsafe { task.eligible_at() };
        let old_state = unsafe { *task.owned_state() };
        let source_entity = old_state.entity;
        let snapshot = if let Some(entity) = source_entity {
            Some(
                entity
                    .migration_snapshot(source_clock.v)
                    .map_err(model_error)?,
            )
        } else {
            None
        };
        let mut detached_clock = source_clock;
        if let Some(entity) = source_entity {
            detached_clock
                .checked_sub_weight(entity.weight)
                .map_err(model_error)?;
        }
        let removed = self.ready_tree.remove(task).map_err(tree_error)?;
        removed
            .transfer_owner(self.id, CONFIGURING)
            .expect("EEVDF ready owner changed while beginning migration");
        // SAFETY: the source scheduler owns the task and has removed its link.
        unsafe {
            Self::stage_migration_state(
                &removed,
                self.id,
                params,
                snapshot,
                source_clock,
                detached_clock,
                source_entity,
                source_key,
                source_eligible_at,
                removed.rr_remaining(),
                old_state.runtime_remainder_ns,
                old_state.runtime_fraction_remainder_ns,
                old_state.runtime_fraction_period_ns,
                old_state.runtime_fraction_class,
                old_state.runtime_fraction_weight,
                old_state.rr_fraction_q,
                old_state.dormant_fair,
                EevdfMigrationOrigin::Ready,
            );
        }
        self.clock = detached_clock;
        Self::migration_token_from_task(removed, false).ok_or(SchedulerError::InconsistentState)
    }

    /// Detach the currently running task for an axtask CPU handoff.  The
    /// returned token owns a CONFIGURING claim.  The explicit park bridge can
    /// release that claim after it has transferred the task-layer reference.
    pub fn begin_running_migration(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
    ) -> Result<EevdfMigration<T>, SchedulerError> {
        self.ensure_running(task)?;
        self.settle_before_lifecycle(task)?;
        let source_clock = self.clock;
        let params = task.sched_params();
        let old_state = unsafe { *task.owned_state() };
        let source_entity = old_state.entity;
        let source_key = unsafe { task.key() };
        let source_eligible_at = unsafe { task.eligible_at() };
        let snapshot = if let Some(entity) = source_entity {
            Some(
                entity
                    .migration_snapshot(source_clock.v)
                    .map_err(model_error)?,
            )
        } else {
            None
        };
        let mut detached_clock = source_clock;
        if let Some(entity) = source_entity {
            detached_clock
                .checked_sub_weight(entity.weight)
                .map_err(model_error)?;
        }
        // SAFETY: the current task is owned by this scheduler.
        unsafe {
            Self::stage_migration_state(
                task,
                self.id,
                params,
                snapshot,
                source_clock,
                detached_clock,
                source_entity,
                source_key,
                source_eligible_at,
                task.rr_remaining(),
                old_state.runtime_remainder_ns,
                old_state.runtime_fraction_remainder_ns,
                old_state.runtime_fraction_period_ns,
                old_state.runtime_fraction_class,
                old_state.runtime_fraction_weight,
                old_state.rr_fraction_q,
                old_state.dormant_fair,
                EevdfMigrationOrigin::Running,
            );
        }
        self.clock = detached_clock;
        self.running = None;
        task.transfer_owner(self.id, CONFIGURING)
            .expect("EEVDF running owner changed while beginning migration");
        Self::migration_token_from_task(Arc::clone(task), false)
            .ok_or(SchedulerError::InconsistentState)
    }

    fn claim_migration_for_commit(task: &Arc<EEVDFTask<T>>) -> Result<(), SchedulerError> {
        match task.owner() {
            CONFIGURING => Ok(()),
            UNOWNED => task.value().claim_migration(),
            _ => Err(SchedulerError::ForeignQueue),
        }
    }

    fn migration_error(
        kind: SchedulerError,
        migration: EevdfMigration<T>,
    ) -> EevdfMigrationCommitError<T> {
        // A token created from an explicitly parked task must remain parked
        // when destination/source preparation fails.  Active CONFIGURING
        // tokens retain that claim for direct retry.
        if migration.parked {
            if let Some(task) = migration.task.as_ref() {
                if task.owner() == CONFIGURING {
                    task.transfer_owner(CONFIGURING, UNOWNED)
                        .expect("EEVDF parked migration owner changed while releasing failure");
                }
            }
        }
        EevdfMigrationCommitError { kind, migration }
    }

    /// Commit a migration token on this destination run queue.  All model,
    /// clock, and key calculations occur before any destination publication;
    /// failed insertion returns the live token and leaves this scheduler
    /// untouched.
    pub fn commit_migration(
        &mut self,
        mut migration: EevdfMigration<T>,
    ) -> Result<Arc<EEVDFTask<T>>, EevdfMigrationCommitError<T>> {
        let task = Arc::clone(migration.task());
        if migration.source_scheduler_id == self.id && self.id != UNOWNED {
            return Err(EevdfMigrationCommitError {
                kind: SchedulerError::ForeignQueue,
                migration,
            });
        }
        if let Err(kind) = Self::claim_migration_for_commit(&task) {
            return Err(Self::migration_error(kind, migration));
        }
        // SAFETY: `claim_migration_for_commit` holds CONFIGURING for a parked
        // token, and an active token already owns that claim.
        let metadata = unsafe { task.owned_state().migration };
        let Some(metadata) = metadata else {
            return Err(Self::migration_error(
                SchedulerError::InconsistentState,
                migration,
            ));
        };
        if metadata.source_scheduler_id != migration.source_scheduler_id
            || metadata.origin != migration.origin
            || task.sched_params() != metadata.params
        {
            return Err(Self::migration_error(
                SchedulerError::InconsistentState,
                migration,
            ));
        }
        // Allocate the destination identity once.  A failed commit retains
        // the token, so retries must not consume a fresh scheduler ID.
        let destination_id = match self.ensure_id() {
            Ok(id) => id,
            Err(kind) => return Err(Self::migration_error(kind, migration)),
        };
        let (next_clock, entity) = if let Some(snapshot) = metadata.snapshot {
            let mut next_clock = self.clock;
            if let Err(kind) = next_clock
                .checked_add_weight(snapshot.weight)
                .map_err(model_error)
            {
                return Err(Self::migration_error(kind, migration));
            }
            let entity = match Entity::from_migration(snapshot, next_clock.v).map_err(model_error) {
                Ok(entity) => entity,
                Err(kind) => return Err(Self::migration_error(kind, migration)),
            };
            (next_clock, Some(entity))
        } else {
            (self.clock, None)
        };

        let fair = entity.is_some();
        let (key, eligible_at) = if let Some(entity) = entity.as_ref() {
            let sequence = match self.peek_fair_sequence() {
                Ok(sequence) => sequence,
                Err(kind) => return Err(Self::migration_error(kind, migration)),
            };
            let (key, eligible_at) = match Self::fair_key(&entity, sequence) {
                Ok(value) => value,
                Err(kind) => return Err(Self::migration_error(kind, migration)),
            };
            (key, eligible_at)
        } else {
            let sequence = match self.peek_rt_sequence(false) {
                Ok(sequence) => sequence,
                Err(kind) => return Err(Self::migration_error(kind, migration)),
            };
            (Self::rt_key(metadata.params, sequence), 0)
        };
        if let Err(kind) = self.insert_staged(
            &task,
            key,
            eligible_at,
            metadata.source_key,
            metadata.source_eligible_at,
        ) {
            return Err(Self::migration_error(kind, migration));
        }
        let renew_rr = matches!(metadata.params.class, EevdfTaskClass::RoundRobin)
            && metadata.rr_remaining == 0;
        if renew_rr {
            Self::reset_rr_slice(&task);
        } else {
            task.set_rr_remaining(metadata.rr_remaining);
            // SAFETY: CONFIGURING excludes all task-state access.
            unsafe { task.owned_state_mut().rr_fraction_q = metadata.rr_fraction_q };
        }
        // SAFETY: the task remains behind CONFIGURING until publication.
        unsafe {
            let state = task.owned_state_mut();
            state.entity = entity;
            state.runtime_remainder_ns = metadata.runtime_remainder_ns;
            state.runtime_fraction_remainder_ns = metadata.runtime_fraction_remainder_ns;
            state.runtime_fraction_period_ns = metadata.runtime_fraction_period_ns;
            state.runtime_fraction_class = metadata.runtime_fraction_class;
            state.runtime_fraction_weight = metadata.runtime_fraction_weight;
            state.rr_fraction_q = if renew_rr { 0 } else { metadata.rr_fraction_q };
            state.dormant_fair = metadata.dormant_fair;
            state.migration = None;
            state.rt_sleeping = false;
            state.fork_seed = None;
            state.sleep_v = None;
        }
        task.transfer_owner(CONFIGURING, destination_id)
            .expect("EEVDF migration owner changed before ready commit");
        self.id = destination_id;
        self.clock = next_clock;
        if fair {
            self.commit_fair_sequence();
        } else {
            self.commit_rt_sequence(false);
        }
        Ok(migration
            .task
            .take()
            .expect("live EEVDF migration always owns its task"))
    }

    /// Recover a migration parked by token drop/cancel or by a base-trait
    /// handoff.  The returned token is again live and can be committed or
    /// rolled back with typed failure handling.
    pub fn resume_migration(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
    ) -> Result<EevdfMigration<T>, SchedulerError> {
        if task.owner() != UNOWNED {
            return Err(if task.owner() == CONFIGURING {
                SchedulerError::TaskBusy
            } else {
                SchedulerError::ForeignQueue
            });
        }
        task.value().claim_migration()?;
        Self::migration_token_from_task(Arc::clone(task), true).ok_or_else(|| {
            task.transfer_owner(CONFIGURING, UNOWNED)
                .expect("EEVDF migration owner changed while releasing resume failure");
            SchedulerError::InconsistentState
        })
    }

    pub fn rollback_migration(
        &mut self,
        mut migration: EevdfMigration<T>,
    ) -> Result<Arc<EEVDFTask<T>>, EevdfMigrationCommitError<T>> {
        if self.id == UNOWNED || migration.source_scheduler_id != self.id {
            return Err(EevdfMigrationCommitError {
                kind: SchedulerError::ForeignQueue,
                migration,
            });
        }
        let task = Arc::clone(migration.task());
        if let Err(kind) = Self::claim_migration_for_commit(&task) {
            return Err(Self::migration_error(kind, migration));
        }
        // SAFETY: the migration claim is held for this complete snapshot.
        let metadata = unsafe { task.owned_state().migration };
        let Some(metadata) = metadata else {
            return Err(Self::migration_error(
                SchedulerError::InconsistentState,
                migration,
            ));
        };
        if metadata.source_scheduler_id != migration.source_scheduler_id
            || metadata.origin != migration.origin
            || task.sched_params() != metadata.params
        {
            return Err(Self::migration_error(
                SchedulerError::InconsistentState,
                migration,
            ));
        }
        // If no source-side clock progress occurred after detachment, restore
        // the complete source representation directly.  Reconstructing from
        // the migration snapshot would materialize lag at the detached clock
        // and can change the entity/key even though nothing intervened.
        let (next_clock, entity, key, eligible_at) = if self.clock == metadata.detached_clock {
            (
                metadata.source_clock,
                metadata.source_entity,
                metadata.source_key,
                metadata.source_eligible_at,
            )
        } else if let Some(snapshot) = metadata.snapshot {
            let mut next_clock = self.clock;
            if let Err(kind) = next_clock
                .checked_add_weight(snapshot.weight)
                .map_err(model_error)
            {
                return Err(Self::migration_error(kind, migration));
            }
            let entity = match Entity::from_migration(snapshot, next_clock.v).map_err(model_error) {
                Ok(entity) => entity,
                Err(kind) => return Err(Self::migration_error(kind, migration)),
            };
            let (key, eligible_at) = match Self::fair_key(&entity, metadata.source_key.sequence()) {
                Ok(value) => value,
                Err(kind) => return Err(Self::migration_error(kind, migration)),
            };
            (next_clock, Some(entity), key, eligible_at)
        } else {
            (
                self.clock,
                None,
                metadata.source_key,
                metadata.source_eligible_at,
            )
        };
        if let Err(kind) = self.insert_staged(
            &task,
            key,
            eligible_at,
            metadata.source_key,
            metadata.source_eligible_at,
        ) {
            return Err(Self::migration_error(kind, migration));
        }
        let renew_rr = matches!(metadata.params.class, EevdfTaskClass::RoundRobin)
            && matches!(metadata.origin, EevdfMigrationOrigin::Running)
            && metadata.rr_remaining == 0;
        task.set_rr_remaining(if renew_rr {
            RR_TIMESLICE_TICKS
        } else {
            metadata.rr_remaining
        });
        // SAFETY: CONFIGURING excludes all task-state access.
        unsafe {
            let state = task.owned_state_mut();
            state.entity = entity;
            state.runtime_remainder_ns = metadata.runtime_remainder_ns;
            state.runtime_fraction_remainder_ns = metadata.runtime_fraction_remainder_ns;
            state.runtime_fraction_period_ns = metadata.runtime_fraction_period_ns;
            state.runtime_fraction_class = metadata.runtime_fraction_class;
            state.runtime_fraction_weight = metadata.runtime_fraction_weight;
            state.rr_fraction_q = if renew_rr { 0 } else { metadata.rr_fraction_q };
            state.dormant_fair = metadata.dormant_fair;
            state.migration = None;
            state.rt_sleeping = false;
            state.fork_seed = None;
            state.sleep_v = None;
        }
        task.transfer_owner(CONFIGURING, self.id)
            .expect("EEVDF migration owner changed before ready rollback");
        self.clock = next_clock;
        Ok(migration
            .task
            .take()
            .expect("live EEVDF migration always owns its task"))
    }

    /// Resume a parked migration directly on this destination, returning only
    /// its scheduler error for the base-task handoff boundary.
    pub fn enqueue_migrated_task(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
    ) -> Result<Arc<EEVDFTask<T>>, SchedulerError> {
        let token = self.resume_migration(task)?;
        if token.source_scheduler_id() == self.id {
            self.rollback_migration(token).map_err(|error| error.kind())
        } else {
            self.commit_migration(token).map_err(|error| error.kind())
        }
    }

    fn active_reconfigured_state(
        &self,
        old_state: EevdfOwnedState,
        old_params: EevdfTaskParams,
        new_params: EevdfTaskParams,
        next_total_weight: u128,
        clock_v: i128,
    ) -> Result<(Option<Entity>, Option<Entity>), SchedulerError> {
        let old_fair = !is_rt_class(old_params.class);
        let new_fair = !is_rt_class(new_params.class);
        if old_fair && old_state.entity.is_none() {
            return Err(SchedulerError::InconsistentState);
        }
        if old_fair && old_state.entity.is_some_and(|entity| entity.is_sleeping()) {
            return Err(SchedulerError::InconsistentState);
        }
        match (old_fair, new_fair) {
            (true, true) => {
                let mut entity = old_state.entity.ok_or(SchedulerError::InconsistentState)?;
                entity
                    .reconfigure(
                        fair_class(new_params.class).expect("fair class conversion failed"),
                        Self::fair_weight(new_params)?,
                        next_total_weight,
                        clock_v,
                    )
                    .map_err(model_error)?;
                Ok((Some(entity), None))
            }
            (true, false) => {
                let mut dormant = old_state.entity.ok_or(SchedulerError::InconsistentState)?;
                dormant.freeze_at(clock_v).map_err(model_error)?;
                Ok((None, Some(dormant)))
            }
            (false, true) => {
                let weight = Self::fair_weight(new_params)?;
                let entity = if let Some(mut dormant) = old_state.dormant_fair {
                    dormant
                        .reconfigure_frozen(
                            fair_class(new_params.class).expect("fair class conversion failed"),
                            weight,
                            next_total_weight,
                            clock_v,
                        )
                        .map_err(model_error)?;
                    dormant
                } else {
                    Entity::new(
                        fair_class(new_params.class).expect("fair class conversion failed"),
                        weight,
                        next_total_weight,
                        clock_v,
                    )
                    .map_err(model_error)?
                };
                Ok((Some(entity), None))
            }
            (false, false) => Ok((None, old_state.dormant_fair)),
        }
    }

    fn sleeping_reconfigured_state(
        &self,
        old_state: EevdfOwnedState,
        old_params: EevdfTaskParams,
        new_params: EevdfTaskParams,
    ) -> Result<(Option<Entity>, Option<Entity>, bool), SchedulerError> {
        let anchor = old_state.sleep_v.unwrap_or(self.clock.v);
        let old_fair = !is_rt_class(old_params.class);
        let new_fair = !is_rt_class(new_params.class);
        match (old_fair, new_fair) {
            (true, true) => {
                let mut entity = old_state.entity.ok_or(SchedulerError::InconsistentState)?;
                let weight = Self::fair_weight(new_params)?;
                let total_weight = self
                    .clock
                    .total_weight
                    .checked_add(weight)
                    .ok_or(SchedulerError::ArithmeticExhausted)?;
                entity
                    .reconfigure_sleeping(
                        fair_class(new_params.class).expect("fair class conversion failed"),
                        weight,
                        total_weight,
                    )
                    .map_err(model_error)?;
                Ok((Some(entity), None, false))
            }
            (true, false) => Ok((None, old_state.entity, true)),
            (false, true) => {
                let weight = Self::fair_weight(new_params)?;
                let total_weight = self
                    .clock
                    .total_weight
                    .checked_add(weight)
                    .ok_or(SchedulerError::ArithmeticExhausted)?;
                let mut entity = if let Some(mut dormant) = old_state.dormant_fair {
                    if !dormant.is_sleeping() {
                        dormant.begin_sleep_frozen(anchor).map_err(model_error)?;
                    }
                    dormant
                        .reconfigure_sleeping(
                            fair_class(new_params.class).expect("fair class conversion failed"),
                            weight,
                            total_weight,
                        )
                        .map_err(model_error)?;
                    dormant
                } else {
                    let mut fresh = Entity::new(
                        fair_class(new_params.class).expect("fair class conversion failed"),
                        weight,
                        total_weight,
                        anchor,
                    )
                    .map_err(model_error)?;
                    fresh.begin_sleep(anchor).map_err(model_error)?;
                    fresh
                };
                if !entity.is_sleeping() {
                    entity.begin_sleep(anchor).map_err(model_error)?;
                }
                Ok((Some(entity), None, false))
            }
            (false, false) => Ok((None, old_state.dormant_fair, true)),
        }
    }

    fn next_rr_budget(
        old_params: EevdfTaskParams,
        new_params: EevdfTaskParams,
        old_remaining: usize,
    ) -> usize {
        if !matches!(new_params.class, EevdfTaskClass::RoundRobin) {
            0
        } else if matches!(old_params.class, EevdfTaskClass::RoundRobin) {
            old_remaining
        } else {
            RR_TIMESLICE_TICKS
        }
    }

    /// Update a task's scheduling tuple across virgin, sleeping, ready, and
    /// running states.  Fair request progress and dormant fair debt are kept
    /// in the model; RT/RR budgets follow class semantics.
    pub fn set_task_params(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
        params: EevdfTaskParams,
    ) -> Result<EevdfParamUpdate<T>, SchedulerError> {
        self.set_task_params_impl(task, params, false)
    }

    fn set_task_params_impl(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
        params: EevdfTaskParams,
        // A runtime-aware boundary has already settled all wall-clock service
        // under the old tuple. Any remaining conversion residue belongs to
        // that old class/weight and is cleared at a changed tuple boundary;
        // it must never be reinterpreted by the new publication.
        allow_settled_fraction: bool,
    ) -> Result<EevdfParamUpdate<T>, SchedulerError> {
        let params = params
            .validated()
            .ok_or(SchedulerError::InvalidParameters)?;
        match task.owner() {
            CONFIGURING => Err(SchedulerError::TaskBusy),
            UNOWNED => {
                // Claim CONFIGURING before reading the state slot.  In
                // particular, a parked migration is represented in the same
                // UNOWNED owner word and must not be inspected locklessly.
                let claim = task.value().claim_reconfiguration()?;
                let state = unsafe { *task.owned_state() };
                if state.migration.is_some() {
                    drop(claim);
                    return Err(SchedulerError::TaskBusy);
                }
                if state.runtime_remainder_ns != 0
                    || (!allow_settled_fraction && state.runtime_fraction_remainder_ns != 0)
                {
                    drop(claim);
                    return Err(SchedulerError::InconsistentState);
                }
                let old_params = task.sched_params();
                Self::validate_runtime_fraction_owner(task, state)?;
                let old_fraction_weight = Self::runtime_fraction_weight(old_params)?;
                let new_fraction_weight = Self::runtime_fraction_weight(params)?;
                let clear_fraction = state.runtime_fraction_remainder_ns != 0
                    && (old_params.class != params.class
                        || old_fraction_weight != new_fraction_weight);
                if state.entity.is_none() && !state.rt_sleeping && state.dormant_fair.is_none() {
                    // A fork seed is fair-only metadata.  Reconfiguration is
                    // allowed before admission, so moving a seeded child to
                    // RT must clear the seed atomically with publication.
                    unsafe {
                        let next = task.owned_state_mut();
                        if is_rt_class(params.class) {
                            next.fork_seed = None;
                        }
                        if clear_fraction {
                            Self::clear_runtime_fraction(next);
                        }
                    }
                    task.value().apply_validated(params);
                    return claim.finish().map(|_| EevdfParamUpdate::NoPreemption);
                }
                let (entity, dormant, rt_sleeping) =
                    self.sleeping_reconfigured_state(state, old_params, params)?;
                // SAFETY: CONFIGURING claim excludes all task-state access.
                unsafe {
                    let next = task.owned_state_mut();
                    next.entity = entity;
                    next.rr_fraction_q = if matches!(old_params.class, EevdfTaskClass::RoundRobin)
                        && matches!(params.class, EevdfTaskClass::RoundRobin)
                    {
                        state.rr_fraction_q
                    } else {
                        0
                    };
                    next.dormant_fair = dormant;
                    next.rt_sleeping = rt_sleeping;
                    next.sleep_v = state.sleep_v;
                    if clear_fraction {
                        Self::clear_runtime_fraction(next);
                    }
                    task.value().publish_validated(
                        params,
                        Self::next_rr_budget(old_params, params, task.rr_remaining()),
                    );
                }
                claim.finish().map(|_| EevdfParamUpdate::NoPreemption)
            }
            owner if owner != self.id || self.id == UNOWNED => Err(SchedulerError::ForeignQueue),
            _ => {
                let old_params = task.sched_params();
                let old_state = unsafe { *task.owned_state() };
                if old_state.runtime_remainder_ns != 0
                    || (!allow_settled_fraction && old_state.runtime_fraction_remainder_ns != 0)
                {
                    return Err(SchedulerError::InconsistentState);
                }
                Self::validate_runtime_fraction_owner(task, old_state)?;
                let old_fraction_weight = Self::runtime_fraction_weight(old_params)?;
                let new_fraction_weight = Self::runtime_fraction_weight(params)?;
                let clear_fraction = old_state.runtime_fraction_remainder_ns != 0
                    && (old_params.class != params.class
                        || old_fraction_weight != new_fraction_weight);
                let running = self
                    .running
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, task));
                if !running && !task.is_linked() {
                    return Err(SchedulerError::InconsistentState);
                }
                let old_fair = !is_rt_class(old_params.class);
                let new_fair = !is_rt_class(params.class);
                let old_weight = if old_fair {
                    Some(Self::fair_weight(old_params)?)
                } else {
                    None
                };
                let new_weight = if new_fair {
                    Some(Self::fair_weight(params)?)
                } else {
                    None
                };
                // Compute the final aggregate once.  Applying a subtraction
                // and addition separately can round the clock residue twice;
                // equal-weight reconfiguration must be an exact clock no-op.
                let mut next_clock = self.clock;
                let mut next_total_weight = self.clock.total_weight;
                if let Some(weight) = old_weight {
                    next_total_weight = next_total_weight
                        .checked_sub(weight)
                        .ok_or(SchedulerError::InconsistentState)?;
                }
                if let Some(weight) = new_weight {
                    next_total_weight = next_total_weight
                        .checked_add(weight)
                        .ok_or(SchedulerError::ArithmeticExhausted)?;
                }
                if next_total_weight != self.clock.total_weight {
                    next_clock
                        .set_total_weight(next_total_weight)
                        .map_err(model_error)?;
                }
                let (entity, dormant) = self.active_reconfigured_state(
                    old_state,
                    old_params,
                    params,
                    next_clock.total_weight,
                    next_clock.v,
                )?;
                let next_rr = Self::next_rr_budget(old_params, params, task.rr_remaining());
                // Reserve the replacement key before unlinking.  Every
                // fallible sequence, deadline, and eligibility calculation
                // belongs to this staged phase so an arithmetic failure
                // cannot strand the exact ready node.
                let replacement = if !running {
                    let sequence = if new_fair {
                        self.peek_fair_sequence()?
                    } else {
                        self.peek_rt_sequence(false)?
                    };
                    Some(if let Some(entity) = entity.as_ref() {
                        Self::fair_key(entity, sequence)?
                    } else {
                        (Self::rt_key(params, sequence), 0)
                    })
                } else {
                    None
                };
                let (removed, old_key, old_eligible_at, sequence_kind) = if running {
                    (
                        None,
                        unsafe { task.key() },
                        unsafe { task.eligible_at() },
                        None,
                    )
                } else {
                    let key = unsafe { task.key() };
                    let eligible_at = unsafe { task.eligible_at() };
                    let removed = self.ready_tree.remove(task).map_err(tree_error)?;
                    let seq_kind = if new_fair { Some(true) } else { Some(false) };
                    (Some(removed), key, eligible_at, seq_kind)
                };
                if let Some(_) = sequence_kind {
                    let (key, eligible_at) =
                        replacement.expect("ready replacement key missing after preflight");
                    if let Err(error) =
                        self.insert_staged(task, key, eligible_at, old_key, old_eligible_at)
                    {
                        // `insert_staged` restored the exact old key.  The
                        // removed node must itself be restored before the
                        // error leaves the scheduler.
                        let removed = removed.expect("ready task disappeared during reconfigure");
                        self.ready_tree
                            .insert(removed)
                            .expect("EEVDF ready rollback encountered a duplicate old key");
                        return Err(error);
                    }
                    if new_fair {
                        self.commit_fair_sequence();
                    } else {
                        self.commit_rt_sequence(false);
                    }
                }
                // SAFETY: this scheduler owns the task and either keeps it
                // running or has reinserted its ready node.
                unsafe {
                    let next = task.owned_state_mut();
                    next.entity = entity;
                    next.rr_fraction_q = if matches!(old_params.class, EevdfTaskClass::RoundRobin)
                        && matches!(params.class, EevdfTaskClass::RoundRobin)
                    {
                        old_state.rr_fraction_q
                    } else {
                        0
                    };
                    next.dormant_fair = dormant;
                    next.rt_sleeping = false;
                    next.migration = None;
                    next.sleep_v = None;
                    if clear_fraction {
                        Self::clear_runtime_fraction(next);
                    }
                }
                task.value().publish_validated(params, next_rr);
                self.clock = next_clock;
                drop(removed);
                Ok(self.post_update_outcome())
            }
        }
    }

    /// Set parameters after materializing the current owner's pending
    /// sub-period service under the old tuple.  This boundary is intentionally
    /// separate from the legacy no-runtime API so model-only callers can keep
    /// their existing transaction shape.
    pub fn set_task_params_with_runtime(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
        params: EevdfTaskParams,
        period_ns: u64,
    ) -> Result<EevdfParamUpdate<T>, SchedulerError> {
        if params.validated().is_none() {
            return Err(SchedulerError::InvalidParameters);
        }
        let running = self
            .running
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, task));
        if running {
            let before_clock = self.clock;
            let before_state = unsafe { *task.owned_state() };
            let before_rr = task.rr_remaining();
            self.settle_runtime_remainder(task, period_ns)?;
            let result = self.set_task_params_impl(task, params, true);
            if result.is_err() {
                // Runtime settlement and parameter publication form one
                // boundary transaction. A failed replacement key/weight
                // calculation must not leave old-tuple service committed
                // while the old tuple remains published.
                self.clock = before_clock;
                unsafe { *task.owned_state_mut() = before_state };
                task.set_rr_remaining(before_rr);
            }
            return result;
        }
        // Non-running tasks are validated and claimed by the ordinary
        // parameter transaction below.  Never inspect their UnsafeCell state
        // through an UNOWNED/CONFIGURING race here.
        self.set_task_params_impl(task, params, true)
    }

    pub fn set_priority(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
        prio: isize,
    ) -> Result<EevdfParamUpdate<T>, SchedulerError> {
        self.set_priority_impl(task, prio, false)
    }

    fn set_priority_impl(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
        prio: isize,
        allow_settled_fraction: bool,
    ) -> Result<EevdfParamUpdate<T>, SchedulerError> {
        if !(-20..=19).contains(&prio) {
            return Err(SchedulerError::InvalidParameters);
        }
        match task.owner() {
            CONFIGURING => return Err(SchedulerError::TaskBusy),
            owner if owner != UNOWNED && (self.id == UNOWNED || owner != self.id) => {
                return Err(SchedulerError::ForeignQueue);
            }
            _ => {}
        }
        let current = task.sched_params();
        if is_rt_class(current.class) {
            return Err(SchedulerError::IncompatibleClass);
        }
        self.set_task_params_impl(
            task,
            EevdfTaskParams {
                class: current.class,
                nice: prio as i8,
                rt_priority: 0,
            },
            allow_settled_fraction,
        )
    }

    pub fn set_priority_with_runtime(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
        prio: isize,
        period_ns: u64,
    ) -> Result<EevdfParamUpdate<T>, SchedulerError> {
        if !(-20..=19).contains(&prio) {
            return Err(SchedulerError::InvalidParameters);
        }
        let running = self
            .running
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, task));
        if running {
            let before_clock = self.clock;
            let before_state = unsafe { *task.owned_state() };
            let before_rr = task.rr_remaining();
            self.settle_runtime_remainder(task, period_ns)?;
            let result = self.set_priority_impl(task, prio, true);
            if result.is_err() {
                self.clock = before_clock;
                unsafe { *task.owned_state_mut() = before_state };
                task.set_rr_remaining(before_rr);
            }
            return result;
        }
        self.set_priority_impl(task, prio, true)
    }

    fn enqueue_wakeup(&mut self, task: Arc<EEVDFTask<T>>) -> Result<(), SchedulerError> {
        let scheduler_id = self.ensure_id()?;
        if task.owner() != UNOWNED {
            return if task.owner() == scheduler_id {
                Err(SchedulerError::AlreadyQueued)
            } else {
                Err(SchedulerError::ForeignQueue)
            };
        }
        task.claim(scheduler_id)?;
        // The scheduler claim is acquired before any state-slot inspection;
        // parked migrations share the UNOWNED owner word with sleepers.
        if unsafe { task.owned_state().migration.is_some() } {
            self.release_claim(&task);
            return Err(SchedulerError::TaskBusy);
        }
        if self
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, &task))
            || task.is_linked()
        {
            self.release_claim(&task);
            return Err(SchedulerError::AlreadyQueued);
        }

        let params = task.sched_params();
        let old_key = unsafe { task.key() };
        let old_eligible_at = unsafe { task.eligible_at() };
        let old_entity = unsafe { task.owned_state().entity };
        let old_dormant_fair = unsafe { task.owned_state().dormant_fair };
        let old_rt_sleeping = unsafe { task.owned_state().rt_sleeping };
        let (next_clock, entity, dormant_fair, key, eligible_at, reset_rr) = if let Some(entity) =
            old_entity
        {
            if !entity.is_sleeping() || old_rt_sleeping {
                self.release_claim(&task);
                return Err(SchedulerError::InconsistentState);
            }
            let weight = match Self::fair_weight(params) {
                Ok(weight) => weight,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            // A sleeping fair entity is absent from Clock.total_weight.  Add
            // it to a local final clock before reconfiguring its request.
            let mut next_clock = self.clock;
            if let Err(error) = next_clock.checked_add_weight(weight).map_err(model_error) {
                self.release_claim(&task);
                return Err(error);
            }
            let mut next = entity;
            if let Err(error) = next
                .wake_preserving_progress(self.clock.v)
                .map_err(model_error)
            {
                self.release_claim(&task);
                return Err(error);
            }
            if next_clock.v != self.clock.v {
                if let Err(error) = next
                    .activate_preserving_progress(next_clock.v)
                    .map_err(model_error)
                {
                    self.release_claim(&task);
                    return Err(error);
                }
            }
            let class = match fair_class(params.class) {
                Some(class) => class,
                None => {
                    self.release_claim(&task);
                    return Err(SchedulerError::IncompatibleClass);
                }
            };
            if next.class != class || next.weight != weight {
                self.release_claim(&task);
                return Err(SchedulerError::InconsistentState);
            }
            let sequence = match self.next_fair_sequence() {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            let (key, eligible_at) = match Self::fair_key(&next, sequence) {
                Ok(key) => key,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            (
                next_clock,
                Some(next),
                old_dormant_fair,
                key,
                eligible_at,
                false,
            )
        } else if is_rt_class(params.class) && old_rt_sleeping {
            let dormant_fair = match old_dormant_fair {
                Some(mut dormant) => {
                    if let Err(error) = dormant.end_sleep_frozen(self.clock.v).map_err(model_error)
                    {
                        self.release_claim(&task);
                        return Err(error);
                    }
                    Some(dormant)
                }
                None => None,
            };
            let sequence = match self.next_rt_sequence(false) {
                Ok(sequence) => sequence,
                Err(error) => {
                    self.release_claim(&task);
                    return Err(error);
                }
            };
            (
                self.clock,
                None,
                dormant_fair,
                Self::rt_key(params, sequence),
                0,
                matches!(params.class, EevdfTaskClass::RoundRobin),
            )
        } else {
            self.release_claim(&task);
            return Err(SchedulerError::InconsistentState);
        };

        if let Err(error) = self.insert_staged(&task, key, eligible_at, old_key, old_eligible_at) {
            self.release_claim(&task);
            return Err(error);
        }
        if reset_rr {
            Self::reset_rr_slice(&task);
        }
        // SAFETY: owner and tree exclusion are held for the complete state
        // publication.
        unsafe {
            task.owned_state_mut().entity = entity;
            task.owned_state_mut().dormant_fair = dormant_fair;
            task.owned_state_mut().rt_sleeping = false;
            task.owned_state_mut().sleep_v = None;
        }
        if matches!(params.class, EevdfTaskClass::RoundRobin) {
            Self::reset_rr_slice(&task);
        }
        self.clock = next_clock;
        Ok(())
    }

    fn validate_running(&self, task: &Arc<EEVDFTask<T>>) -> Result<(), SchedulerError> {
        if self.id == UNOWNED || task.owner() != self.id {
            return Err(SchedulerError::ForeignQueue);
        }
        if !self
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, task))
        {
            return Err(SchedulerError::InconsistentState);
        }
        Ok(())
    }

    /// Adopt an axtask-style initial current task when it is still virgin.
    /// Existing state or a foreign owner is never silently overwritten.
    fn ensure_running(&mut self, task: &Arc<EEVDFTask<T>>) -> Result<(), SchedulerError> {
        if self.running.is_some() {
            return self.validate_running(task);
        }
        if task.owner() != UNOWNED {
            return Err(if task.owner() == self.id {
                SchedulerError::InconsistentState
            } else {
                SchedulerError::ForeignQueue
            });
        }
        let scheduler_id = self.ensure_id()?;
        task.claim(scheduler_id)?;
        // SAFETY: the owner claim supplies exclusive state access.
        if !unsafe { task.is_virgin() } {
            self.release_claim(task);
            return Err(SchedulerError::InconsistentState);
        }

        let params = task.sched_params();
        let fork_seed = unsafe { task.owned_state().fork_seed };
        if is_rt_class(params.class) && fork_seed.is_some() {
            self.release_claim(task);
            return Err(SchedulerError::IncompatibleClass);
        }
        let (next_clock, entity) = if is_rt_class(params.class) {
            (self.clock, None)
        } else {
            let weight = match Self::fair_weight(params) {
                Ok(weight) => weight,
                Err(error) => {
                    self.release_claim(task);
                    return Err(error);
                }
            };
            let mut next_clock = self.clock;
            if let Err(error) = next_clock.checked_add_weight(weight).map_err(model_error) {
                self.release_claim(task);
                return Err(error);
            }
            let entity = match Entity::with_lag(
                fair_class(params.class).expect("fair class conversion failed"),
                weight,
                next_clock.total_weight,
                next_clock.v,
                fork_lag_for_class(
                    fork_seed,
                    fair_class(params.class).expect("fair class conversion failed"),
                ),
            )
            .map_err(model_error)
            {
                Ok(entity) => entity,
                Err(error) => {
                    self.release_claim(task);
                    return Err(error);
                }
            };
            (next_clock, Some(entity))
        };
        // SAFETY: the task remains owned by this scheduler while publishing
        // its state and installing the running Arc clone.
        unsafe {
            task.owned_state_mut().entity = entity;
            task.owned_state_mut().migration = None;
            task.owned_state_mut().dormant_fair = None;
            task.owned_state_mut().rt_sleeping = false;
            task.owned_state_mut().fork_seed = None;
            task.owned_state_mut().sleep_v = None;
        }
        if matches!(params.class, EevdfTaskClass::RoundRobin) {
            Self::reset_rr_slice(task);
        }
        self.clock = next_clock;
        self.running = Some(Arc::clone(task));
        Ok(())
    }

    fn put_running(
        &mut self,
        prev: Arc<EEVDFTask<T>>,
        reason: EnqueueReason,
    ) -> Result<(), SchedulerError> {
        // The first current task in axtask may be virgin and never have gone
        // through New.  Adopt it before applying Yield/Preempt semantics.
        self.ensure_running(&prev)?;
        // A direct scheduler caller may hand the current task back without
        // first going through the task-layer runtime hook.  Materialize that
        // token at the ownership boundary so Yield/Preempt cannot defer old
        // service into the next request or RR slice.
        self.settle_before_lifecycle(&prev)?;
        let params = prev.sched_params();
        let old_key = unsafe { prev.key() };
        let old_eligible_at = unsafe { prev.eligible_at() };
        let (next_entity, key, eligible_at, reset_rr) = if let Some(entity) =
            unsafe { prev.owned_state().entity }
        {
            let mut next = entity;
            match reason {
                EnqueueReason::Yield => next
                    .yield_request(self.clock.total_weight, self.clock.v)
                    .map_err(model_error)?,
                EnqueueReason::Preempt => {
                    if next.request.remaining() == 0 {
                        next.renew(self.clock.total_weight, self.clock.v)
                            .map_err(model_error)?;
                    } else {
                        next.preempt_request().map_err(model_error)?;
                    }
                }
                _ => return Err(SchedulerError::InconsistentState),
            }
            let sequence = self.next_fair_sequence()?;
            let (key, eligible_at) = Self::fair_key(&next, sequence)?;
            (Some(next), key, eligible_at, false)
        } else if is_rt_class(params.class) {
            let (front, reset_rr) = match (params.class, reason) {
                (EevdfTaskClass::Fifo, EnqueueReason::Preempt) => (true, false),
                (EevdfTaskClass::Fifo, EnqueueReason::Yield) => (false, false),
                (EevdfTaskClass::RoundRobin, EnqueueReason::Yield) => (false, true),
                (EevdfTaskClass::RoundRobin, EnqueueReason::Preempt) if prev.rr_remaining() > 0 => {
                    (true, false)
                }
                (EevdfTaskClass::RoundRobin, EnqueueReason::Preempt) => (false, true),
                _ => return Err(SchedulerError::IncompatibleClass),
            };
            let sequence = self.next_rt_sequence(front)?;
            (None, Self::rt_key(params, sequence), 0, reset_rr)
        } else {
            return Err(SchedulerError::InconsistentState);
        };

        if let Err(error) = self.insert_staged(&prev, key, eligible_at, old_key, old_eligible_at) {
            return Err(error);
        }
        if reset_rr {
            Self::reset_rr_slice(&prev);
        }
        // SAFETY: the task is still owned by this scheduler and is now
        // linked in its one ready tree.
        unsafe {
            prev.owned_state_mut().entity = next_entity;
            prev.owned_state_mut().rt_sleeping = false;
            prev.owned_state_mut().migration = None;
            prev.owned_state_mut().sleep_v = None;
        }
        self.running = None;
        Ok(())
    }

    fn remove_ready(
        &mut self,
        task: &Arc<EEVDFTask<T>>,
    ) -> Result<Option<Arc<EEVDFTask<T>>>, SchedulerError> {
        match task.owner() {
            UNOWNED => return Ok(None),
            owner if owner != self.id || self.id == UNOWNED => {
                return Err(SchedulerError::ForeignQueue);
            }
            _ => {}
        }
        if self
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, task))
        {
            return Err(SchedulerError::InconsistentState);
        }
        if !task.is_linked() {
            return Err(SchedulerError::InconsistentState);
        }

        let fair = unsafe { task.owned_state().entity }.is_some();
        let next_clock = if fair {
            let weight = Self::fair_weight(task.sched_params())?;
            let mut next = self.clock;
            next.checked_sub_weight(weight).map_err(model_error)?;
            Some(next)
        } else {
            None
        };
        let removed = self.ready_tree.remove(task).map_err(tree_error)?;
        if let Some(next_clock) = next_clock {
            self.clock = next_clock;
        }
        // SAFETY: removal returned the exact node and ownership remains held
        // by this scheduler until the owner word is released below.
        unsafe {
            removed.owned_state_mut().entity = None;
            removed.owned_state_mut().runtime_remainder_ns = 0;
            removed.owned_state_mut().runtime_fraction_remainder_ns = 0;
            removed.owned_state_mut().runtime_fraction_period_ns = 0;
            removed.owned_state_mut().runtime_fraction_class = EevdfTaskClass::Normal;
            removed.owned_state_mut().runtime_fraction_weight = 0;
            removed.owned_state_mut().rr_fraction_q = 0;
            removed.owned_state_mut().migration = None;
            removed.owned_state_mut().dormant_fair = None;
            removed.owned_state_mut().rt_sleeping = false;
            removed.owned_state_mut().fork_seed = None;
            removed.owned_state_mut().sleep_v = None;
        }
        removed
            .transfer_owner(self.id, UNOWNED)
            .expect("EEVDF ready task owner invariant violated during removal");
        Ok(Some(removed))
    }

    fn has_ready_rt(&self) -> bool {
        self.ready_tree
            .front()
            .is_some_and(|task| unsafe { task.key().class_rank() == RT_CLASS_RANK })
    }

    fn has_higher_rt(&self, priority: u8) -> bool {
        self.ready_tree.front().is_some_and(|task| unsafe {
            let key = task.key();
            key.class_rank() == RT_CLASS_RANK && key.order() < rt_order(priority)
        })
    }

    fn has_same_rt(&self, priority: u8) -> bool {
        self.ready_tree.front().is_some_and(|task| unsafe {
            let key = task.key();
            key.class_rank() == RT_CLASS_RANK && key.order() == rt_order(priority)
        })
    }

    fn has_earlier_eligible_fair(&self, deadline: i128) -> bool {
        self.ready_tree
            .peek_earliest_eligible(bias_i128(self.clock.v))
            .is_some_and(|task| unsafe {
                let key = task.key();
                key.class_rank() == FAIR_CLASS_RANK && key.order() < bias_i128(deadline)
            })
    }

    /// Query the post-update relationship between this scheduler's current
    /// task and its ready set.  This helper is deliberately infallible and
    /// read-only: parameter publication has already committed before it is
    /// called, so no arithmetic error can turn a successful update into an
    /// error or require a second publication transaction.
    fn post_update_outcome(&self) -> EevdfParamUpdate<T> {
        let Some(current) = self.running.as_ref() else {
            return EevdfParamUpdate::NoPreemption;
        };
        let params = current.sched_params();
        let should_preempt = if is_rt_class(params.class) {
            self.has_higher_rt(params.rt_priority)
        } else {
            let Some(entity) = (unsafe { current.owned_state().entity }) else {
                return EevdfParamUpdate::NoPreemption;
            };
            if self.has_ready_rt() {
                true
            } else if entity
                .is_eligible(self.clock.v)
                .expect("EEVDF current eligibility invariant failed after parameter update")
            {
                self.has_earlier_eligible_fair(entity.request.deadline)
            } else {
                self.ready_tree
                    .peek_earliest_eligible(bias_i128(self.clock.v))
                    .is_some()
            }
        };
        if should_preempt {
            EevdfParamUpdate::PreemptCurrent(Arc::clone(current))
        } else {
            EevdfParamUpdate::NoPreemption
        }
    }

    fn pick_rt_front(&mut self) -> Option<Arc<EEVDFTask<T>>> {
        let ptr = self.ready_tree.front().map(|task| task as *const _)?;
        // SAFETY: the pointer was obtained from this tree and remains linked
        // until this exclusive removal.
        Some(
            self.ready_tree
                .remove(unsafe { &*ptr })
                .expect("EEVDF ready tree lost its front task"),
        )
    }

    fn pick_ready(&mut self) -> Option<Arc<EEVDFTask<T>>> {
        if self.has_ready_rt() {
            return self.pick_rt_front();
        }
        let now = bias_i128(self.clock.v);
        if self.ready_tree.peek_earliest_eligible(now).is_none() {
            let min = self.ready_tree.min_eligible_at()?;
            let next_v = crate::eevdf_model::unbias_i128(min);
            if next_v > self.clock.v {
                self.clock
                    .jump_to(next_v)
                    .expect("EEVDF eligibility cache moved clock backwards");
            }
        }
        self.ready_tree
            .pop_earliest_eligible(bias_i128(self.clock.v))
    }

    fn tick_fair_bulk(&mut self, current: &Arc<EEVDFTask<T>>, ticks: u128) -> bool {
        self.fair_service_transaction(current, ticks, 0)
            .expect("EEVDF infallible bulk service exhausted arithmetic")
    }

    fn tick_fair(&mut self, current: &Arc<EEVDFTask<T>>) -> bool {
        self.tick_fair_bulk(current, 1)
    }

    /// Apply one staged fair-service sample.  The entity and queue clock are
    /// copied first, including any request renewal required at an ownership
    /// boundary; only the final checked state is published.
    fn fair_service_transaction(
        &mut self,
        current: &Arc<EEVDFTask<T>>,
        ticks: u128,
        fraction_q: u64,
    ) -> Result<bool, SchedulerError> {
        if ticks == 0 && fraction_q == 0 {
            return Ok(false);
        }
        let entity = unsafe {
            current
                .owned_state()
                .entity
                .ok_or(SchedulerError::InconsistentState)?
        };
        let mut next_clock = self.clock;
        let mut next_entity = entity;
        let mut reschedule = false;
        if next_entity.request.is_exhausted() && !self.ready_tree.is_empty() {
            next_entity
                .service(&mut next_clock, ticks, u128::from(fraction_q))
                .map_err(model_error)?;
            reschedule = true;
        } else {
            if next_entity.request.is_exhausted() {
                next_entity
                    .renew(next_clock.total_weight, next_clock.v)
                    .map_err(model_error)?;
            }
            next_entity
                .service(&mut next_clock, ticks, u128::from(fraction_q))
                .map_err(model_error)?;
        }

        if self.has_ready_rt() {
            reschedule = true;
        } else {
            let current_eligible = next_entity.is_eligible(next_clock.v).map_err(model_error)?;
            if current_eligible {
                if self.has_earlier_eligible_fair(next_entity.request.deadline) {
                    reschedule = true;
                }
            } else if self
                .ready_tree
                .peek_earliest_eligible(bias_i128(next_clock.v))
                .is_some()
            {
                reschedule = true;
            }
        }
        if next_entity.request.is_exhausted() {
            if !self.ready_tree.is_empty() {
                reschedule = true;
            } else {
                next_entity
                    .renew(next_clock.total_weight, next_clock.v)
                    .map_err(model_error)?;
            }
        }

        self.clock = next_clock;
        unsafe { current.owned_state_mut().entity = Some(next_entity) };
        Ok(reschedule)
    }

    /// Apply whole and Q32 service to a round-robin budget as one staged
    /// transaction.  `rr_fraction_q` is progress within the current integer
    /// tick; combining it with a later whole-tick sample is necessary to
    /// avoid either losing the fractional service or charging a renewed slice
    /// twice.
    fn rr_service_transaction(
        &mut self,
        current: &Arc<EEVDFTask<T>>,
        ticks: u128,
        fraction_q: u64,
    ) -> Result<bool, SchedulerError> {
        if ticks == 0 && fraction_q == 0 {
            return Ok(false);
        }
        let params = current.sched_params();
        if !matches!(params.class, EevdfTaskClass::RoundRobin) {
            return Ok(self.has_higher_rt(params.rt_priority));
        }
        let state = unsafe { *current.owned_state() };
        let old_fraction = u128::from(state.rr_fraction_q);
        if old_fraction >= ONE {
            return Err(SchedulerError::InconsistentState);
        }
        let fraction_total = old_fraction
            .checked_add(u128::from(fraction_q))
            .ok_or(SchedulerError::ArithmeticExhausted)?;
        let carried_ticks = fraction_total / ONE;
        let next_fraction = (fraction_total % ONE) as u64;
        let service_ticks = ticks
            .checked_add(carried_ticks)
            .ok_or(SchedulerError::ArithmeticExhausted)?;
        let before = current.rr_remaining() as u128;
        let higher = self.has_higher_rt(params.rt_priority);
        let same = self.has_same_rt(params.rt_priority);
        let mut reschedule = higher;

        let (next_remaining, next_rr_fraction) = if higher {
            // Higher-priority RT work preempts the current owner, but service
            // already observed by this boundary is still accounted.  Keep a
            // non-exhausted budget for the eventual resume and discard only
            // the exhausted boundary below.
            if before == 0 || service_ticks >= before {
                reschedule = true;
                (0, 0)
            } else {
                ((before - service_ticks) as usize, next_fraction)
            }
        } else if before == 0 {
            // A zero budget is the published exhausted marker after a peer
            // boundary.  The first subsequent sample renews the slice; its
            // first whole tick is the renewal event itself, matching the
            // existing RR handoff semantics.
            if same {
                reschedule = true;
                (0, 0)
            } else {
                let consumed_after_renewal = service_ticks.saturating_sub(1);
                let slice = RR_TIMESLICE_TICKS as u128;
                let within = consumed_after_renewal % slice;
                let remaining = if within == 0 { slice } else { slice - within };
                (remaining as usize, next_fraction)
            }
        } else if service_ticks < before {
            ((before - service_ticks) as usize, next_fraction)
        } else if same {
            // Equal-priority work must observe an exhausted budget at the
            // boundary, including a boundary reached solely by Q32 service.
            reschedule = true;
            (0, 0)
        } else {
            // No peer is waiting.  Carry any overshoot into a fresh slice in
            // constant time; the fractional part belongs to that fresh slice
            // and is not stale progress from the exhausted one.
            let after_boundary = service_ticks - before;
            let slice = RR_TIMESLICE_TICKS as u128;
            let within = after_boundary % slice;
            let remaining = if within == 0 { slice } else { slice - within };
            (remaining as usize, next_fraction)
        };

        current.set_rr_remaining(next_remaining);
        unsafe { current.owned_state_mut().rr_fraction_q = next_rr_fraction };
        Ok(reschedule)
    }

    fn tick_rt_bulk(&mut self, current: &Arc<EEVDFTask<T>>, ticks: u128) -> bool {
        self.rr_service_transaction(current, ticks, 0)
            .expect("EEVDF RT bulk service exhausted arithmetic")
    }

    fn tick_rt(&mut self, current: &Arc<EEVDFTask<T>>) -> bool {
        self.tick_rt_bulk(current, 1)
    }

    /// Materialize a current task's sub-period token using the parameters
    /// that are still published.  This is deliberately local to the owner
    /// transaction: no absolute clock/entity snapshot crosses yield, sleep,
    /// or migration.
    pub fn settle_runtime_remainder(
        &mut self,
        current: &Arc<EEVDFTask<T>>,
        period_ns: u64,
    ) -> Result<bool, SchedulerError> {
        self.ensure_running(current)?;
        if period_ns == 0 {
            return Err(SchedulerError::InconsistentState);
        }
        let state = unsafe { *current.owned_state() };
        if state.runtime_remainder_ns >= period_ns
            || (state.runtime_remainder_ns != 0
                && state.runtime_fraction_period_ns != 0
                && state.runtime_fraction_period_ns != period_ns)
            || (state.runtime_fraction_remainder_ns != 0
                && (state.runtime_fraction_period_ns == 0
                    || state.runtime_fraction_remainder_ns >= state.runtime_fraction_period_ns))
        {
            return Err(SchedulerError::InconsistentState);
        }
        Self::validate_runtime_fraction_owner(current, state)?;
        if state.runtime_fraction_remainder_ns != 0 && state.runtime_fraction_period_ns != period_ns
        {
            // A conversion residue is still runtime in the old tuple.  Do not
            // silently reinterpret it under a new period/class context.
            return Err(SchedulerError::InconsistentState);
        }
        let old_remainder = if state.runtime_fraction_period_ns == period_ns {
            state.runtime_fraction_remainder_ns
        } else {
            0
        };
        let numerator = u128::from(state.runtime_remainder_ns)
            .checked_mul(ONE)
            .and_then(|value| value.checked_add(u128::from(old_remainder)))
            .ok_or(SchedulerError::ArithmeticExhausted)?;
        let fraction_ticks = numerator / u128::from(period_ns);
        if fraction_ticks > ONE {
            return Err(SchedulerError::InconsistentState);
        }
        let conversion_remainder = (numerator % u128::from(period_ns)) as u64;
        let whole = (fraction_ticks / ONE) as u128;
        let fraction_q = (fraction_ticks % ONE) as u64;
        let params = current.sched_params();
        let fraction_weight = Self::runtime_fraction_weight(params)?;
        let reschedule = if is_rt_class(params.class) {
            if matches!(params.class, EevdfTaskClass::RoundRobin) {
                self.rr_service_transaction(current, whole, fraction_q)?
            } else {
                self.has_higher_rt(params.rt_priority)
            }
        } else {
            // `fair_service_transaction` stages the whole and fractional
            // portions together; no clock/entity mutation can precede a later
            // arithmetic failure in the same boundary.
            self.fair_service_transaction(current, whole, fraction_q)?
        };
        unsafe {
            let state = current.owned_state_mut();
            state.runtime_remainder_ns = 0;
            Self::publish_runtime_fraction_owner(
                state,
                params,
                fraction_weight,
                conversion_remainder,
                period_ns,
            );
        }
        Ok(reschedule)
    }

    /// Account an explicit wall-clock sample without consulting a platform
    /// clock.  Sub-period runtime is retained in the task-owned state slot so
    /// yielding twice at half a period is equivalent to one full period of
    /// service.  Bulk service is a single model transaction; a missed timer
    /// boundary therefore cannot turn into an unbounded per-tick operation.
    fn account_runtime_delta(&mut self, current: &Arc<EEVDFTask<T>>, delta: RuntimeDelta) -> bool {
        self.account_runtime_transaction(current, delta)
            .expect("EEVDF runtime accounting exhausted arithmetic")
    }

    fn account_runtime_transaction(
        &mut self,
        current: &Arc<EEVDFTask<T>>,
        delta: RuntimeDelta,
    ) -> Result<bool, SchedulerError> {
        let period = delta.period_ns();
        if period == 0 {
            return Err(SchedulerError::InconsistentState);
        }
        let state = unsafe { *current.owned_state() };
        if state.runtime_remainder_ns >= period
            || (state.runtime_remainder_ns != 0
                && state.runtime_fraction_period_ns != 0
                && state.runtime_fraction_period_ns != period)
            || (state.runtime_fraction_remainder_ns != 0
                && (state.runtime_fraction_period_ns == 0
                    || state.runtime_fraction_remainder_ns >= state.runtime_fraction_period_ns))
            || (state.runtime_fraction_remainder_ns != 0
                && state.runtime_fraction_period_ns != period)
        {
            return Err(SchedulerError::InconsistentState);
        }
        Self::validate_runtime_fraction_owner(current, state)?;
        let params = current.sched_params();
        let fraction_weight = Self::runtime_fraction_weight(params)?;
        let total = u128::from(state.runtime_remainder_ns)
            .checked_add(u128::from(delta.elapsed_ns()))
            .ok_or(SchedulerError::ArithmeticExhausted)?;
        let mut periods = total / u128::from(period);
        let remainder = (total % u128::from(period)) as u64;
        let old_remainder = if state.runtime_fraction_period_ns == period {
            state.runtime_fraction_remainder_ns
        } else {
            0
        };
        let numerator = u128::from(remainder)
            .checked_mul(ONE)
            .and_then(|value| value.checked_add(u128::from(old_remainder)))
            .ok_or(SchedulerError::ArithmeticExhausted)?;
        let extra_q = numerator / u128::from(period);
        let conversion_remainder = (numerator % u128::from(period)) as u64;
        if extra_q >= ONE {
            periods = periods
                .checked_add(extra_q / ONE)
                .ok_or(SchedulerError::ArithmeticExhausted)?;
        }
        let next_remainder = if extra_q >= ONE { 0 } else { remainder };
        let reschedule = if periods == 0 {
            false
        } else if is_rt_class(params.class) {
            // `extra_q < ONE` is backed by `next_remainder` and remains
            // deferred until the explicit settlement boundary; only the
            // whole periods are materialized here.
            self.rr_service_transaction(current, periods, 0)?
        } else {
            self.fair_service_transaction(current, periods, 0)?
        };

        // The model transaction above is the only fallible state transition.
        // Publishing this bounded conversion token cannot fail, so a retry
        // after an arithmetic error observes the original bytes exactly.
        unsafe {
            let state = current.owned_state_mut();
            state.runtime_remainder_ns = next_remainder;
            // Keep the new fixed-point conversion residue even when it did
            // not yet produce a whole scheduler tick.  Reusing
            // `old_remainder` here drops the low bits of a second fragment;
            // repeated sub-period samples then slowly lose service.
            Self::publish_runtime_fraction_owner(
                state,
                params,
                fraction_weight,
                conversion_remainder,
                period,
            );
        }
        Ok(reschedule)
    }

    fn settle_before_lifecycle(&mut self, task: &Arc<EEVDFTask<T>>) -> Result<(), SchedulerError> {
        let state = unsafe { *task.owned_state() };
        if state.runtime_remainder_ns != 0 || state.runtime_fraction_remainder_ns != 0 {
            let period = state.runtime_fraction_period_ns;
            self.settle_runtime_remainder(task, period)?;
        }
        Ok(())
    }
}

impl<T> BaseScheduler for EEVDFScheduler<T> {
    type SchedItem = Arc<EEVDFTask<T>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) -> Result<(), SchedulerError> {
        self.enqueue_new(task)
    }

    fn remove_task(
        &mut self,
        task: &Self::SchedItem,
    ) -> Result<Option<Self::SchedItem>, SchedulerError> {
        self.remove_ready(task)
    }

    fn remove_task_for_migration(
        &mut self,
        task: &Self::SchedItem,
    ) -> Result<Option<Self::SchedItem>, SchedulerError> {
        let migration = self.begin_ready_migration(task)?;
        migration.park().map(Some)
    }

    fn deactivate_task(&mut self, task: &Self::SchedItem, reason: DeactivateReason) {
        if matches!(reason, DeactivateReason::Migrate) {
            self.ensure_running(task)
                .expect("EEVDF deactivate current validation failed");
            self.settle_before_lifecycle(task)
                .expect("EEVDF lifecycle boundary retained invalid runtime state");
            let migration = self
                .begin_running_migration(task)
                .expect("EEVDF running migration preparation failed");
            migration
                .park()
                .expect("EEVDF running migration park ownership changed");
            return;
        }
        self.ensure_running(task)
            .expect("EEVDF deactivate current validation failed");
        // The scheduler trait is infallible at lifecycle edges, so the task
        // state carries the period captured by the last runtime sample. This
        // closes direct scheduler callers as well as the axtask integration;
        // no pending wall time can silently cross Sleep/Exit/Migrate.
        self.settle_before_lifecycle(task)
            .expect("EEVDF lifecycle boundary retained invalid runtime state");
        match reason {
            DeactivateReason::Sleep => {
                let params = task.sched_params();
                if is_rt_class(params.class) {
                    // RT tasks have no fair Entity, so the marker is the
                    // state that distinguishes sleeping from virgin.
                    unsafe {
                        task.owned_state_mut().rt_sleeping = true;
                        task.owned_state_mut().sleep_v = Some(self.clock.v);
                    };
                    self.running = None;
                    task.transfer_owner(self.id, UNOWNED)
                        .expect("EEVDF RT owner invariant violated during sleep");
                    return;
                }
                let weight =
                    Self::fair_weight(params).expect("EEVDF running fair task has invalid weight");
                let old_entity = unsafe { task.owned_state().entity }
                    .expect("EEVDF running fair task has no entity");
                let mut next_entity = old_entity;
                next_entity
                    .begin_sleep(self.clock.v)
                    .expect("EEVDF infallible sleep transition failed");
                let mut next_clock = self.clock;
                next_clock
                    .checked_sub_weight(weight)
                    .expect("EEVDF infallible sleep accounting failed");
                // Commit only after both value transitions completed.
                unsafe {
                    task.owned_state_mut().entity = Some(next_entity);
                    task.owned_state_mut().rt_sleeping = false;
                    task.owned_state_mut().sleep_v = Some(self.clock.v);
                }
                self.clock = next_clock;
                self.running = None;
                task.transfer_owner(self.id, UNOWNED)
                    .expect("EEVDF fair owner invariant violated during sleep");
            }
            DeactivateReason::Exit => {
                let params = task.sched_params();
                let next_clock = if is_rt_class(params.class) {
                    self.clock
                } else {
                    let weight = Self::fair_weight(params)
                        .expect("EEVDF running fair task has invalid weight");
                    let mut next = self.clock;
                    next.checked_sub_weight(weight)
                        .expect("EEVDF infallible exit accounting failed");
                    next
                };
                unsafe {
                    task.owned_state_mut().entity = None;
                    task.owned_state_mut().runtime_remainder_ns = 0;
                    task.owned_state_mut().runtime_fraction_remainder_ns = 0;
                    task.owned_state_mut().runtime_fraction_period_ns = 0;
                    task.owned_state_mut().runtime_fraction_class = EevdfTaskClass::Normal;
                    task.owned_state_mut().runtime_fraction_weight = 0;
                    task.owned_state_mut().rr_fraction_q = 0;
                    task.owned_state_mut().migration = None;
                    task.owned_state_mut().dormant_fair = None;
                    task.owned_state_mut().rt_sleeping = false;
                    task.owned_state_mut().fork_seed = None;
                    task.owned_state_mut().sleep_v = None;
                }
                self.clock = next_clock;
                self.running = None;
                task.transfer_owner(self.id, UNOWNED)
                    .expect("EEVDF running task owner invariant violated during exit");
            }
            DeactivateReason::Migrate => unreachable!("handled before lifecycle validation"),
        }
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        if self.running.is_some() {
            return None;
        }
        let next = self.pick_ready()?;
        next.transfer_owner(self.id, self.id)
            .expect("EEVDF picked task owner invariant violated");
        self.running = Some(Arc::clone(&next));
        Some(next)
    }

    fn put_prev_task(
        &mut self,
        prev: Self::SchedItem,
        preempt: bool,
    ) -> Result<(), SchedulerError> {
        self.put_running(
            prev,
            if preempt {
                EnqueueReason::Preempt
            } else {
                EnqueueReason::Yield
            },
        )
    }

    fn enqueue_task(
        &mut self,
        task: Self::SchedItem,
        reason: EnqueueReason,
    ) -> Result<(), SchedulerError> {
        match reason {
            EnqueueReason::New => self.enqueue_new(task),
            EnqueueReason::Wakeup => self.enqueue_wakeup(task),
            EnqueueReason::Yield | EnqueueReason::Preempt => self.put_running(task, reason),
            EnqueueReason::Migrate => self.enqueue_migrated_task(&task).map(|_| ()),
        }
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        self.ensure_running(current)
            .expect("EEVDF task_tick current validation failed");
        let params = current.sched_params();
        if is_rt_class(params.class) {
            self.tick_rt(current)
        } else {
            self.tick_fair(current)
        }
    }

    fn account_runtime(&mut self, current: &Self::SchedItem, delta: RuntimeDelta) -> bool {
        self.ensure_running(current)
            .expect("EEVDF runtime accounting current validation failed");
        self.account_runtime_delta(current, delta)
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> Result<(), SchedulerError> {
        EEVDFScheduler::set_priority(self, task, prio).map(|_| ())
    }
}

impl<T> Default for EEVDFScheduler<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for EEVDFScheduler<T> {
    fn drop(&mut self) {
        while let Some(node) = self.ready_tree.front() {
            let ptr = node as *const _;
            // SAFETY: ptr came from this tree and is removed before the next
            // iteration; the returned Arc is the tree's ownership unit.
            let task = self
                .ready_tree
                .remove(unsafe { &*ptr })
                .expect("EEVDF ready tree lost a node during scheduler drop");
            // SAFETY: the scheduler still owns the task while it is being
            // removed, so the state can be cleared before releasing the
            // ownership claim.  A surviving Arc must be virgin and
            // re-admittable after this scheduler is dropped.
            unsafe {
                task.owned_state_mut().entity = None;
                task.owned_state_mut().runtime_remainder_ns = 0;
                task.owned_state_mut().runtime_fraction_remainder_ns = 0;
                task.owned_state_mut().runtime_fraction_period_ns = 0;
                task.owned_state_mut().runtime_fraction_class = EevdfTaskClass::Normal;
                task.owned_state_mut().runtime_fraction_weight = 0;
                task.owned_state_mut().rr_fraction_q = 0;
                task.owned_state_mut().migration = None;
                task.owned_state_mut().dormant_fair = None;
                task.owned_state_mut().rt_sleeping = false;
                task.owned_state_mut().fork_seed = None;
                task.owned_state_mut().sleep_v = None;
            }
            task.set_rr_remaining(0);
            task.transfer_owner(self.id, UNOWNED)
                .expect("EEVDF ready owner invariant violated during drop");
        }
        if let Some(task) = self.running.take() {
            // SAFETY: `running` remains owned by this scheduler until the
            // claim is released below.
            unsafe {
                task.owned_state_mut().entity = None;
                task.owned_state_mut().runtime_remainder_ns = 0;
                task.owned_state_mut().runtime_fraction_remainder_ns = 0;
                task.owned_state_mut().runtime_fraction_period_ns = 0;
                task.owned_state_mut().runtime_fraction_class = EevdfTaskClass::Normal;
                task.owned_state_mut().runtime_fraction_weight = 0;
                task.owned_state_mut().rr_fraction_q = 0;
                task.owned_state_mut().migration = None;
                task.owned_state_mut().dormant_fair = None;
                task.owned_state_mut().rt_sleeping = false;
                task.owned_state_mut().fork_seed = None;
                task.owned_state_mut().sleep_v = None;
            }
            task.set_rr_remaining(0);
            task.transfer_owner(self.id, UNOWNED)
                .expect("EEVDF running owner invariant violated during drop");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn runtime_accounting_carries_subperiod_and_is_idempotent_at_same_boundary() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1usize));
        scheduler.add_task(task).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        let before = scheduler.clock();
        let half = RuntimeDelta::new(5_000_000, 10_000_000);

        assert!(!scheduler.account_runtime(&current, half));
        assert_eq!(scheduler.clock(), before);
        assert_eq!(
            unsafe { current.owned_state().runtime_remainder_ns },
            5_000_000
        );

        // A second half-period completes exactly one scheduler period.
        assert!(!scheduler.account_runtime(&current, half));
        assert_eq!(scheduler.clock().accounted_ticks, 1);
        assert_eq!(unsafe { current.owned_state().runtime_remainder_ns }, 0);

        // Replaying the same timestamp produces a zero token and cannot
        // double-charge the current entity.
        let after = scheduler.clock();
        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(0, 10_000_000)));
        assert_eq!(scheduler.clock(), after);
        assert_eq!(unsafe { current.owned_state().runtime_remainder_ns }, 0);
    }

    #[test]
    fn running_migration_settles_before_detach_and_keeps_new_remainder() {
        let mut source = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(2usize));
        source.add_task(task).unwrap();
        let current = source.pick_next_task().unwrap();
        assert!(!source.account_runtime(&current, RuntimeDelta::new(5_000_000, 10_000_000)));
        let migration = source.begin_running_migration(&current).unwrap();

        let mut destination = EEVDFScheduler::new();
        let parked = destination.commit_migration(migration).unwrap();
        let current = destination.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&parked, &current));
        assert!(!destination.account_runtime(&current, RuntimeDelta::new(5_000_000, 10_000_000)));
        assert_eq!(destination.clock().accounted_ticks, 0);
        assert_eq!(
            unsafe { current.owned_state().runtime_remainder_ns },
            5_000_000
        );
        destination
            .settle_runtime_remainder(&current, 10_000_000)
            .unwrap();
        assert_eq!(unsafe { current.owned_state().runtime_remainder_ns }, 0);
    }

    #[test]
    fn runtime_remainder_parameter_update_replays_old_fair_weight() {
        let mut actual = EEVDFScheduler::new();
        let actual_task = Arc::new(EEVDFTask::new(10usize));
        let actual_peer = Arc::new(EEVDFTask::new(11usize));
        actual.add_task(actual_task.clone()).unwrap();
        actual.add_task(actual_peer).unwrap();
        let actual_current = actual.pick_next_task().unwrap();

        let mut reference = EEVDFScheduler::new();
        let reference_task = Arc::new(EEVDFTask::new(10usize));
        let reference_peer = Arc::new(EEVDFTask::new(11usize));
        reference.add_task(reference_task.clone()).unwrap();
        reference.add_task(reference_peer).unwrap();
        let reference_current = reference.pick_next_task().unwrap();

        let half = RuntimeDelta::new(5_000_000, 10_000_000);
        let _ = actual.account_runtime(&actual_current, half);
        let _ = reference.account_runtime(&reference_current, half);

        let new_params = EevdfTaskParams {
            class: EevdfTaskClass::Normal,
            nice: 10,
            rt_priority: 0,
        };
        let _ = actual
            .set_task_params_with_runtime(&actual_current, new_params, 10_000_000)
            .unwrap();
        let _ = reference
            .set_task_params_with_runtime(&reference_current, new_params, 10_000_000)
            .unwrap();
        let _ = actual.account_runtime(&actual_current, half);
        let _ = reference.account_runtime(&reference_current, half);
        let _ = actual
            .settle_runtime_remainder(&actual_current, 10_000_000)
            .unwrap();
        let _ = reference
            .settle_runtime_remainder(&reference_current, 10_000_000)
            .unwrap();

        assert_eq!(actual.clock(), reference.clock());
        assert_eq!(unsafe { actual_current.owned_state().entity }, unsafe {
            reference_current.owned_state().entity
        });
        assert_eq!(
            unsafe { actual_current.owned_state().runtime_remainder_ns },
            0
        );
    }

    #[test]
    fn runtime_settlement_keeps_sleeping_fair_reconfigure_model_valid() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(12usize));
        scheduler.add_task(task.clone()).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        let half = RuntimeDelta::new(5_000_000, 10_000_000);
        assert!(!scheduler.account_runtime(&current, half));

        // Materialize the old fair tuple before the lifecycle edge.  No
        // absolute clock/entity replay is allowed to follow the task into
        // sleeping state.
        let _ = scheduler
            .set_task_params_with_runtime(
                &current,
                EevdfTaskParams {
                    class: EevdfTaskClass::Normal,
                    nice: 5,
                    rt_priority: 0,
                },
                10_000_000,
            )
            .unwrap();
        assert_eq!(unsafe { current.owned_state().runtime_remainder_ns }, 0);
        scheduler.deactivate_task(&current, DeactivateReason::Sleep);

        let _ = scheduler
            .set_task_params(
                &current,
                EevdfTaskParams {
                    class: EevdfTaskClass::Batch,
                    nice: 5,
                    rt_priority: 0,
                },
            )
            .unwrap();
        assert_eq!(current.sched_params().class, EevdfTaskClass::Batch);
    }

    #[test]
    fn legacy_parameter_update_cannot_bypass_pending_runtime_settlement() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(13usize));
        scheduler.add_task(task.clone()).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(5_000_000, 10_000_000)));

        assert_eq!(
            scheduler.set_task_params(&current, EevdfTaskParams::default()),
            Err(SchedulerError::InconsistentState)
        );
        assert_eq!(
            unsafe { current.owned_state().runtime_remainder_ns },
            5_000_000
        );
        let _ = scheduler
            .set_task_params_with_runtime(&current, EevdfTaskParams::default(), 10_000_000)
            .unwrap();
        assert_eq!(unsafe { current.owned_state().runtime_remainder_ns }, 0);
    }

    #[test]
    fn runtime_parameter_wrappers_report_claim_and_foreign_races() {
        let mut scheduler = EEVDFScheduler::new();
        let configuring = Arc::new(EEVDFTask::new(14usize));
        let claim = configuring.value().claim_reconfiguration().unwrap();
        assert_eq!(
            scheduler.set_task_params_with_runtime(
                &configuring,
                EevdfTaskParams::default(),
                10_000_000,
            ),
            Err(SchedulerError::TaskBusy)
        );
        assert_eq!(
            scheduler.set_priority_with_runtime(&configuring, 0, 10_000_000),
            Err(SchedulerError::TaskBusy)
        );
        drop(claim);

        let mut owner = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(15usize));
        owner.add_task(task.clone()).unwrap();
        assert_eq!(
            scheduler.set_priority_with_runtime(&task, 0, 10_000_000),
            Err(SchedulerError::ForeignQueue)
        );

        let migration = owner.begin_ready_migration(&task).unwrap();
        assert_eq!(
            scheduler.set_task_params_with_runtime(&task, EevdfTaskParams::default(), 10_000_000,),
            Err(SchedulerError::TaskBusy)
        );
        owner.rollback_migration(migration).unwrap();
    }

    #[test]
    fn runtime_remainder_class_transition_charges_old_class_first() {
        let mut fair_to_rt = EEVDFScheduler::new();
        let fair_task = Arc::new(EEVDFTask::new(20usize));
        fair_to_rt.add_task(fair_task).unwrap();
        let fair_current = fair_to_rt.pick_next_task().unwrap();
        let half = RuntimeDelta::new(5_000_000, 10_000_000);
        assert!(!fair_to_rt.account_runtime(&fair_current, half));
        let _ = fair_to_rt
            .set_task_params_with_runtime(
                &fair_current,
                rt(EevdfTaskClass::RoundRobin, 20),
                10_000_000,
            )
            .unwrap();
        assert!(!fair_to_rt.account_runtime(&fair_current, half));
        assert_eq!(fair_to_rt.clock().accounted_ticks, 0);
        assert_eq!(fair_current.rr_remaining(), RR_TIMESLICE_TICKS);
        fair_to_rt
            .settle_runtime_remainder(&fair_current, 10_000_000)
            .unwrap();
        assert_eq!(fair_current.rr_remaining(), RR_TIMESLICE_TICKS);
        assert!(!fair_to_rt.account_runtime(&fair_current, half));
        fair_to_rt
            .settle_runtime_remainder(&fair_current, 10_000_000)
            .unwrap();
        assert_eq!(fair_current.rr_remaining(), RR_TIMESLICE_TICKS - 1);

        let mut rt_to_fair = EEVDFScheduler::new();
        let rt_task = Arc::new(EEVDFTask::new(21usize));
        rt_task
            .configure(rt(EevdfTaskClass::RoundRobin, 20))
            .unwrap();
        rt_to_fair.add_task(rt_task).unwrap();
        let rt_current = rt_to_fair.pick_next_task().unwrap();
        assert!(!rt_to_fair.account_runtime(&rt_current, half));
        let _ = rt_to_fair
            .set_task_params_with_runtime(&rt_current, EevdfTaskParams::default(), 10_000_000)
            .unwrap();
        assert!(!rt_to_fair.account_runtime(&rt_current, half));
        let _ = rt_to_fair
            .settle_runtime_remainder(&rt_current, 10_000_000)
            .unwrap();
        assert_eq!(rt_to_fair.clock().accounted_ticks, 0);

        let mut fifo_to_rr = EEVDFScheduler::new();
        let fifo_task = Arc::new(EEVDFTask::new(22usize));
        fifo_task.configure(rt(EevdfTaskClass::Fifo, 20)).unwrap();
        fifo_to_rr.add_task(fifo_task).unwrap();
        let fifo_current = fifo_to_rr.pick_next_task().unwrap();
        assert!(!fifo_to_rr.account_runtime(&fifo_current, half));
        let _ = fifo_to_rr
            .set_task_params_with_runtime(
                &fifo_current,
                rt(EevdfTaskClass::RoundRobin, 20),
                10_000_000,
            )
            .unwrap();
        assert_eq!(fifo_current.rr_remaining(), RR_TIMESLICE_TICKS);
        assert_eq!(
            unsafe { fifo_current.owned_state().runtime_remainder_ns },
            0
        );
    }

    #[test]
    fn runtime_settlement_stages_whole_fraction_and_remainder_atomically() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(23usize));
        let peer = Arc::new(EEVDFTask::new(24usize));
        scheduler.add_task(task).unwrap();
        scheduler.add_task(peer).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        let delta = RuntimeDelta::new(5_000_000, 10_000_000);
        assert!(!scheduler.account_runtime(&current, delta));
        let before_clock = scheduler.clock;
        let before_entity = unsafe { current.owned_state().entity.unwrap() };
        let before_state = unsafe { *current.owned_state() };

        scheduler.clock.v = i128::MAX;
        assert_eq!(
            scheduler.settle_runtime_remainder(&current, delta.period_ns()),
            Err(SchedulerError::ArithmeticExhausted)
        );
        assert_eq!(scheduler.clock.v, i128::MAX);
        assert_eq!(
            unsafe { current.owned_state().entity.unwrap() },
            before_entity
        );
        assert_eq!(
            unsafe { current.owned_state().runtime_remainder_ns },
            before_state.runtime_remainder_ns
        );
        assert_eq!(unsafe { *current.owned_state() }, before_state);

        scheduler.clock = before_clock;
        let _ = scheduler
            .settle_runtime_remainder(&current, delta.period_ns())
            .unwrap();
        assert_eq!(unsafe { current.owned_state().runtime_remainder_ns }, 0);
    }

    #[test]
    fn runtime_fraction_conversion_residue_conserves_irregular_period_fragments() {
        let mut fragmented = EEVDFScheduler::new();
        let fragmented_task = Arc::new(EEVDFTask::new(230usize));
        fragmented.add_task(fragmented_task).unwrap();
        let fragmented_current = fragmented.pick_next_task().unwrap();

        for _ in 0..3 {
            assert!(!fragmented.account_runtime(&fragmented_current, RuntimeDelta::new(1, 3)));
            let _ = fragmented
                .settle_runtime_remainder(&fragmented_current, 3)
                .unwrap();
        }

        let mut contiguous = EEVDFScheduler::new();
        let contiguous_task = Arc::new(EEVDFTask::new(231usize));
        contiguous.add_task(contiguous_task).unwrap();
        let contiguous_current = contiguous.pick_next_task().unwrap();
        assert!(!contiguous.account_runtime(&contiguous_current, RuntimeDelta::new(3, 3)));

        let fragmented_clock = fragmented.clock();
        let contiguous_clock = contiguous.clock();
        assert_eq!(fragmented_clock.v, contiguous_clock.v);
        assert_eq!(fragmented_clock.remainder, contiguous_clock.remainder);
        assert_eq!(fragmented_clock.total_weight, contiguous_clock.total_weight);
        assert_eq!(unsafe { fragmented_current.owned_state().entity }, unsafe {
            contiguous_current.owned_state().entity
        });
        let state = unsafe { fragmented_current.owned_state() };
        assert_eq!(state.runtime_remainder_ns, 0);
        assert_eq!(state.runtime_fraction_remainder_ns, 0);
    }

    #[test]
    fn settled_conversion_residue_is_cleared_at_class_change_after_old_context_accounted() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(232usize));
        scheduler.add_task(task).unwrap();
        let current = scheduler.pick_next_task().unwrap();

        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(1, 3)));
        let _ = scheduler.settle_runtime_remainder(&current, 3).unwrap();
        assert_eq!(unsafe { current.owned_state().runtime_remainder_ns }, 0);
        assert_ne!(
            unsafe { current.owned_state().runtime_fraction_remainder_ns },
            0
        );

        assert_eq!(
            scheduler.set_task_params(
                &current,
                EevdfTaskParams {
                    class: EevdfTaskClass::Batch,
                    nice: 0,
                    rt_priority: 0,
                },
            ),
            Err(SchedulerError::InconsistentState)
        );

        let result = scheduler.set_task_params_with_runtime(
            &current,
            EevdfTaskParams {
                class: EevdfTaskClass::Batch,
                nice: 0,
                rt_priority: 0,
            },
            3,
        );
        assert!(result.is_ok());
        assert_eq!(
            unsafe { current.owned_state().runtime_fraction_remainder_ns },
            0
        );
        assert_eq!(
            unsafe { current.owned_state().runtime_fraction_period_ns },
            0
        );
    }

    #[test]
    fn fifo_to_fair_does_not_reinterpret_fifo_conversion_residue() {
        let mut fifo_scheduler = EEVDFScheduler::new();
        let fifo = Arc::new(EEVDFTask::new(233usize));
        fifo.configure(rt(EevdfTaskClass::Fifo, 20)).unwrap();
        fifo_scheduler.add_task(fifo).unwrap();
        let fifo_current = fifo_scheduler.pick_next_task().unwrap();
        assert!(!fifo_scheduler.account_runtime(&fifo_current, RuntimeDelta::new(1, 3)));

        let _ = fifo_scheduler
            .set_task_params_with_runtime(&fifo_current, EevdfTaskParams::default(), 3)
            .unwrap();
        assert_eq!(fifo_scheduler.clock().accounted_ticks, 0);
        assert_eq!(
            unsafe { fifo_current.owned_state().runtime_remainder_ns },
            0
        );
        assert_eq!(
            unsafe { fifo_current.owned_state().runtime_fraction_remainder_ns },
            0
        );

        // Only service observed after the fair publication may affect the
        // fair entity. Compare it with a fresh fair task receiving the same
        // two-nanosecond fragment, not with the old FIFO fragment.
        assert!(!fifo_scheduler.account_runtime(&fifo_current, RuntimeDelta::new(2, 3)));
        fifo_scheduler
            .settle_runtime_remainder(&fifo_current, 3)
            .unwrap();

        let mut reference = EEVDFScheduler::new();
        let reference_task = Arc::new(EEVDFTask::new(234usize));
        reference.add_task(reference_task).unwrap();
        let reference_current = reference.pick_next_task().unwrap();
        assert!(!reference.account_runtime(&reference_current, RuntimeDelta::new(2, 3)));
        reference
            .settle_runtime_remainder(&reference_current, 3)
            .unwrap();

        assert_eq!(fifo_scheduler.clock(), reference.clock());
        assert_eq!(unsafe { fifo_current.owned_state().entity }, unsafe {
            reference_current.owned_state().entity
        });
    }

    #[test]
    fn fair_reweight_and_rr_fifo_transitions_clear_old_residue_atomically() {
        let mut fair_scheduler = EEVDFScheduler::new();
        let fair = Arc::new(EEVDFTask::new(235usize));
        fair_scheduler.add_task(fair).unwrap();
        let fair_current = fair_scheduler.pick_next_task().unwrap();
        assert!(!fair_scheduler.account_runtime(&fair_current, RuntimeDelta::new(1, 3)));
        let _ = fair_scheduler
            .set_task_params_with_runtime(
                &fair_current,
                EevdfTaskParams {
                    class: EevdfTaskClass::Normal,
                    nice: 5,
                    rt_priority: 0,
                },
                3,
            )
            .unwrap();
        assert_eq!(
            unsafe { fair_current.owned_state().runtime_fraction_remainder_ns },
            0
        );
        assert_eq!(
            unsafe { fair_current.owned_state().runtime_fraction_period_ns },
            0
        );

        let mut rr_fifo = EEVDFScheduler::new();
        let rr = Arc::new(EEVDFTask::new(236usize));
        rr.configure(rt(EevdfTaskClass::RoundRobin, 10)).unwrap();
        rr_fifo.add_task(rr).unwrap();
        let rr_current = rr_fifo.pick_next_task().unwrap();
        assert!(!rr_fifo.account_runtime(&rr_current, RuntimeDelta::new(1, 3)));
        let _ = rr_fifo
            .set_task_params_with_runtime(&rr_current, rt(EevdfTaskClass::Fifo, 10), 3)
            .unwrap();
        assert_eq!(unsafe { rr_current.owned_state().rr_fraction_q }, 0);
        assert_eq!(
            unsafe { rr_current.owned_state().runtime_fraction_remainder_ns },
            0
        );

        let _ = rr_fifo
            .set_task_params_with_runtime(&rr_current, rt(EevdfTaskClass::RoundRobin, 10), 3)
            .unwrap();
        assert_eq!(rr_current.rr_remaining(), RR_TIMESLICE_TICKS);
        assert_eq!(unsafe { rr_current.owned_state().rr_fraction_q }, 0);
    }

    #[test]
    fn runtime_parameter_failure_rolls_back_old_tuple_settlement() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(237usize));
        task.configure(rt(EevdfTaskClass::Fifo, 20)).unwrap();
        scheduler.add_task(task).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(1, 3)));

        let before_state = unsafe { *current.owned_state() };
        let before_rr = current.rr_remaining();
        let mut failed_clock = scheduler.clock;
        failed_clock.total_weight = u128::MAX;
        scheduler.clock = failed_clock;

        // FIFO has no fair weight to add.  The attempted FIFO -> fair
        // publication therefore fails after the old FIFO runtime boundary
        // has been settled, exercising the wrapper's rollback path.
        assert_eq!(
            scheduler.set_task_params_with_runtime(&current, EevdfTaskParams::default(), 3,),
            Err(SchedulerError::ArithmeticExhausted)
        );
        assert_eq!(scheduler.clock, failed_clock);
        assert_eq!(unsafe { *current.owned_state() }, before_state);
        assert_eq!(current.rr_remaining(), before_rr);
        assert_eq!(current.sched_params().class, EevdfTaskClass::Fifo);
    }

    #[test]
    fn fractional_rr_exhaustion_requests_equal_priority_peer() {
        let mut scheduler = EEVDFScheduler::new();
        let first = Arc::new(EEVDFTask::new(25usize));
        let second = Arc::new(EEVDFTask::new(26usize));
        first.configure(rt(EevdfTaskClass::RoundRobin, 10)).unwrap();
        second
            .configure(rt(EevdfTaskClass::RoundRobin, 10))
            .unwrap();
        scheduler.add_task(first).unwrap();
        scheduler.add_task(second).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        current.set_rr_remaining(1);
        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(5, 10)));
        assert!(!scheduler.settle_runtime_remainder(&current, 10).unwrap());
        assert_eq!(current.rr_remaining(), 1);
        assert_eq!(
            unsafe { current.owned_state().rr_fraction_q },
            (ONE / 2) as u64
        );

        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(5, 10)));
        assert!(scheduler.settle_runtime_remainder(&current, 10).unwrap());
        assert_eq!(current.rr_remaining(), 0);
        assert_eq!(unsafe { current.owned_state().rr_fraction_q }, 0);
    }

    #[test]
    fn rr_yield_and_exhausted_preempt_reset_fractional_budget() {
        let mut scheduler = EEVDFScheduler::new();
        let first = Arc::new(EEVDFTask::new(27usize));
        let second = Arc::new(EEVDFTask::new(28usize));
        first.configure(rt(EevdfTaskClass::RoundRobin, 10)).unwrap();
        second
            .configure(rt(EevdfTaskClass::RoundRobin, 10))
            .unwrap();
        scheduler.add_task(first).unwrap();
        scheduler.add_task(second).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        unsafe { current.owned_state_mut().rr_fraction_q = (ONE / 2) as u64 };
        scheduler.put_prev_task(current.clone(), false).unwrap();
        assert_eq!(current.rr_remaining(), RR_TIMESLICE_TICKS);
        assert_eq!(unsafe { current.owned_state().rr_fraction_q }, 0);

        let current = scheduler.pick_next_task().unwrap();
        unsafe { current.owned_state_mut().rr_fraction_q = (ONE / 2) as u64 };
        current.set_rr_remaining(0);
        scheduler.put_prev_task(current.clone(), true).unwrap();
        assert_eq!(current.rr_remaining(), RR_TIMESLICE_TICKS);
        assert_eq!(unsafe { current.owned_state().rr_fraction_q }, 0);
    }

    #[test]
    fn rr_whole_service_after_fractional_progress_carries_new_slice_progress() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(280usize));
        task.configure(rt(EevdfTaskClass::RoundRobin, 10)).unwrap();
        scheduler.add_task(task).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        current.set_rr_remaining(1);

        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(5, 10)));
        assert!(!scheduler.settle_runtime_remainder(&current, 10).unwrap());
        assert_eq!(
            unsafe { current.owned_state().rr_fraction_q },
            (ONE / 2) as u64
        );

        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(15, 10)));
        assert_eq!(current.rr_remaining(), RR_TIMESLICE_TICKS);
        assert_eq!(
            unsafe { current.owned_state().rr_fraction_q },
            (ONE / 2) as u64
        );
        assert!(!scheduler.settle_runtime_remainder(&current, 10).unwrap());
        assert_eq!(current.rr_remaining(), RR_TIMESLICE_TICKS - 1);
        assert_eq!(unsafe { current.owned_state().rr_fraction_q }, 0);
    }

    #[test]
    fn direct_sleep_boundary_settles_pending_runtime_before_wakeup() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(29usize));
        scheduler.add_task(task.clone()).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        assert!(!scheduler.account_runtime(&current, RuntimeDelta::new(5, 10)));
        scheduler.deactivate_task(&current, DeactivateReason::Sleep);
        assert_eq!(unsafe { task.owned_state().runtime_remainder_ns }, 0);
        assert_eq!(scheduler.clock().accounted_ticks, 0);
        scheduler
            .enqueue_task(task.clone(), EnqueueReason::Wakeup)
            .unwrap();
        assert_eq!(unsafe { task.owned_state().runtime_remainder_ns }, 0);
        assert_eq!(
            unsafe { task.owned_state().runtime_fraction_remainder_ns },
            0
        );
    }

    #[test]
    fn params_validation_and_packed_round_trip_match_cfs_shape() {
        let normal = EevdfTaskParams {
            class: EevdfTaskClass::Normal,
            nice: -20,
            rt_priority: u8::MAX,
        }
        .validated()
        .unwrap();
        assert_eq!(normal.nice, -20);
        assert_eq!(normal.rt_priority, 0);
        assert_eq!(EevdfTaskParams::from_packed(normal.packed()), normal);

        let idle = EevdfTaskParams {
            class: EevdfTaskClass::Idle,
            nice: -20,
            rt_priority: 1,
        }
        .validated()
        .unwrap();
        assert_eq!(idle.nice, NICE_RANGE_POS);
        assert_eq!(idle.rt_priority, 0);

        assert!(EevdfTaskParams {
            class: EevdfTaskClass::RoundRobin,
            nice: 4,
            rt_priority: 0,
        }
        .validated()
        .is_none());
        let fifo = EevdfTaskParams {
            class: EevdfTaskClass::Fifo,
            nice: 17,
            rt_priority: 12,
        }
        .validated()
        .unwrap();
        assert_eq!(fifo.nice, 0);
        assert_eq!(EevdfTaskParams::from_packed(fifo.packed()), fifo);
        assert_eq!(eevdf_weight_for_nice(0), Some(1024));
        assert_eq!(eevdf_weight_for_nice(-20), Some(88761));
        assert_eq!(eevdf_weight_for(idle), Some(15));
    }

    #[test]
    fn invalid_nice_is_rejected_without_weight_table_panic() {
        assert_eq!(eevdf_weight_for_nice(-21), None);
        assert_eq!(eevdf_weight_for_nice(20), None);
        assert_eq!(eevdf_weight_for_nice(i8::MIN), None);
        assert_eq!(
            eevdf_weight_for(EevdfTaskParams {
                class: EevdfTaskClass::Normal,
                nice: 20,
                rt_priority: 0,
            }),
            None
        );
    }

    #[test]
    fn constructor_inner_deref_and_into_inner() {
        let task = EEVDFTask::new(String::from("task"));
        assert_eq!(task.inner(), "task");
        assert_eq!(&*task, "task");
        assert_eq!(task.sched_params(), EevdfTaskParams::default());
        assert_eq!(task.value().rr_remaining(), 0);
        assert_eq!(task.into_inner(), "task");
    }

    #[test]
    fn configure_is_atomic_and_excludes_competing_owner() {
        let task = EEVDFTask::new(7usize);
        let bad = EevdfTaskParams {
            class: EevdfTaskClass::Normal,
            nice: 20,
            rt_priority: 0,
        };
        assert_eq!(task.configure(bad), Err(SchedulerError::InvalidParameters));
        assert_eq!(task.sched_params(), EevdfTaskParams::default());

        let claim = task.value().claim_configuration().unwrap();
        // SAFETY: `claim` holds the CONFIGURING ownership exclusion required
        // by the helper's contract.
        assert!(unsafe { task.is_virgin() });
        assert_eq!(
            task.configure(EevdfTaskParams::default()),
            Err(SchedulerError::TaskBusy)
        );
        drop(claim);
        task.configure(EevdfTaskParams {
            class: EevdfTaskClass::Batch,
            nice: 5,
            rt_priority: 0,
        })
        .unwrap();
        assert_eq!(task.sched_params().class, EevdfTaskClass::Batch);

        let claim = task.value().claim_configuration().unwrap();
        // SAFETY: `claim` holds the CONFIGURING ownership exclusion for both
        // the state write and the virginity check.
        unsafe {
            task.owned_state_mut().entity =
                Some(Entity::new(crate::eevdf_model::RequestClass::Normal, 1, 1, 0).unwrap());
            assert!(!task.is_virgin());
        }
        claim.finish().unwrap();
        assert_eq!(
            task.configure(EevdfTaskParams::default()),
            Err(SchedulerError::AlreadyQueued)
        );
    }

    #[test]
    fn payload_and_task_have_explicit_conditional_smp_traits() {
        assert_send::<EevdfTaskPayload<i32>>();
        assert_sync::<EevdfTaskPayload<i32>>();
        assert_send_sync::<EevdfTaskPayload<i32>>();
        assert_send_sync::<EEVDFTask<i32>>();
    }

    fn rt(class: EevdfTaskClass, priority: u8) -> EevdfTaskParams {
        EevdfTaskParams {
            class,
            nice: 0,
            rt_priority: priority,
        }
    }

    #[test]
    fn scheduler_fair_clock_and_pointer_identity() {
        let mut scheduler = EEVDFScheduler::new();
        let first = Arc::new(EEVDFTask::new(1));
        let second = Arc::new(EEVDFTask::new(2));
        scheduler.add_task(first.clone()).unwrap();
        scheduler.add_task(second.clone()).unwrap();
        assert_eq!(scheduler.clock.total_weight, 2048);

        let current = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&current, &first) || Arc::ptr_eq(&current, &second));
        assert_eq!(scheduler.clock.total_weight, 2048);
        scheduler.put_prev_task(current.clone(), false).unwrap();
        assert_eq!(scheduler.clock.total_weight, 2048);
        let again = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&again, &first) || Arc::ptr_eq(&again, &second));
    }

    #[test]
    fn scheduler_tick_preempts_ineligible_current_with_eligible_later_deadline() {
        let mut scheduler = EEVDFScheduler::new();
        let current = Arc::new(EEVDFTask::new(1));
        let later = Arc::new(EEVDFTask::new(2));
        later
            .configure(EevdfTaskParams {
                class: EevdfTaskClass::Batch,
                ..Default::default()
            })
            .unwrap();
        scheduler.add_task(current.clone()).unwrap();
        scheduler.add_task(later.clone()).unwrap();

        let running = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&running, &current));
        let current_deadline = unsafe {
            running
                .owned_state()
                .entity
                .expect("running fair task has no entity")
                .request
                .deadline
        };
        let later_deadline = unsafe {
            scheduler
                .ready_tree
                .front()
                .expect("ready fair task is missing")
                .owned_state()
                .entity
                .expect("ready fair task has no entity")
                .request
                .deadline
        };
        assert!(later_deadline > current_deadline);

        assert!(scheduler.task_tick(&running));
        assert!(!unsafe {
            running
                .owned_state()
                .entity
                .expect("running fair task lost its entity")
                .is_eligible(scheduler.clock.v)
                .unwrap()
        });
    }

    #[test]
    fn runtime_boundary_preserves_remote_preempt_request_after_full_accounting() {
        let mut scheduler = EEVDFScheduler::new();
        let current = Arc::new(EEVDFTask::new(1));
        let later = Arc::new(EEVDFTask::new(2));
        later
            .configure(EevdfTaskParams {
                class: EevdfTaskClass::Batch,
                ..Default::default()
            })
            .unwrap();
        scheduler.add_task(current.clone()).unwrap();
        scheduler.add_task(later).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&running, &current));

        // A remote wake/preempt publication only asks the owner CPU to
        // re-evaluate. The owner must first consume the complete elapsed
        // sample, then return the exact scheduler decision.
        assert!(scheduler.account_runtime(&running, RuntimeDelta::new(10_000_000, 10_000_000)));
        assert_eq!(scheduler.clock().accounted_ticks, 1);
        assert_eq!(unsafe { running.owned_state().runtime_remainder_ns }, 0);
    }

    #[test]
    fn runtime_accounting_bulk_fair_handles_maximum_delta_once() {
        let mut scheduler = EEVDFScheduler::new();
        let current = Arc::new(EEVDFTask::new(4usize));
        let ready = Arc::new(EEVDFTask::new(5usize));
        scheduler.add_task(current).unwrap();
        scheduler.add_task(ready).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        let ticks = u64::MAX as u128;

        assert!(scheduler.account_runtime(&running, RuntimeDelta::new(u64::MAX, 1)));
        assert_eq!(scheduler.clock().accounted_ticks, ticks);
        assert_eq!(
            unsafe { running.owned_state().entity.unwrap().request.remaining() },
            0
        );
    }

    #[test]
    fn runtime_accounting_bulk_round_robin_uses_constant_time_modulo() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(6usize));
        task.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        scheduler.add_task(task).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        let ticks = u64::MAX as u128;
        let slice = RR_TIMESLICE_TICKS as u128;
        let after_first = (ticks - slice) % slice;
        let expected = if after_first == 0 {
            slice
        } else {
            slice - after_first
        };

        assert!(!scheduler.account_runtime(&running, RuntimeDelta::new(u64::MAX, 1)));
        assert_eq!(running.rr_remaining(), expected as usize);
    }

    #[test]
    fn runtime_accounting_bulk_round_robin_keeps_exhausted_budget_for_peer() {
        let mut scheduler = EEVDFScheduler::new();
        let first = Arc::new(EEVDFTask::new(7usize));
        let second = Arc::new(EEVDFTask::new(8usize));
        first.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        second
            .configure(rt(EevdfTaskClass::RoundRobin, 20))
            .unwrap();
        scheduler.add_task(first).unwrap();
        scheduler.add_task(second).unwrap();
        let running = scheduler.pick_next_task().unwrap();

        assert!(scheduler.account_runtime(&running, RuntimeDelta::new(u64::MAX, 1)));
        assert_eq!(running.rr_remaining(), 0);
    }

    #[test]
    fn scheduler_realtime_priority_fifo_and_rr_rotation() {
        let mut fifo = EEVDFScheduler::new();
        let low = Arc::new(EEVDFTask::new(1));
        low.configure(rt(EevdfTaskClass::Fifo, 10)).unwrap();
        let high = Arc::new(EEVDFTask::new(2));
        high.configure(rt(EevdfTaskClass::Fifo, 20)).unwrap();
        fifo.add_task(low.clone()).unwrap();
        fifo.add_task(high.clone()).unwrap();
        assert!(Arc::ptr_eq(&fifo.pick_next_task().unwrap(), &high));
        fifo.put_prev_task(high.clone(), true).unwrap();
        assert!(Arc::ptr_eq(&fifo.pick_next_task().unwrap(), &high));

        let mut rr = EEVDFScheduler::new();
        let current = Arc::new(EEVDFTask::new(3));
        current
            .configure(rt(EevdfTaskClass::RoundRobin, 30))
            .unwrap();
        let peer = Arc::new(EEVDFTask::new(4));
        peer.configure(rt(EevdfTaskClass::RoundRobin, 30)).unwrap();
        rr.add_task(current.clone()).unwrap();
        rr.add_task(peer.clone()).unwrap();
        assert!(Arc::ptr_eq(&rr.pick_next_task().unwrap(), &current));
        for _ in 0..(RR_TIMESLICE_TICKS - 1) {
            assert!(!rr.task_tick(&current));
        }
        assert!(rr.task_tick(&current));
        assert_eq!(current.rr_remaining(), 0);
        rr.put_prev_task(current.clone(), true).unwrap();
        assert!(Arc::ptr_eq(&rr.pick_next_task().unwrap(), &peer));
    }

    #[test]
    fn scheduler_sleep_wake_reclaims_owner_and_weight() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(7));
        scheduler.add_task(task.clone()).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        scheduler.deactivate_task(&current, DeactivateReason::Sleep);
        assert_eq!(task.owner(), UNOWNED);
        assert_eq!(scheduler.clock.total_weight, 0);
        assert_eq!(
            task.configure(EevdfTaskParams::default()),
            Err(SchedulerError::AlreadyQueued)
        );
        scheduler
            .enqueue_task(task.clone(), EnqueueReason::Wakeup)
            .unwrap();
        assert_ne!(task.owner(), UNOWNED);
        assert_eq!(scheduler.clock.total_weight, 1024);
        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &task));
    }

    #[test]
    fn scheduler_rt_dormant_fair_does_not_gain_rt_clock_credit() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        let peer = Arc::new(EEVDFTask::new(2));
        scheduler.add_task(task.clone()).unwrap();
        scheduler.add_task(peer).unwrap();

        let _ = scheduler
            .set_task_params(&task, rt(EevdfTaskClass::Fifo, 20))
            .unwrap();
        let frozen_lag = unsafe { task.owned_state().dormant_fair.unwrap().lag };
        scheduler.clock.advance_ticks(2).unwrap();
        let _ = scheduler
            .set_task_params(&task, EevdfTaskParams::default())
            .unwrap();

        let entity = unsafe { task.owned_state().entity.unwrap() };
        assert_eq!(entity.lag, frozen_lag);
        assert_eq!(entity.lag_stamp, scheduler.clock.v);
    }

    #[test]
    fn scheduler_rt_sleep_wakes_dormant_fair_without_credit_then_reactivates() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(3));
        scheduler.add_task(task.clone()).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        let _ = scheduler
            .set_task_params(&running, rt(EevdfTaskClass::Fifo, 20))
            .unwrap();
        let frozen_lag = unsafe { task.owned_state().dormant_fair.unwrap().lag };
        scheduler.deactivate_task(&task, DeactivateReason::Sleep);
        scheduler
            .clock
            .jump_to(scheduler.clock.v + 17 * crate::eevdf_model::ONE as i128)
            .unwrap();

        scheduler
            .enqueue_task(task.clone(), EnqueueReason::Wakeup)
            .unwrap();
        let state = unsafe { task.owned_state() };
        assert!(!state.rt_sleeping);
        let dormant = state.dormant_fair.unwrap();
        assert!(!dormant.is_sleeping());
        assert_eq!(dormant.lag, frozen_lag);

        let _ = scheduler
            .set_task_params(&task, EevdfTaskParams::default())
            .unwrap();
        let state = unsafe { task.owned_state() };
        assert!(state.entity.is_some());
        assert!(state.dormant_fair.is_none());
        assert_eq!(state.entity.unwrap().lag, frozen_lag);
    }

    #[test]
    fn scheduler_rt_sleep_dormant_can_become_fair_before_wakeup() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(4));
        scheduler.add_task(task.clone()).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        let _ = scheduler
            .set_task_params(&running, rt(EevdfTaskClass::Fifo, 20))
            .unwrap();
        let frozen_lag = unsafe { task.owned_state().dormant_fair.unwrap().lag };
        scheduler.deactivate_task(&task, DeactivateReason::Sleep);
        scheduler
            .clock
            .jump_to(scheduler.clock.v + 13 * crate::eevdf_model::ONE as i128)
            .unwrap();

        let _ = scheduler
            .set_task_params(&task, EevdfTaskParams::default())
            .unwrap();
        let state = unsafe { task.owned_state() };
        let sleeping = state.entity.unwrap();
        assert!(sleeping.is_sleeping());
        assert!(state.dormant_fair.is_none());
        assert_eq!(sleeping.lag, frozen_lag);
        scheduler
            .enqueue_task(task.clone(), EnqueueReason::Wakeup)
            .unwrap();
        assert!(!unsafe { task.owned_state().entity.unwrap().is_sleeping() });
        assert_eq!(
            unsafe { task.owned_state().entity.unwrap().lag },
            frozen_lag
        );
    }

    #[test]
    fn scheduler_sleep_reconfigure_uses_eventual_aggregate_once() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(5));
        task.configure(EevdfTaskParams {
            nice: -2,
            ..EevdfTaskParams::default()
        })
        .unwrap();
        let peer = Arc::new(EEVDFTask::new(6));
        peer.configure(EevdfTaskParams {
            // Keep the target task's initial deadline strictly earlier for
            // every compile-time EEVDF target, including the longer
            // throughput request.  The old nice=5 pairing ties/reverses once
            // the target doubles, making this accounting test depend on the
            // selected profile rather than the sleep/reconfigure contract.
            nice: 9,
            ..EevdfTaskParams::default()
        })
        .unwrap();
        scheduler.add_task(task.clone()).unwrap();
        scheduler.add_task(peer).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&running, &task));
        let old_q = unsafe { task.owned_state().entity.unwrap().request.q };
        for _ in 0..(old_q - 1) {
            scheduler.task_tick(&running);
        }
        let before = unsafe { task.owned_state().entity.unwrap().request };
        assert_eq!(before.remaining_ticks, 1);
        scheduler.deactivate_task(&task, DeactivateReason::Sleep);

        let new_params = EevdfTaskParams {
            class: EevdfTaskClass::Batch,
            nice: -2,
            rt_priority: 0,
        };
        let _ = scheduler.set_task_params(&task, new_params).unwrap();
        let weight = eevdf_weight_for_nice(new_params.nice).unwrap();
        let eventual_total = scheduler.clock.total_weight + weight;
        let expected_q = (crate::eevdf_model::TARGET_TICKS_BATCH * weight).div_ceil(eventual_total);
        let expected_remaining = (before.remaining_ticks * expected_q).div_ceil(before.q);
        let converted = unsafe { task.owned_state().entity.unwrap().request };
        assert_eq!(converted.q, expected_q);
        assert_eq!(converted.remaining_ticks, expected_remaining);

        scheduler
            .enqueue_task(task.clone(), EnqueueReason::Wakeup)
            .unwrap();
        let woken = unsafe { task.owned_state().entity.unwrap().request };
        assert_eq!(woken.q, expected_q);
        assert_eq!(woken.remaining_ticks, expected_remaining);
    }

    #[test]
    fn scheduler_drop_clears_ready_and_running_state_for_re_admission() {
        let ready = Arc::new(EEVDFTask::new(1));
        let running = Arc::new(EEVDFTask::new(2));
        running
            .configure(rt(EevdfTaskClass::RoundRobin, 20))
            .unwrap();

        {
            let mut scheduler = EEVDFScheduler::new();
            scheduler.add_task(ready.clone()).unwrap();
            scheduler.add_task(running.clone()).unwrap();
            assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &running));
        }

        assert_eq!(ready.owner(), UNOWNED);
        assert_eq!(running.owner(), UNOWNED);
        assert_eq!(ready.rr_remaining(), 0);
        assert_eq!(running.rr_remaining(), 0);
        assert!(unsafe { ready.is_virgin() });
        assert!(unsafe { running.is_virgin() });

        let mut replacement = EEVDFScheduler::new();
        replacement.add_task(ready.clone()).unwrap();
        replacement.add_task(running.clone()).unwrap();
        assert_eq!(running.rr_remaining(), RR_TIMESLICE_TICKS);
    }

    #[test]
    fn scheduler_rt_sleep_marker_blocks_configuration_and_wakes() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(8));
        task.configure(rt(EevdfTaskClass::Fifo, 40)).unwrap();
        scheduler.add_task(task.clone()).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        scheduler.deactivate_task(&current, DeactivateReason::Sleep);
        assert_eq!(task.owner(), UNOWNED);
        assert_eq!(
            task.configure(rt(EevdfTaskClass::Fifo, 41)),
            Err(SchedulerError::AlreadyQueued)
        );
        scheduler
            .enqueue_task(task.clone(), EnqueueReason::Wakeup)
            .unwrap();
        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &task));
    }

    #[test]
    fn scheduler_foreign_and_sequence_errors_leave_tasks_untouched() {
        let mut owner = EEVDFScheduler::new();
        let mut foreign = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(10));
        owner.add_task(task.clone()).unwrap();
        let other = Arc::new(EEVDFTask::new(12));
        foreign.add_task(other).unwrap();
        assert!(matches!(
            foreign.remove_task(&task),
            Err(SchedulerError::ForeignQueue)
        ));
        assert!(Arc::ptr_eq(&owner.pick_next_task().unwrap(), &task));

        let mut exhausted = EEVDFScheduler::new();
        let first = Arc::new(EEVDFTask::new(13));
        exhausted.add_task(first).unwrap();
        exhausted.fair_sequence = i128::MAX;
        let second = Arc::new(EEVDFTask::new(14));
        assert_eq!(
            exhausted.add_task(second.clone()),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(second.owner(), UNOWNED);
        assert_eq!(exhausted.ready_tree.len(), 1);
    }

    #[test]
    fn scheduler_new_round_robin_restarts_slice_after_successful_admission() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        task.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        scheduler.add_task(task.clone()).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(task.rr_remaining(), RR_TIMESLICE_TICKS);
        assert!(!scheduler.task_tick(&running));
        let partial = task.rr_remaining();
        scheduler.put_prev_task(running, true).unwrap();

        let removed = scheduler.remove_task(&task).unwrap().unwrap();
        assert!(Arc::ptr_eq(&removed, &task));
        assert_eq!(task.rr_remaining(), partial);
        scheduler.add_task(removed).unwrap();
        assert_eq!(task.rr_remaining(), RR_TIMESLICE_TICKS);
    }

    #[test]
    fn scheduler_new_round_robin_insertion_error_preserves_old_slice() {
        let mut scheduler = EEVDFScheduler::new();
        let first = Arc::new(EEVDFTask::new(1));
        first.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        scheduler.add_task(first).unwrap();

        let second = Arc::new(EEVDFTask::new(2));
        second
            .configure(rt(EevdfTaskClass::RoundRobin, 20))
            .unwrap();
        second.set_rr_remaining(2);
        scheduler.rt_back_sequence = 0;
        assert_eq!(
            scheduler.add_task(second.clone()),
            Err(SchedulerError::InconsistentState)
        );
        assert_eq!(second.rr_remaining(), 2);
        assert_eq!(second.owner(), UNOWNED);
    }

    #[test]
    fn scheduler_initial_current_yield_is_lazily_adopted() {
        let mut scheduler = EEVDFScheduler::new();
        let current = Arc::new(EEVDFTask::new(9));
        scheduler.put_prev_task(current.clone(), false).unwrap();
        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &current));
    }

    #[test]
    #[should_panic(expected = "EEVDF task_tick current validation failed")]
    fn scheduler_task_tick_fail_stops_on_invalid_current() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        scheduler.add_task(task.clone()).unwrap();
        let _ = scheduler.task_tick(&task);
    }

    #[test]
    fn fair_key_keeps_deadline_separate_from_eligibility() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(11));
        task.configure(EevdfTaskParams {
            class: EevdfTaskClass::Batch,
            ..Default::default()
        })
        .unwrap();
        scheduler.add_task(task.clone()).unwrap();
        let node = scheduler.ready_tree.front().unwrap();
        // A fresh request starts eligible at V=0 but its deadline is one
        // virtual request length later.
        assert_eq!(unsafe { node.eligible_at() }, bias_i128(0));
        assert_eq!(unsafe { node.key().class_rank() }, FAIR_CLASS_RANK);
        assert!(unsafe { node.key().order() } > bias_i128(0));
    }

    #[test]
    fn eevdf_reservation_cancel_commit_and_exhaustion_retry() {
        let mut scheduler = EEVDFScheduler::new();
        let cancelled = Arc::new(EEVDFTask::new(1));
        let reservation = scheduler.reserve_new_task(&cancelled).unwrap();
        assert_eq!(cancelled.owner(), CONFIGURING);
        let returned = reservation.cancel().unwrap();
        assert!(Arc::ptr_eq(&returned, &cancelled));
        assert_eq!(cancelled.owner(), UNOWNED);

        let task = Arc::new(EEVDFTask::new(2));
        let reservation = scheduler.reserve_new_task(&task).unwrap();
        scheduler.fair_sequence = i128::MAX;
        scheduler.commit_reserved_task(reservation).unwrap();
        assert_eq!(task.owner(), scheduler.id);
        assert_eq!(scheduler.ready_tree.len(), 1);
    }

    #[test]
    fn eevdf_ready_migration_preserves_snapshot_and_rolls_back() {
        let task = Arc::new(EEVDFTask::new(1));
        let peer = Arc::new(EEVDFTask::new(2));
        let mut source = EEVDFScheduler::new();
        source.add_task(task.clone()).unwrap();
        source.add_task(peer.clone()).unwrap();
        let running = source.pick_next_task().unwrap();
        source.task_tick(&running);
        source.put_prev_task(running, true).unwrap();
        let before = source.clock;
        let migration = source.begin_ready_migration(&task).unwrap();
        assert_eq!(source.clock.total_weight, before.total_weight - 1024);
        let snapshot = unsafe {
            task.owned_state()
                .migration
                .expect("migration metadata missing")
                .snapshot
                .expect("fair migration snapshot missing")
        };
        let mut destination = EEVDFScheduler::new();
        let committed = destination.commit_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&committed, &task));
        let entity = unsafe { task.owned_state().entity.unwrap() };
        assert_eq!(entity.lag_at(destination.clock.v).unwrap(), snapshot.lag);

        let rollback_task = Arc::new(EEVDFTask::new(3));
        source.add_task(rollback_task.clone()).unwrap();
        let migration = source.begin_ready_migration(&rollback_task).unwrap();
        let restored = source.rollback_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&restored, &rollback_task));
        assert!(rollback_task.is_linked());
    }

    #[test]
    fn bounded_ready_query_accounts_rt_prefix_and_preserves_migration_choice() {
        let mut source = EEVDFScheduler::new();
        let rt_task = Arc::new(EEVDFTask::new(1));
        rt_task.configure(rt(EevdfTaskClass::Fifo, 20)).unwrap();
        let fair_task = Arc::new(EEVDFTask::new(2));
        source.add_task(rt_task.clone()).unwrap();
        source.add_task(fair_task.clone()).unwrap();

        let before_len = source.ready_len();
        let mut predicate_calls = 0;
        assert!(source
            .bounded_ready_candidate(1, |task| {
                predicate_calls += 1;
                *task.inner() == 2
            })
            .is_none());
        assert_eq!(predicate_calls, 1);
        assert_eq!(source.ready_len(), before_len);
        assert!(rt_task.is_linked());
        assert!(fair_task.is_linked());

        let candidate = source
            .bounded_ready_candidate(2, |task| *task.inner() == 2)
            .expect("fair task follows the RT prefix");
        assert_eq!(candidate.visited(), 2);
        assert_eq!(candidate.class(), EevdfTaskClass::Normal);
        assert_eq!(candidate.key().class_rank(), FAIR_CLASS_RANK);
        assert_eq!(source.ready_len(), before_len);
        assert!(fair_task.is_linked());

        let migration = source.begin_ready_migration(candidate.task()).unwrap();
        let mut destination = EEVDFScheduler::new();
        let committed = destination.commit_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&committed, &fair_task));
        assert_eq!(source.ready_len(), 1);
        assert_eq!(destination.ready_len(), 1);
        assert!(fair_task.is_linked());

        let rollback_task = Arc::new(EEVDFTask::new(3));
        source.add_task(rollback_task.clone()).unwrap();
        let candidate = source
            .bounded_ready_candidate(2, |task| *task.inner() == 3)
            .expect("rollback task follows the RT prefix");
        assert_eq!(candidate.visited(), 2);
        let migration = source.begin_ready_migration(candidate.task()).unwrap();
        let restored = source.rollback_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&restored, &rollback_task));
        assert_eq!(source.ready_len(), 2);
        assert!(rollback_task.is_linked());
    }

    #[test]
    fn bounded_ready_query_continuation_reaches_later_candidates() {
        let mut scheduler = EEVDFScheduler::new();
        for id in 0..5 {
            scheduler.add_task(Arc::new(EEVDFTask::new(id))).unwrap();
        }
        let mut cursor = EevdfReadyCursor::new();
        assert!(scheduler
            .bounded_ready_candidate_from(&mut cursor, 2, |_| false)
            .is_none());

        let mut candidate = None;
        let max_attempts = scheduler.ready_len().saturating_add(1);
        for _ in 0..max_attempts {
            if let Some(found) =
                scheduler.bounded_ready_candidate_from(&mut cursor, 2, |task| *task.inner() == 2)
            {
                candidate = Some(found);
                break;
            }
        }
        let candidate = candidate.expect("continuation must eventually reach the target");
        assert_eq!(*candidate.task().inner(), 2);
        assert!(candidate.visited() <= 2);
    }

    #[test]
    fn eevdf_ready_rollback_without_intervention_restores_exact_source_state() {
        let mut source = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        let peer = Arc::new(EEVDFTask::new(2));
        source.add_task(task.clone()).unwrap();
        source.add_task(peer).unwrap();

        let source_clock = source.clock;
        let source_entity = unsafe { task.owned_state().entity };
        let source_key = unsafe { task.key() };
        let source_eligible_at = unsafe { task.eligible_at() };
        let migration = source.begin_ready_migration(&task).unwrap();
        let metadata = unsafe { task.owned_state().migration.unwrap() };
        assert_eq!(metadata.source_clock, source_clock);
        assert_eq!(metadata.source_entity, source_entity);
        assert_eq!(metadata.detached_clock, source.clock);

        let restored = source.rollback_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&restored, &task));
        assert_eq!(source.clock, source_clock);
        assert_eq!(unsafe { task.owned_state().entity }, source_entity);
        assert_eq!(unsafe { task.key() }, source_key);
        assert_eq!(unsafe { task.eligible_at() }, source_eligible_at);
        assert!(task.is_linked());
    }

    #[test]
    fn eevdf_running_rollback_without_intervention_restores_exact_source_state_ready() {
        let mut source = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        let peer = Arc::new(EEVDFTask::new(2));
        source.add_task(task.clone()).unwrap();
        source.add_task(peer).unwrap();
        let task = source.pick_next_task().unwrap();

        let source_clock = source.clock;
        let source_entity = unsafe { task.owned_state().entity };
        let source_key = unsafe { task.key() };
        let source_eligible_at = unsafe { task.eligible_at() };
        let migration = source.begin_running_migration(&task).unwrap();
        let metadata = unsafe { task.owned_state().migration.unwrap() };
        assert_eq!(metadata.source_clock, source_clock);
        assert_eq!(metadata.source_entity, source_entity);
        assert_eq!(metadata.detached_clock, source.clock);

        let restored = source.rollback_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&restored, &task));
        assert!(source.running.is_none());
        assert_eq!(source.clock, source_clock);
        assert_eq!(unsafe { task.owned_state().entity }, source_entity);
        assert_eq!(unsafe { task.key() }, source_key);
        assert_eq!(unsafe { task.eligible_at() }, source_eligible_at);
        assert!(task.is_linked());
    }

    #[test]
    fn eevdf_intervening_tick_rollback_keeps_advanced_clock_and_rebases_entity() {
        let mut source = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        let peer = Arc::new(EEVDFTask::new(2));
        source.add_task(task.clone()).unwrap();
        source.add_task(peer).unwrap();
        let source_key = unsafe { task.key() };
        let migration = source.begin_ready_migration(&task).unwrap();
        let metadata = unsafe { task.owned_state().migration.unwrap() };
        let snapshot = metadata.snapshot.unwrap();
        let detached_clock = metadata.detached_clock;

        let running = source.pick_next_task().unwrap();
        assert!(!Arc::ptr_eq(&running, &task));
        assert!(!source.task_tick(&running));
        let advanced_clock = source.clock;
        assert_ne!(advanced_clock, detached_clock);

        let mut expected_clock = advanced_clock;
        expected_clock
            .checked_add_weight(snapshot.weight)
            .expect("expected rollback clock rebase should fit");
        let expected_entity =
            Entity::from_migration(snapshot, expected_clock.v).expect("snapshot should rebuild");
        let (expected_key, expected_eligible_at) =
            EEVDFScheduler::<u8>::fair_key(&expected_entity, source_key.sequence()).unwrap();

        let restored = source.rollback_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&restored, &task));
        assert_eq!(source.clock, expected_clock);
        assert_eq!(source.clock.accounted_ticks, advanced_clock.accounted_ticks);
        assert_eq!(unsafe { task.owned_state().entity }, Some(expected_entity));
        assert_eq!(unsafe { task.key() }, expected_key);
        assert_eq!(unsafe { task.eligible_at() }, expected_eligible_at);
        assert!(task.is_linked());
    }

    #[test]
    fn eevdf_two_migrations_preserve_and_then_consume_dormant_fair_state() {
        let task = Arc::new(EEVDFTask::new(1));
        let mut source = EEVDFScheduler::new();
        source.add_task(task.clone()).unwrap();
        let _ = source
            .set_task_params(&task, rt(EevdfTaskClass::Fifo, 20))
            .unwrap();

        let migration = source.begin_ready_migration(&task).unwrap();
        let parked = migration.park().unwrap();
        let migration = source.resume_migration(&parked).unwrap();
        let restored = source.rollback_migration(migration).unwrap();
        assert!(unsafe { restored.owned_state().dormant_fair.is_some() });

        let migration = source.begin_ready_migration(&restored).unwrap();
        let mut destination = EEVDFScheduler::new();
        destination.commit_migration(migration).unwrap();
        let _ = destination
            .set_task_params(&task, EevdfTaskParams::default())
            .unwrap();
        let state = unsafe { task.owned_state() };
        assert!(state.entity.is_some());
        assert!(state.dormant_fair.is_none());
        assert!(state.migration.is_none());
    }

    #[test]
    fn eevdf_parameter_updates_round_trip_fair_rt_and_sleeping() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        scheduler.add_task(task.clone()).unwrap();
        let _ = scheduler
            .set_task_params(&task, rt(EevdfTaskClass::RoundRobin, 20))
            .unwrap();
        assert_eq!(scheduler.clock.total_weight, 0);
        let _ = scheduler
            .set_task_params(&task, EevdfTaskParams::default())
            .unwrap();
        assert_eq!(scheduler.clock.total_weight, 1024);
        let current = scheduler.pick_next_task().unwrap();
        scheduler.deactivate_task(&current, DeactivateReason::Sleep);
        let _ = scheduler
            .set_task_params(
                &task,
                EevdfTaskParams {
                    class: EevdfTaskClass::Batch,
                    nice: 5,
                    rt_priority: 0,
                },
            )
            .unwrap();
        assert_eq!(task.sched_params().class, EevdfTaskClass::Batch);
        assert_eq!(scheduler.clock.total_weight, 0);
        scheduler
            .enqueue_task(task.clone(), EnqueueReason::Wakeup)
            .unwrap();
        assert_eq!(scheduler.clock.total_weight, 335);
    }

    #[test]
    fn eevdf_fork_seed_is_bounded_and_child_request_is_fresh() {
        let mut scheduler = EEVDFScheduler::new();
        let parent = Arc::new(EEVDFTask::new(1));
        scheduler.add_task(parent.clone()).unwrap();
        let seed = scheduler.fork_seed(&parent).unwrap();
        let child = Arc::new(EEVDFTask::new(2));
        child.install_fork_seed(seed).unwrap();
        let reservation = scheduler.reserve_new_task(&child).unwrap();
        scheduler.commit_reserved_task(reservation).unwrap();
        let child_entity = unsafe { child.owned_state().entity.unwrap() };
        assert_eq!(child_entity.request.remaining(), child_entity.request.q);
    }

    #[test]
    fn eevdf_fork_seed_is_rebounded_for_a_lower_credit_child_class() {
        let mut scheduler = EEVDFScheduler::new();
        let parent = Arc::new(EEVDFTask::new(1));
        parent
            .configure(EevdfTaskParams {
                class: EevdfTaskClass::Batch,
                ..Default::default()
            })
            .unwrap();
        scheduler.add_task(parent.clone()).unwrap();
        let mut seed = scheduler.fork_seed(&parent).unwrap();
        // Simulate a parent carrying the largest fair-class credit.  The
        // child remains Normal, whose cap is deliberately smaller.
        seed.lag = credit_cap(crate::eevdf_model::RequestClass::Batch);

        let child = Arc::new(EEVDFTask::new(2));
        child.install_fork_seed(seed).unwrap();
        scheduler.add_task(child.clone()).unwrap();
        let child_entity = unsafe { child.owned_state().entity.unwrap() };
        assert_eq!(child_entity.class, crate::eevdf_model::RequestClass::Normal);
        assert_eq!(
            child_entity.lag,
            credit_cap(crate::eevdf_model::RequestClass::Normal)
        );
    }

    #[test]
    fn eevdf_seeded_virgin_reconfiguration_to_rt_is_admissible() {
        let mut scheduler = EEVDFScheduler::new();
        let parent = Arc::new(EEVDFTask::new(1));
        scheduler.add_task(parent.clone()).unwrap();
        let seed = scheduler.fork_seed(&parent).unwrap();
        let child = Arc::new(EEVDFTask::new(2));
        child.install_fork_seed(seed).unwrap();

        let _ = scheduler
            .set_task_params(&child, rt(EevdfTaskClass::RoundRobin, 20))
            .unwrap();
        scheduler.add_task(child.clone()).unwrap();
        assert_eq!(child.sched_params().class, EevdfTaskClass::RoundRobin);
        assert_eq!(child.rr_remaining(), RR_TIMESLICE_TICKS);
    }

    #[test]
    fn eevdf_running_virgin_round_robin_adopts_full_slice() {
        let mut scheduler = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        task.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        scheduler.put_prev_task(task.clone(), false).unwrap();
        assert_eq!(task.rr_remaining(), RR_TIMESLICE_TICKS);
    }

    #[test]
    fn eevdf_trait_migration_parks_and_resumes_without_a_panic() {
        let mut source = EEVDFScheduler::new();
        let mut destination = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        source.add_task(task.clone()).unwrap();
        let parked = source.remove_task_for_migration(&task).unwrap().unwrap();
        assert!(Arc::ptr_eq(&parked, &task));
        assert_eq!(task.owner(), UNOWNED);
        source
            .enqueue_task(task.clone(), EnqueueReason::Migrate)
            .unwrap();
        assert!(task.is_linked());
        let parked = source.remove_task_for_migration(&task).unwrap().unwrap();
        assert!(Arc::ptr_eq(&parked, &task));
        assert_eq!(task.owner(), UNOWNED);
        destination
            .enqueue_task(task.clone(), EnqueueReason::Migrate)
            .unwrap();
        assert_ne!(task.owner(), UNOWNED);
    }

    #[test]
    fn eevdf_failed_ready_reconfigure_restores_node_and_clock() {
        let mut scheduler = EEVDFScheduler::new();
        let first = Arc::new(EEVDFTask::new(1));
        let second = Arc::new(EEVDFTask::new(2));
        scheduler.add_task(first.clone()).unwrap();
        scheduler.add_task(second.clone()).unwrap();
        let before_owner = second.owner();
        let before_clock = scheduler.clock;
        let before_key = unsafe { second.key() };
        let before_eligible_at = unsafe { second.eligible_at() };
        let before_state = unsafe { *second.owned_state() };
        let before_params = second.sched_params();
        let before_rr_remaining = second.rr_remaining();
        let before_ready_len = scheduler.ready_tree.len();
        scheduler.fair_sequence = i128::MAX;
        let before_fair_sequence = scheduler.fair_sequence;
        let before_rt_front_sequence = scheduler.rt_front_sequence;
        let before_rt_back_sequence = scheduler.rt_back_sequence;
        assert_eq!(
            scheduler.set_task_params(
                &second,
                EevdfTaskParams {
                    class: EevdfTaskClass::Batch,
                    ..Default::default()
                },
            ),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(second.owner(), before_owner);
        assert!(second.is_linked());
        assert_eq!(unsafe { second.key() }, before_key);
        assert_eq!(unsafe { second.eligible_at() }, before_eligible_at);
        assert_eq!(unsafe { *second.owned_state() }, before_state);
        assert_eq!(second.sched_params(), before_params);
        assert_eq!(second.rr_remaining(), before_rr_remaining);
        assert_eq!(scheduler.ready_tree.len(), before_ready_len);
        assert_eq!(scheduler.clock, before_clock);
        assert_eq!(scheduler.fair_sequence, before_fair_sequence);
        assert_eq!(scheduler.rt_front_sequence, before_rt_front_sequence);
        assert_eq!(scheduler.rt_back_sequence, before_rt_back_sequence);
        assert!(first.is_linked());
    }

    #[test]
    fn eevdf_running_migration_can_commit_or_rollback() {
        let mut source = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        source.add_task(task.clone()).unwrap();
        let current = source.pick_next_task().unwrap();
        source.task_tick(&current);
        let migration = source.begin_running_migration(&task).unwrap();
        assert_eq!(migration.origin(), EevdfMigrationOrigin::Running);
        assert_eq!(task.owner(), CONFIGURING);
        let mut destination = EEVDFScheduler::new();
        let destination_current = Arc::new(EEVDFTask::new(2));
        destination.add_task(destination_current.clone()).unwrap();
        let destination_current = destination.pick_next_task().unwrap();
        destination.commit_migration(migration).unwrap();
        assert!(destination
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, &destination_current)));
        assert!(task.is_linked());

        let migration = destination.begin_ready_migration(&task).unwrap();
        assert_eq!(migration.origin(), EevdfMigrationOrigin::Ready);
        let restored = destination.rollback_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&restored, &task));
        assert!(destination
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, &destination_current)));
        assert!(task.is_linked());

        let rollback_task = Arc::new(EEVDFTask::new(3));
        source.add_task(rollback_task.clone()).unwrap();
        let rollback_current = source.pick_next_task().unwrap();
        let migration = source.begin_running_migration(&rollback_current).unwrap();
        assert_eq!(migration.origin(), EevdfMigrationOrigin::Running);
        let restored = source.rollback_migration(migration).unwrap();
        assert!(Arc::ptr_eq(&restored, &rollback_task));
        assert!(source.running.is_none());
        assert!(rollback_task.is_linked());
    }

    #[test]
    fn eevdf_param_update_reports_post_commit_preemption() {
        let mut scheduler = EEVDFScheduler::new();
        let current = Arc::new(EEVDFTask::new(1));
        let ready = Arc::new(EEVDFTask::new(2));
        scheduler.add_task(current.clone()).unwrap();
        let current = scheduler.pick_next_task().unwrap();
        scheduler.add_task(ready.clone()).unwrap();

        let outcome = scheduler
            .set_task_params(&ready, rt(EevdfTaskClass::Fifo, 20))
            .unwrap();
        assert!(matches!(
            &outcome,
            EevdfParamUpdate::PreemptCurrent(task) if Arc::ptr_eq(task, &current)
        ));
        assert!(outcome.requests_preemption());

        let mut running_downgrade = EEVDFScheduler::new();
        let running = Arc::new(EEVDFTask::new(3));
        running.configure(rt(EevdfTaskClass::Fifo, 20)).unwrap();
        let peer = Arc::new(EEVDFTask::new(4));
        peer.configure(rt(EevdfTaskClass::Fifo, 20)).unwrap();
        running_downgrade.add_task(running.clone()).unwrap();
        running_downgrade.add_task(peer).unwrap();
        let running = running_downgrade.pick_next_task().unwrap();
        let outcome = running_downgrade
            .set_task_params(&running, EevdfTaskParams::default())
            .unwrap();
        assert!(matches!(
            &outcome,
            EevdfParamUpdate::PreemptCurrent(task) if Arc::ptr_eq(task, &running)
        ));

        let mut unchanged = EEVDFScheduler::new();
        let only = Arc::new(EEVDFTask::new(5));
        unchanged.add_task(only).unwrap();
        let only = unchanged.pick_next_task().unwrap();
        assert_eq!(
            unchanged.set_task_params(&only, EevdfTaskParams::default()),
            Ok(EevdfParamUpdate::NoPreemption)
        );
    }

    #[test]
    fn eevdf_running_rr_migration_restarts_an_exhausted_slice_as_ready() {
        let mut source = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        task.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        let peer = Arc::new(EEVDFTask::new(2));
        peer.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        source.add_task(task).unwrap();
        source.add_task(peer).unwrap();
        let current = source.pick_next_task().unwrap();
        for _ in 0..(RR_TIMESLICE_TICKS - 1) {
            assert!(!source.task_tick(&current));
        }
        assert!(source.task_tick(&current));
        assert_eq!(current.rr_remaining(), 0);
        let migration = source.begin_running_migration(&current).unwrap();
        let mut destination = EEVDFScheduler::new();
        let restored = destination.commit_migration(migration).unwrap();
        assert!(restored.is_linked());
        assert_eq!(restored.rr_remaining(), RR_TIMESLICE_TICKS);
        assert!(destination.running.is_none());

        let mut rollback = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(3));
        task.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        let peer = Arc::new(EEVDFTask::new(4));
        peer.configure(rt(EevdfTaskClass::RoundRobin, 20)).unwrap();
        rollback.add_task(task).unwrap();
        rollback.add_task(peer).unwrap();
        let current = rollback.pick_next_task().unwrap();
        for _ in 0..(RR_TIMESLICE_TICKS - 1) {
            assert!(!rollback.task_tick(&current));
        }
        assert!(rollback.task_tick(&current));
        let migration = rollback.begin_running_migration(&current).unwrap();
        let restored = rollback.rollback_migration(migration).unwrap();
        assert!(restored.is_linked());
        assert_eq!(restored.rr_remaining(), RR_TIMESLICE_TICKS);
        assert!(rollback.running.is_none());
    }

    #[test]
    fn eevdf_failed_migration_commit_keeps_live_token_and_destination() {
        let mut source = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        source.add_task(task.clone()).unwrap();
        let migration = source.begin_ready_migration(&task).unwrap();
        let mut destination = EEVDFScheduler::new();
        let existing = Arc::new(EEVDFTask::new(2));
        destination.add_task(existing).unwrap();
        destination.fair_sequence = 0;
        let before_clock = destination.clock;
        let error = match destination.commit_migration(migration) {
            Ok(_) => panic!("duplicate migration key unexpectedly committed"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), SchedulerError::InconsistentState);
        assert_eq!(destination.clock, before_clock);
        let migration = error.into_migration();
        assert_eq!(migration.task().owner(), CONFIGURING);
        source.rollback_migration(migration).unwrap();
        assert!(task.is_linked());
    }

    #[test]
    fn eevdf_failed_destination_retry_reuses_scheduler_identity() {
        let mut source = EEVDFScheduler::new();
        let task = Arc::new(EEVDFTask::new(1));
        source.add_task(task.clone()).unwrap();
        let migration = source.begin_ready_migration(&task).unwrap();

        let mut destination = EEVDFScheduler::new();
        destination.fair_sequence = i128::MAX;
        let first = match destination.commit_migration(migration) {
            Ok(_) => panic!("migration unexpectedly committed"),
            Err(error) => error,
        };
        let destination_id = destination.id;
        assert_ne!(destination_id, UNOWNED);
        let second = match destination.commit_migration(first.into_migration()) {
            Ok(_) => panic!("migration unexpectedly committed on retry"),
            Err(error) => error,
        };
        assert_eq!(destination.id, destination_id);
        assert_eq!(second.kind(), SchedulerError::SequenceExhausted);
        source.rollback_migration(second.into_migration()).unwrap();
    }
}
