//! EEVDF task representation and single-runqueue scheduler core.
//!
//! The task boundary owns the atomically published scheduling tuple,
//! intrusive node, and scheduler-owned state slot.  [`EEVDFScheduler`] adds
//! the one-tree ready path and lifecycle policy around that representation.

use core::{
    cell::UnsafeCell,
    ops::Deref,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use crate::{
    allocate_scheduler_id,
    cfs::{RR_TIMESLICE_TICKS, RT_PRIORITY_MAX, RT_PRIORITY_MIN},
    eevdf_model::{bias_i128, Clock, Entity, MigrationSnapshot, ModelError},
    eevdf_tree::{EevdfNode, EevdfTree, EevdfTreeError},
    BaseScheduler, DeactivateReason, EnqueueReason, SchedulerError, CONFIGURING, UNOWNED,
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

/// Scheduler-owned fair state.  Access requires the queue-owner/rq-lock
/// contract described by [`EevdfTaskPayload::owned_state_mut`].
pub(crate) struct EevdfOwnedState {
    pub(crate) entity: Option<Entity>,
    pub(crate) migration: Option<MigrationSnapshot>,
    /// A sleeping RT task has no fair `Entity`; retain an explicit lifecycle
    /// bit so it cannot be mistaken for a virgin task after owner release.
    pub(crate) rt_sleeping: bool,
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
                migration: None,
                rt_sleeping: false,
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
            let _ = self.transfer_owner(CONFIGURING, UNOWNED);
            return Err(SchedulerError::AlreadyQueued);
        }
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
            state.entity.is_none() && state.migration.is_none() && !state.rt_sleeping
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
    pub(crate) const fn clock(&self) -> Clock {
        self.clock
    }

    /// Returns the number of ready tasks.  The running task is not linked.
    pub(crate) const fn ready_len(&self) -> usize {
        self.ready_tree.len()
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

    fn stage_key(task: &Arc<EEVDFTask<T>>, key: EevdfReadyKey, eligible_at: u128) {
        // SAFETY: every caller holds this scheduler's logical task ownership,
        // and the task is unlinked while the scheduler lock/exclusion is held.
        unsafe { task.stage_unlinked(key, eligible_at) };
    }

    fn fair_weight(params: EevdfTaskParams) -> Result<u128, SchedulerError> {
        eevdf_weight_for(params).ok_or(SchedulerError::InvalidParameters)
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

    fn release_claim(&self, task: &Arc<EEVDFTask<T>>) {
        task.transfer_owner(self.id, UNOWNED)
            .expect("EEVDF task owner changed while a claim was held");
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
            let entity = match Entity::new(class, weight, next_clock.total_weight, next_clock.v)
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

        if matches!(params.class, EevdfTaskClass::RoundRobin) {
            task.set_rr_remaining(RR_TIMESLICE_TICKS);
        }

        // SAFETY: the task is owned by this scheduler and remains behind the
        // runqueue exclusion while its entity is published.
        unsafe {
            task.owned_state_mut().entity = entity;
            task.owned_state_mut().migration = None;
            task.owned_state_mut().rt_sleeping = false;
        }
        self.clock = next_clock;
        Ok(())
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
        let old_rt_sleeping = unsafe { task.owned_state().rt_sleeping };
        let (next_clock, entity, key, eligible_at, reset_rr) = if let Some(entity) = old_entity {
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
            if let Err(error) = next.wake(self.clock.v).map_err(model_error) {
                self.release_claim(&task);
                return Err(error);
            }
            let class = match fair_class(params.class) {
                Some(class) => class,
                None => {
                    self.release_claim(&task);
                    return Err(SchedulerError::IncompatibleClass);
                }
            };
            if let Err(error) = next
                .reconfigure(class, weight, next_clock.total_weight, next_clock.v)
                .map_err(model_error)
            {
                self.release_claim(&task);
                return Err(error);
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
            (next_clock, Some(next), key, eligible_at, false)
        } else if is_rt_class(params.class) && old_rt_sleeping {
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
            task.set_rr_remaining(RR_TIMESLICE_TICKS);
        }
        // SAFETY: owner and tree exclusion are held for the complete state
        // publication.
        unsafe {
            task.owned_state_mut().entity = entity;
            task.owned_state_mut().rt_sleeping = false;
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
            let entity = match Entity::new(
                fair_class(params.class).expect("fair class conversion failed"),
                weight,
                next_clock.total_weight,
                next_clock.v,
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
            task.owned_state_mut().rt_sleeping = false;
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
            prev.set_rr_remaining(RR_TIMESLICE_TICKS);
        }
        // SAFETY: the task is still owned by this scheduler and is now
        // linked in its one ready tree.
        unsafe {
            prev.owned_state_mut().entity = next_entity;
            prev.owned_state_mut().rt_sleeping = false;
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
            removed.owned_state_mut().migration = None;
            removed.owned_state_mut().rt_sleeping = false;
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

    fn tick_fair(&mut self, current: &Arc<EEVDFTask<T>>) -> bool {
        let entity =
            unsafe { current.owned_state().entity }.expect("EEVDF fair current has no entity");
        if entity.request.remaining() == 0 {
            if !self.ready_tree.is_empty() {
                return true;
            }
            let mut renewed = entity;
            renewed
                .renew(self.clock.total_weight, self.clock.v)
                .expect("EEVDF infallible tick renewal exhausted arithmetic");
            // No ready competitor exists, so renewal is committed in place.
            unsafe { current.owned_state_mut().entity = Some(renewed) };
        }

        let mut next_clock = self.clock;
        let mut next_entity =
            unsafe { current.owned_state().entity }.expect("EEVDF fair current entity disappeared");
        next_entity
            .tick_service(&mut next_clock, 1)
            .expect("EEVDF infallible tick service exhausted arithmetic");
        self.clock = next_clock;
        unsafe { current.owned_state_mut().entity = Some(next_entity) };

        if self.has_ready_rt() {
            return true;
        }
        let current_eligible = next_entity
            .is_eligible(self.clock.v)
            .expect("EEVDF infallible tick eligibility check exhausted arithmetic");
        if current_eligible {
            let deadline = next_entity.request.deadline;
            if self.has_earlier_eligible_fair(deadline) {
                return true;
            }
        } else if self
            .ready_tree
            .peek_earliest_eligible(bias_i128(self.clock.v))
            .is_some()
        {
            return true;
        }
        if next_entity.request.remaining() == 0 {
            if !self.ready_tree.is_empty() {
                return true;
            }
            let mut renewed = next_entity;
            renewed
                .renew(self.clock.total_weight, self.clock.v)
                .expect("EEVDF infallible tick renewal exhausted arithmetic");
            unsafe { current.owned_state_mut().entity = Some(renewed) };
        }
        false
    }

    fn tick_rt(&mut self, current: &Arc<EEVDFTask<T>>) -> bool {
        let params = current.sched_params();
        if self.has_higher_rt(params.rt_priority) {
            return true;
        }
        if matches!(params.class, EevdfTaskClass::Fifo) {
            return false;
        }
        let remaining = current.rr_remaining();
        if remaining <= 1 {
            current.set_rr_remaining(0);
            if self.has_same_rt(params.rt_priority) {
                return true;
            }
            current.set_rr_remaining(RR_TIMESLICE_TICKS);
        } else {
            current.set_rr_remaining(remaining - 1);
        }
        false
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
        _task: &Self::SchedItem,
    ) -> Result<Option<Self::SchedItem>, SchedulerError> {
        Err(SchedulerError::UnsupportedOperation)
    }

    fn deactivate_task(&mut self, task: &Self::SchedItem, reason: DeactivateReason) {
        if matches!(reason, DeactivateReason::Migrate) {
            panic!("EEVDF migration is not implemented in the single-runqueue core");
        }
        self.ensure_running(task)
            .expect("EEVDF deactivate current validation failed");
        match reason {
            DeactivateReason::Sleep => {
                let params = task.sched_params();
                if is_rt_class(params.class) {
                    // RT tasks have no fair Entity, so the marker is the
                    // state that distinguishes sleeping from virgin.
                    unsafe { task.owned_state_mut().rt_sleeping = true };
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
                    task.owned_state_mut().migration = None;
                    task.owned_state_mut().rt_sleeping = false;
                }
                self.clock = next_clock;
                self.running = None;
                task.transfer_owner(self.id, UNOWNED)
                    .expect("EEVDF running task owner invariant violated during exit");
            }
            DeactivateReason::Migrate => unreachable!("migration handled above"),
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
            EnqueueReason::Migrate => Err(SchedulerError::UnsupportedOperation),
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

    fn set_priority(
        &mut self,
        _task: &Self::SchedItem,
        _prio: isize,
    ) -> Result<(), SchedulerError> {
        Err(SchedulerError::UnsupportedOperation)
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
                task.owned_state_mut().migration = None;
                task.owned_state_mut().rt_sleeping = false;
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
                task.owned_state_mut().migration = None;
                task.owned_state_mut().rt_sleeping = false;
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
}
