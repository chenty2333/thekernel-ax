use alloc::sync::Arc;
use core::{
    fmt,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, AtomicUsize, Ordering},
};

use intrusive_collections::{intrusive_adapter, Bound, KeyAdapter, RBTree, RBTreeAtomicLink};

use crate::{
    allocate_scheduler_id, BaseScheduler, DeactivateReason, EnqueueReason, SchedulerError,
    CONFIGURING, UNOWNED,
};

/// Default tick budget assigned to a round-robin task.
pub const RR_TIMESLICE_TICKS: usize = 5;
/// Lowest valid real-time priority in the generic scheduler domain.
pub const RT_PRIORITY_MIN: u8 = 1;
/// Highest valid real-time priority in the generic scheduler domain.
///
/// ABI adapters may expose a narrower range. For example, a Linux personality
/// validates its own userspace range instead of changing this mechanism limit.
pub const RT_PRIORITY_MAX: u8 = u8::MAX;
const FAIR_PREEMPT_GRANULARITY_TICKS: isize = 2;
type ReadyKey = (u8, isize, isize);

/// Runtime scheduling class for CFS tasks.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CfsTaskClass {
    /// Ordinary fair scheduling.
    Normal = 0,
    /// Fair scheduling biased toward throughput over latency.
    Batch = 1,
    /// Lowest-precedence fair scheduling.
    Idle = 2,
    /// Fixed-priority, time-sliced scheduling.
    RoundRobin = 3,
    /// Fixed-priority scheduling without time slicing.
    Fifo = 4,
}

/// Runtime scheduling parameters for a CFS task.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CfsTaskParams {
    /// Scheduling class.
    pub class: CfsTaskClass,
    /// Fair-class weight selector in the inclusive range `-20..=19`.
    pub nice: i8,
    /// Real-time priority in the inclusive range
    /// [`RT_PRIORITY_MIN`]`..=`[`RT_PRIORITY_MAX`].
    pub rt_priority: u8,
}

impl Default for CfsTaskParams {
    fn default() -> Self {
        Self {
            class: CfsTaskClass::Normal,
            nice: 0,
            rt_priority: 0,
        }
    }
}

impl CfsTaskParams {
    fn validated(mut self) -> Option<Self> {
        match self.class {
            CfsTaskClass::Idle => {
                self.nice = NICE_RANGE_POS as i8;
                self.rt_priority = 0;
            }
            CfsTaskClass::Normal | CfsTaskClass::Batch => {
                self.rt_priority = 0;
            }
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo => {
                self.nice = 0;
            }
        }
        let valid = match self.class {
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo => {
                (RT_PRIORITY_MIN..=RT_PRIORITY_MAX).contains(&self.rt_priority)
            }
            CfsTaskClass::Normal | CfsTaskClass::Batch | CfsTaskClass::Idle => {
                (-20..=19).contains(&(self.nice as isize))
            }
        };
        valid.then_some(self)
    }

    const fn packed(self) -> u32 {
        (self.class as u32) | ((self.nice as u8 as u32) << 8) | ((self.rt_priority as u32) << 16)
    }

    fn from_packed(value: u32) -> Self {
        let class = match value as u8 {
            0 => CfsTaskClass::Normal,
            1 => CfsTaskClass::Batch,
            2 => CfsTaskClass::Idle,
            3 => CfsTaskClass::RoundRobin,
            4 => CfsTaskClass::Fifo,
            // The word is private and every writer stores a validated class.
            // Contain a hardware/memory fault without indexing outside the
            // weight tables; safe callers cannot manufacture this value.
            _ => CfsTaskClass::Normal,
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

/// task for CFS
pub struct CFSTask<T> {
    inner: T,
    /// Intrusive ready-queue membership. Keeping the tree node in the task
    /// makes every enqueue, wakeup, yield and preemption allocation-free.
    ready_link: RBTreeAtomicLink,
    /// Immutable-for-one-link-lifetime ordering snapshot. Scheduler parameters
    /// may be inspected or changed concurrently, so the intrusive tree must not
    /// derive its structural key from those live atomics while this link is in a
    /// ready queue.
    ready_class: AtomicU8,
    ready_order: AtomicIsize,
    init_vruntime: AtomicIsize,
    delta: AtomicIsize,
    seeded_vruntime: AtomicBool,
    /// One atomic publication word for the class, nice value, and real-time
    /// priority. Readers must never observe a new class paired with an old
    /// class-specific priority during a concurrent safe configuration.
    params: AtomicU32,
    rr_time_slice: AtomicIsize,
    /// Fair vruntime relative to the source run queue's floor while a task is
    /// between run queues. This avoids treating migration as a sleeper wakeup
    /// and keeps the transfer allocation-free.
    migration_vruntime_offset: AtomicIsize,
    migration_vruntime_offset_valid: AtomicBool,
    id: AtomicIsize,
    queue_owner: AtomicUsize,
}

/// Short unqueued-task configuration transaction.
struct CfsConfigurationClaim<'a, T> {
    task: &'a CFSTask<T>,
    active: bool,
}

impl<T> CfsConfigurationClaim<'_, T> {
    fn finish(mut self) -> Result<(), SchedulerError> {
        self.task.transfer_owner(CONFIGURING, UNOWNED)?;
        self.active = false;
        Ok(())
    }
}

impl<T> Drop for CfsConfigurationClaim<'_, T> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.task.transfer_owner(CONFIGURING, UNOWNED);
        }
    }
}

// https://elixir.bootlin.com/linux/latest/source/include/linux/sched/prio.h

const NICE_RANGE_POS: usize = 19; // MAX_NICE in Linux
const NICE_RANGE_NEG: usize = 20; // -MIN_NICE in Linux, the range of nice is [MIN_NICE, MAX_NICE]
                                  // https://elixir.bootlin.com/linux/latest/source/kernel/sched/core.c

const NICE2WEIGHT_POS: [isize; NICE_RANGE_POS + 1] = [
    1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
];
const NICE2WEIGHT_NEG: [isize; NICE_RANGE_NEG + 1] = [
    1024, 1277, 1586, 1991, 2501, 3121, 3906, 4904, 6100, 7620, 9548, 11916, 14949, 18705, 23254,
    29154, 36291, 46273, 56483, 71755, 88761,
];

impl<T> CFSTask<T> {
    /// new with default values
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            ready_link: RBTreeAtomicLink::new(),
            ready_class: AtomicU8::new(1),
            ready_order: AtomicIsize::new(0),
            init_vruntime: AtomicIsize::new(0_isize),
            delta: AtomicIsize::new(0_isize),
            seeded_vruntime: AtomicBool::new(false),
            params: AtomicU32::new(
                CfsTaskParams {
                    class: CfsTaskClass::Normal,
                    nice: 0,
                    rt_priority: 0,
                }
                .packed(),
            ),
            rr_time_slice: AtomicIsize::new(RR_TIMESLICE_TICKS as isize),
            migration_vruntime_offset: AtomicIsize::new(0),
            migration_vruntime_offset_valid: AtomicBool::new(false),
            id: AtomicIsize::new(0_isize),
            queue_owner: AtomicUsize::new(UNOWNED),
        }
    }

    fn claim(&self, scheduler_id: usize) -> Result<(), SchedulerError> {
        match self.queue_owner.compare_exchange(
            UNOWNED,
            scheduler_id,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(owner) if owner == scheduler_id => Err(SchedulerError::AlreadyQueued),
            Err(CONFIGURING) => Err(SchedulerError::TaskBusy),
            Err(_) => Err(SchedulerError::ForeignQueue),
        }
    }

    fn transfer_owner(&self, from: usize, to: usize) -> Result<(), SchedulerError> {
        self.queue_owner
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| SchedulerError::InconsistentState)
    }

    fn owner(&self) -> usize {
        self.queue_owner.load(Ordering::Acquire)
    }

    fn claim_configuration(&self) -> Result<CfsConfigurationClaim<'_, T>, SchedulerError> {
        self.queue_owner
            .compare_exchange(UNOWNED, CONFIGURING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|owner| {
                if owner == CONFIGURING {
                    SchedulerError::TaskBusy
                } else {
                    SchedulerError::AlreadyQueued
                }
            })?;
        Ok(CfsConfigurationClaim {
            task: self,
            active: true,
        })
    }

    /// Consumes the scheduler wrapper and returns the inner task.
    pub fn into_inner(self) -> T {
        self.inner
    }

    fn effective_nice(params: CfsTaskParams) -> isize {
        match params.class {
            CfsTaskClass::Idle => NICE_RANGE_POS as isize,
            CfsTaskClass::Normal | CfsTaskClass::Batch => params.nice as isize,
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo => 0,
        }
    }

    fn weight_for(params: CfsTaskParams) -> isize {
        let nice = Self::effective_nice(params);
        if nice >= 0 {
            NICE2WEIGHT_POS[nice as usize]
        } else {
            NICE2WEIGHT_NEG[(-nice) as usize]
        }
    }

    fn get_id(&self) -> isize {
        self.id.load(Ordering::Acquire)
    }

    fn get_vruntime_with(&self, params: CfsTaskParams) -> isize {
        let weight = Self::weight_for(params);
        if weight == 1024 {
            self.init_vruntime
                .load(Ordering::Acquire)
                .saturating_add(self.delta.load(Ordering::Acquire))
        } else {
            self.init_vruntime
                .load(Ordering::Acquire)
                .saturating_add(self.delta.load(Ordering::Acquire).saturating_mul(1024) / weight)
        }
    }

    fn rebase_vruntime(&self, v: isize) {
        self.init_vruntime.store(v, Ordering::Release);
        self.delta.store(0, Ordering::Release);
    }

    fn stage_migration_vruntime_offset(&self, source_floor: isize) {
        let params = self.load_sched_params();
        if is_realtime(params) {
            self.migration_vruntime_offset_valid
                .store(false, Ordering::Release);
            return;
        }
        let offset = self.get_vruntime_with(params).saturating_sub(source_floor);
        self.migration_vruntime_offset
            .store(offset, Ordering::Relaxed);
        self.migration_vruntime_offset_valid
            .store(true, Ordering::Release);
    }

    fn take_migration_vruntime(&self, destination_floor: isize) -> isize {
        if self
            .migration_vruntime_offset_valid
            .swap(false, Ordering::AcqRel)
        {
            destination_floor.saturating_add(self.migration_vruntime_offset.load(Ordering::Acquire))
        } else {
            self.get_vruntime_with(self.load_sched_params())
                .max(destination_floor)
        }
    }

    fn clear_migration_vruntime_offset(&self) {
        self.migration_vruntime_offset_valid
            .store(false, Ordering::Release);
    }

    fn rr_time_slice(&self) -> isize {
        self.rr_time_slice.load(Ordering::Acquire)
    }

    fn reset_rr_time_slice(&self) {
        self.rr_time_slice
            .store(RR_TIMESLICE_TICKS as isize, Ordering::Release);
    }

    fn task_tick_rr(&self) -> isize {
        self.rr_time_slice
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |slice| {
                Some(slice.saturating_sub(1))
            })
            .unwrap_or(0)
    }

    fn set_sched_params(&self, params: CfsTaskParams) {
        let current = self.load_sched_params();
        let current_vruntime = self.get_vruntime_with(current);
        self.rebase_vruntime(current_vruntime);
        if matches!(params.class, CfsTaskClass::RoundRobin | CfsTaskClass::Fifo) {
            self.reset_rr_time_slice();
        } else {
            self.rr_time_slice.store(0, Ordering::Release);
        }
        self.params.store(params.packed(), Ordering::Release);
    }

    fn set_id(&self, id: isize) {
        self.id.store(id, Ordering::Release);
    }

    fn task_tick(&self) {
        let _ = self
            .delta
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |delta| {
                Some(delta.saturating_add(1))
            });
    }

    fn stage_ready_key(&self, class: u8, order: isize, sequence: isize) {
        // Callers hold the destination scheduler lock and invoke this only
        // while `ready_link` is unlinked. Once insertion publishes the link,
        // these three fields remain untouched until that link is removed.
        self.ready_class.store(class, Ordering::Release);
        self.ready_order.store(order, Ordering::Release);
        self.set_id(sequence);
    }

    fn ready_key(&self) -> ReadyKey {
        (
            self.ready_class.load(Ordering::Acquire),
            self.ready_order.load(Ordering::Acquire),
            self.get_id(),
        )
    }

    fn ready_is_rt(&self) -> bool {
        self.ready_class.load(Ordering::Acquire) == 0
    }

    /// Returns a reference to the inner task struct.
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Returns the current scheduling parameters.
    pub fn sched_params(&self) -> CfsTaskParams {
        self.load_sched_params()
    }

    fn load_sched_params(&self) -> CfsTaskParams {
        CfsTaskParams::from_packed(self.params.load(Ordering::Acquire))
    }

    fn snapshot_reconfiguration_state(&self) -> CfsTaskStateSnapshot {
        CfsTaskStateSnapshot {
            params: self.load_sched_params(),
            ready_key: self.ready_key(),
            init_vruntime: self.init_vruntime.load(Ordering::Acquire),
            delta: self.delta.load(Ordering::Acquire),
            seeded_vruntime: self.seeded_vruntime.load(Ordering::Acquire),
            rr_time_slice: self.rr_time_slice.load(Ordering::Acquire),
        }
    }

    fn restore_reconfiguration_state(&self, snapshot: CfsTaskStateSnapshot) {
        self.init_vruntime
            .store(snapshot.init_vruntime, Ordering::Release);
        self.delta.store(snapshot.delta, Ordering::Release);
        self.seeded_vruntime
            .store(snapshot.seeded_vruntime, Ordering::Release);
        self.rr_time_slice
            .store(snapshot.rr_time_slice, Ordering::Release);
        self.params
            .store(snapshot.params.packed(), Ordering::Release);
        self.stage_ready_key(
            snapshot.ready_key.0,
            snapshot.ready_key.1,
            snapshot.ready_key.2,
        );
    }

    /// Applies the given scheduling parameters to the task.
    ///
    /// Returns a typed error if the parameters are invalid or the task is
    /// currently linked into a scheduler. Use
    /// [`CFScheduler::set_task_params`] to update a ready task and reestablish
    /// its queue ordering atomically.
    pub fn configure(&self, params: CfsTaskParams) -> Result<(), SchedulerError> {
        let params = params
            .validated()
            .ok_or(SchedulerError::InvalidParameters)?;
        let claim = self.claim_configuration()?;
        self.apply_validated(params);
        claim.finish()
    }

    fn apply_validated(&self, params: CfsTaskParams) {
        self.set_sched_params(params);
    }

    fn configure_nice(&self, nice: i8) -> Result<(), SchedulerError> {
        let claim = self.claim_configuration()?;
        let current = self.load_sched_params();
        if is_realtime(current) {
            return Err(SchedulerError::IncompatibleClass);
        }
        let next = CfsTaskParams {
            class: current.class,
            nice,
            rt_priority: 0,
        }
        .validated()
        .expect("validated nice and fair class produce valid scheduler parameters");
        self.apply_validated(next);
        claim.finish()
    }

    /// Seeds a new fair child just behind its parent task.
    ///
    /// This is a generic spawn-fairness mechanism. Whether a child inherits or
    /// resets scheduling policy is a lifecycle decision for the caller.
    pub fn inherit_fair_vruntime_from(&self, parent: &Self) -> Result<(), SchedulerError> {
        let claim = self.claim_configuration()?;
        let child_params = self.load_sched_params();
        let parent_params = parent.load_sched_params();
        if is_realtime(child_params) || is_realtime(parent_params) {
            return Err(SchedulerError::IncompatibleClass);
        }
        self.rebase_vruntime(
            parent
                .get_vruntime_with(parent_params)
                .saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS),
        );
        self.seeded_vruntime.store(true, Ordering::Release);
        claim.finish()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct CfsTaskStateSnapshot {
    params: CfsTaskParams,
    ready_key: ReadyKey,
    init_vruntime: isize,
    delta: isize,
    seeded_vruntime: bool,
    rr_time_slice: isize,
}

impl<T> Deref for CFSTask<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

intrusive_adapter!(ReadyTaskAdapter<T> = Arc<CFSTask<T>>: CFSTask<T> {
    ready_link: RBTreeAtomicLink
});

impl<'a, T> KeyAdapter<'a> for ReadyTaskAdapter<T> {
    type Key = ReadyKey;

    fn get_key(&self, task: &'a CFSTask<T>) -> Self::Key {
        task.ready_key()
    }
}

fn rt_priority_key(priority: u8) -> isize {
    (RT_PRIORITY_MAX - priority) as isize
}

const fn is_realtime(params: CfsTaskParams) -> bool {
    matches!(params.class, CfsTaskClass::RoundRobin | CfsTaskClass::Fifo)
}

/// A simple [Completely Fair Scheduler][1] (CFS).
///
/// [1]: https://en.wikipedia.org/wiki/Completely_Fair_Scheduler
pub struct CFScheduler<T> {
    /// A single intrusive tree keeps real-time tasks before fair tasks while
    /// preserving the ordering within both classes. Unlike `BTreeMap`, it
    /// cannot allocate at the runnable-publication point.
    ready_queue: RBTree<ReadyTaskAdapter<T>>,
    min_vruntime: Option<isize>,
    id: usize,
    fair_sequence: isize,
    rt_front_seq: isize,
    rt_back_seq: isize,
}

#[derive(Clone, Copy)]
enum CfsEnqueueAction {
    New,
    Wakeup,
    ExplicitYield,
    Requeue { preempt: bool },
    Migrate,
}

/// A queue-owner claim acquired before any enqueue-side task state changes.
struct CfsEnqueueClaim<T> {
    task: Option<Arc<CFSTask<T>>>,
    scheduler_id: usize,
}

impl<T> CfsEnqueueClaim<T> {
    fn task(&self) -> &Arc<CFSTask<T>> {
        self.task
            .as_ref()
            .expect("live CFS enqueue claim always owns its task")
    }

    fn publish(mut self) -> Arc<CFSTask<T>> {
        self.task
            .take()
            .expect("live CFS enqueue claim always owns its task")
    }
}

impl<T> Drop for CfsEnqueueClaim<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.transfer_owner(self.scheduler_id, UNOWNED);
        }
    }
}

/// One unpublished CFS ready-queue admission.
///
/// The reservation claims the task's private queue-ownership word and one
/// scheduler-local ordering sequence while the destination scheduler is
/// locked. It does not link the task into the ready tree. Dropping it releases
/// the claim. Ordering sequences are monotonic and never reused, so this token
/// remains unique even if later admissions exhaust their sequence domain.
#[must_use = "dropping the reservation cancels runnable-task publication"]
pub struct CfsTaskReservation<T> {
    task: Option<Arc<CFSTask<T>>>,
    scheduler_id: usize,
    sequence: isize,
    params: CfsTaskParams,
}

impl<T> CfsTaskReservation<T> {
    /// Returns the exact unpublished task retained by this reservation.
    pub fn task(&self) -> &Arc<CFSTask<T>> {
        self.task
            .as_ref()
            .expect("live CFS reservation always owns its task")
    }

    /// Cancels this reservation and returns the exact unpublished task.
    pub fn cancel(mut self) -> Arc<CFSTask<T>> {
        let task = self
            .task
            .take()
            .expect("live CFS reservation always owns its task");
        task.transfer_owner(CONFIGURING, UNOWNED)
            .expect("private CFS reservation ownership changed before cancellation");
        task
    }
}

impl<T> fmt::Debug for CfsTaskReservation<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CfsTaskReservation")
            .field("scheduler_id", &self.scheduler_id)
            .field("sequence", &self.sequence)
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl<T> Drop for CfsTaskReservation<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // A safe caller cannot change queue ownership while CONFIGURING is
            // held. If unsafe code corrupted it, retain the foreign owner
            // rather than overwriting scheduler state during cancellation.
            let _ = task.transfer_owner(CONFIGURING, UNOWNED);
        }
    }
}

/// Failed commit of a CFS task reservation.
///
/// The error retains the complete unpublished reservation, so the caller may
/// retry it with the owning scheduler or cancel it without losing task
/// ownership.
pub struct CfsReservationCommitError<T> {
    kind: SchedulerError,
    reservation: CfsTaskReservation<T>,
}

impl<T> CfsReservationCommitError<T> {
    /// Returns the precise scheduler failure.
    pub const fn kind(&self) -> SchedulerError {
        self.kind
    }

    /// Returns the still-live unpublished reservation.
    pub const fn reservation(&self) -> &CfsTaskReservation<T> {
        &self.reservation
    }

    /// Recovers the still-live unpublished reservation.
    pub fn into_reservation(self) -> CfsTaskReservation<T> {
        self.reservation
    }
}

impl<T> fmt::Debug for CfsReservationCommitError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CfsReservationCommitError")
            .field("kind", &self.kind)
            .field("reservation", &self.reservation)
            .finish()
    }
}

impl<T> CFScheduler<T> {
    /// Creates a new empty [`CFScheduler`].
    pub fn new() -> Self {
        Self {
            ready_queue: RBTree::new(ReadyTaskAdapter::new()),
            min_vruntime: None,
            id: UNOWNED,
            fair_sequence: 0,
            rt_front_seq: 0,
            rt_back_seq: 0,
        }
    }

    /// get the name of scheduler
    pub fn scheduler_name() -> &'static str {
        "Completely Fair"
    }

    fn queue_floor(&self) -> isize {
        self.min_vruntime.unwrap_or(0)
    }

    fn ensure_id(&mut self) -> Result<usize, SchedulerError> {
        if self.id == UNOWNED {
            self.id = allocate_scheduler_id()?;
        }
        Ok(self.id)
    }

    fn next_fair_sequence(&mut self) -> Result<isize, SchedulerError> {
        let sequence = self.fair_sequence;
        self.fair_sequence = self
            .fair_sequence
            .checked_add(1)
            .ok_or(SchedulerError::SequenceExhausted)?;
        Ok(sequence)
    }

    fn min_ready_vruntime(&self) -> Option<isize> {
        self.ready_queue
            .lower_bound(Bound::Included(&(1, isize::MIN, isize::MIN)))
            .get()
            .filter(|task| !task.ready_is_rt())
            .map(|task| task.ready_order.load(Ordering::Acquire))
    }

    fn refresh_min_vruntime(&mut self, current_vruntime: Option<isize>) {
        let candidate = match (current_vruntime, self.min_ready_vruntime()) {
            (Some(current), Some(ready)) => Some(current.min(ready)),
            (Some(current), None) => Some(current),
            (None, Some(ready)) => Some(ready),
            (None, None) => None,
        };

        self.min_vruntime = match (self.min_vruntime, candidate) {
            (_, None) => None,
            (Some(old), Some(new)) => Some(old.max(new)),
            (None, Some(new)) => Some(new),
        };
    }

    fn claim_enqueue_task(
        &mut self,
        task: Arc<CFSTask<T>>,
    ) -> Result<CfsEnqueueClaim<T>, SchedulerError> {
        let scheduler_id = self.ensure_id()?;
        task.claim(scheduler_id)?;
        Ok(CfsEnqueueClaim {
            task: Some(task),
            scheduler_id,
        })
    }

    fn wakeup_floor(&self, params: CfsTaskParams) -> isize {
        let floor = self.queue_floor();
        match params.class {
            // Keep freshly woken fair tasks slightly behind the current floor
            // so a wakeup burst does not immediately displace the task that is
            // doing the wakeup-side follow-up work.
            CfsTaskClass::Normal => floor.saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS),
            CfsTaskClass::Batch => floor.saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS + 1),
            CfsTaskClass::Idle => floor.saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS + 2),
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo => floor,
        }
    }

    fn next_rt_seq(&mut self, front: bool) -> Result<isize, SchedulerError> {
        if front {
            self.rt_front_seq = self
                .rt_front_seq
                .checked_sub(1)
                .ok_or(SchedulerError::SequenceExhausted)?;
            Ok(self.rt_front_seq)
        } else {
            let seq = self.rt_back_seq;
            self.rt_back_seq = self
                .rt_back_seq
                .checked_add(1)
                .ok_or(SchedulerError::SequenceExhausted)?;
            Ok(seq)
        }
    }

    fn enqueue_with_action(
        &mut self,
        task: Arc<CFSTask<T>>,
        action: CfsEnqueueAction,
    ) -> Result<(), SchedulerError> {
        let claim = self.claim_enqueue_task(task)?;
        let params = claim.task().sched_params();

        if is_realtime(params) {
            let (front, reset_rr) = match params.class {
                CfsTaskClass::Fifo => (
                    matches!(action, CfsEnqueueAction::Requeue { preempt: true }),
                    false,
                ),
                CfsTaskClass::RoundRobin => match action {
                    CfsEnqueueAction::Requeue { preempt: true }
                        if claim.task().rr_time_slice() > 0 =>
                    {
                        (true, false)
                    }
                    CfsEnqueueAction::New
                    | CfsEnqueueAction::Wakeup
                    | CfsEnqueueAction::ExplicitYield
                    | CfsEnqueueAction::Requeue { .. } => (false, true),
                    // Preserve a positive remaining RR budget across a CPU
                    // transfer. An already exhausted budget starts a fresh
                    // quantum after being placed at the destination tail.
                    CfsEnqueueAction::Migrate if claim.task().rr_time_slice() > 0 => (false, false),
                    CfsEnqueueAction::Migrate => (false, true),
                },
                CfsTaskClass::Normal | CfsTaskClass::Batch | CfsTaskClass::Idle => {
                    unreachable!("realtime snapshot has a fair class")
                }
            };
            let sequence = self.next_rt_seq(front)?;
            claim.task().clear_migration_vruntime_offset();
            if reset_rr {
                claim.task().reset_rr_time_slice();
            }
            claim
                .task()
                .stage_ready_key(0, rt_priority_key(params.rt_priority), sequence);
            self.ready_queue.insert(claim.publish());
            return Ok(());
        }

        let sequence = self.next_fair_sequence()?;
        let vruntime = match action {
            CfsEnqueueAction::New => {
                if claim.task().seeded_vruntime.load(Ordering::Acquire) {
                    claim
                        .task()
                        .get_vruntime_with(params)
                        .max(self.queue_floor())
                } else {
                    self.queue_floor()
                }
            }
            CfsEnqueueAction::Wakeup => claim
                .task()
                .get_vruntime_with(params)
                .max(self.wakeup_floor(params)),
            CfsEnqueueAction::ExplicitYield => {
                let floor = self
                    .min_ready_vruntime()
                    .unwrap_or_else(|| self.queue_floor())
                    .saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS);
                claim.task().get_vruntime_with(params).max(floor)
            }
            CfsEnqueueAction::Requeue { .. } => claim.task().get_vruntime_with(params),
            CfsEnqueueAction::Migrate => claim.task().take_migration_vruntime(self.queue_floor()),
        };
        if matches!(action, CfsEnqueueAction::New) {
            claim.task().seeded_vruntime.store(false, Ordering::Release);
        }
        if !matches!(action, CfsEnqueueAction::Migrate) {
            claim.task().clear_migration_vruntime_offset();
        }
        claim.task().rebase_vruntime(vruntime);
        claim.task().stage_ready_key(1, vruntime, sequence);
        self.ready_queue.insert(claim.publish());
        self.refresh_min_vruntime(None);
        Ok(())
    }

    /// Reserves publication of one brand-new task without making it runnable.
    ///
    /// All fallible scheduler identity, ownership, and ordering work completes
    /// here. Configuration is excluded until the returned token is committed
    /// or dropped. Sequence numbers may contain cancellation gaps, but they are
    /// never wrapped, rebased, or reused; exhaustion is reported explicitly.
    pub fn reserve_new_task(
        &mut self,
        task: &Arc<CFSTask<T>>,
    ) -> Result<CfsTaskReservation<T>, SchedulerError> {
        let scheduler_id = self.ensure_id()?;
        task.queue_owner
            .compare_exchange(UNOWNED, CONFIGURING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|owner| {
                if owner == CONFIGURING {
                    SchedulerError::TaskBusy
                } else if owner == scheduler_id {
                    SchedulerError::AlreadyQueued
                } else {
                    SchedulerError::ForeignQueue
                }
            })?;

        let params = task.sched_params();
        let sequence = if is_realtime(params) {
            self.next_rt_seq(false)
        } else {
            self.next_fair_sequence()
        };
        let sequence = match sequence {
            Ok(sequence) => sequence,
            Err(error) => {
                let _ = task.transfer_owner(CONFIGURING, UNOWNED);
                return Err(error);
            }
        };

        Ok(CfsTaskReservation {
            task: Some(Arc::clone(task)),
            scheduler_id,
            sequence,
            params,
        })
    }

    /// Commits a reservation created by this exact scheduler instance.
    ///
    /// A correct-scheduler commit only writes pre-reserved task metadata and
    /// links an intrusive node; it cannot allocate or exhaust ordering space.
    /// Passing a token from another scheduler, or detecting inconsistent
    /// private ownership, returns a typed owner-retaining error.
    pub fn commit_reserved_task(
        &mut self,
        mut reservation: CfsTaskReservation<T>,
    ) -> Result<Arc<CFSTask<T>>, CfsReservationCommitError<T>> {
        if reservation.scheduler_id != self.id {
            return Err(CfsReservationCommitError {
                kind: SchedulerError::ForeignQueue,
                reservation,
            });
        }
        let task = Arc::clone(reservation.task());
        if task.owner() != CONFIGURING || task.sched_params() != reservation.params {
            return Err(CfsReservationCommitError {
                kind: SchedulerError::InconsistentState,
                reservation,
            });
        }
        if task.transfer_owner(CONFIGURING, self.id).is_err() {
            return Err(CfsReservationCommitError {
                kind: SchedulerError::InconsistentState,
                reservation,
            });
        }

        let params = reservation.params;
        if is_realtime(params) {
            if matches!(params.class, CfsTaskClass::RoundRobin) {
                task.reset_rr_time_slice();
            }
            task.stage_ready_key(0, rt_priority_key(params.rt_priority), reservation.sequence);
        } else {
            let vruntime = if task.seeded_vruntime.load(Ordering::Acquire) {
                task.get_vruntime_with(params).max(self.queue_floor())
            } else {
                self.queue_floor()
            };
            task.seeded_vruntime.store(false, Ordering::Release);
            task.rebase_vruntime(vruntime);
            task.stage_ready_key(1, vruntime, reservation.sequence);
        }

        let task = reservation
            .task
            .take()
            .expect("live CFS reservation always owns its task");
        self.ready_queue.insert(Arc::clone(&task));
        if !is_realtime(params) {
            self.refresh_min_vruntime(None);
        }
        Ok(task)
    }

    fn reinsert_reconfigured(
        &mut self,
        task: Arc<CFSTask<T>>,
        params: CfsTaskParams,
    ) -> Result<(), SchedulerError> {
        let scheduler_id = self.ensure_id()?;
        let sequence = if is_realtime(params) {
            self.next_rt_seq(false)?
        } else {
            self.next_fair_sequence()?
        };
        if is_realtime(params) {
            task.stage_ready_key(0, rt_priority_key(params.rt_priority), sequence);
        } else {
            task.stage_ready_key(1, task.get_vruntime_with(params), sequence);
        }
        task.transfer_owner(CONFIGURING, scheduler_id)?;
        self.ready_queue.insert(task);
        if !is_realtime(params) {
            self.refresh_min_vruntime(None);
        }
        Ok(())
    }

    fn restore_reconfigured(
        &mut self,
        task: Arc<CFSTask<T>>,
        snapshot: CfsTaskStateSnapshot,
        min_vruntime: Option<isize>,
    ) {
        task.restore_reconfiguration_state(snapshot);
        task.transfer_owner(CONFIGURING, self.id)
            .expect("private CFS reconfiguration ownership changed before rollback");
        self.ready_queue.insert(task);
        self.min_vruntime = min_vruntime;
    }

    fn has_ready_rt_with_higher_priority(&self, current_priority: u8) -> bool {
        self.ready_queue.front().get().is_some_and(|task| {
            task.ready_is_rt()
                && task.ready_order.load(Ordering::Acquire) < rt_priority_key(current_priority)
        })
    }

    fn has_ready_rt_with_same_priority(&self, current_priority: u8) -> bool {
        let key = rt_priority_key(current_priority);
        self.ready_queue
            .lower_bound(Bound::Included(&(0, key, isize::MIN)))
            .get()
            .is_some_and(|task| {
                task.ready_is_rt() && task.ready_order.load(Ordering::Acquire) == key
            })
    }

    /// Updates runtime scheduling parameters for an unqueued task or for a task
    /// currently owned by this scheduler.
    ///
    /// A ready task is removed, reconfigured, and reinserted under this
    /// scheduler's exclusive borrow. A task owned by another scheduler is
    /// rejected without mutation.
    pub fn set_task_params(
        &mut self,
        task: &Arc<CFSTask<T>>,
        params: CfsTaskParams,
    ) -> Result<(), SchedulerError> {
        let params = params
            .validated()
            .ok_or(SchedulerError::InvalidParameters)?;
        match task.owner() {
            UNOWNED => task.configure(params),
            CONFIGURING => Err(SchedulerError::TaskBusy),
            owner if owner != self.id || self.id == UNOWNED => Err(SchedulerError::ForeignQueue),
            _ => {
                let previous = task.snapshot_reconfiguration_state();
                let previous_min_vruntime = self.min_vruntime;
                let queued = self
                    .remove_owned_task(task, CONFIGURING, false)?
                    .ok_or(SchedulerError::InconsistentState)?;
                queued.apply_validated(params);
                match self.reinsert_reconfigured(queued.clone(), params) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        self.restore_reconfigured(queued, previous, previous_min_vruntime);
                        Err(error)
                    }
                }
            }
        }
    }

    fn remove_owned_task(
        &mut self,
        task: &Arc<CFSTask<T>>,
        next_owner: usize,
        preserve_migration_vruntime_offset: bool,
    ) -> Result<Option<Arc<CFSTask<T>>>, SchedulerError> {
        match task.owner() {
            UNOWNED => return Ok(None),
            CONFIGURING => return Err(SchedulerError::TaskBusy),
            owner if owner != self.id || self.id == UNOWNED => {
                return Err(SchedulerError::ForeignQueue);
            }
            _ => {}
        }
        let key = task.ready_key();
        let migration_floor = preserve_migration_vruntime_offset.then(|| self.queue_floor());
        let mut cursor = self.ready_queue.lower_bound_mut(Bound::Included(&key));
        loop {
            let Some(found) = cursor.get() else {
                return Err(SchedulerError::InconsistentState);
            };
            if found.ready_key() != key {
                return Err(SchedulerError::InconsistentState);
            }
            if core::ptr::eq(found, Arc::as_ptr(task)) {
                break;
            }
            cursor.move_next();
        }
        if let Some(source_floor) = migration_floor {
            task.stage_migration_vruntime_offset(source_floor);
        }
        let removed = match cursor.remove() {
            Some(removed) => removed,
            None => {
                if preserve_migration_vruntime_offset {
                    task.clear_migration_vruntime_offset();
                }
                return Err(SchedulerError::InconsistentState);
            }
        };
        removed.transfer_owner(self.id, next_owner)?;
        if key.0 != 0 {
            self.refresh_min_vruntime(None);
        }
        Ok(Some(removed))
    }
}

impl<T> BaseScheduler for CFScheduler<T> {
    type SchedItem = Arc<CFSTask<T>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) -> Result<(), SchedulerError> {
        self.enqueue_with_action(task, CfsEnqueueAction::New)
    }

    fn remove_task(
        &mut self,
        task: &Self::SchedItem,
    ) -> Result<Option<Self::SchedItem>, SchedulerError> {
        self.remove_owned_task(task, UNOWNED, false)
    }

    fn remove_task_for_migration(
        &mut self,
        task: &Self::SchedItem,
    ) -> Result<Option<Self::SchedItem>, SchedulerError> {
        self.remove_owned_task(task, UNOWNED, true)
    }

    fn deactivate_task(&mut self, task: &Self::SchedItem, reason: DeactivateReason) {
        match reason {
            DeactivateReason::Migrate => task.stage_migration_vruntime_offset(self.queue_floor()),
            DeactivateReason::Sleep | DeactivateReason::Exit => {
                task.clear_migration_vruntime_offset()
            }
        }
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        let next = self.ready_queue.front_mut().remove();
        if let Some(task) = &next {
            task.transfer_owner(self.id, UNOWNED)
                .expect("CFS queue owner invariant violated");
        }
        match next.as_ref() {
            Some(task) if !task.ready_is_rt() => {
                self.refresh_min_vruntime(Some(task.ready_order.load(Ordering::Acquire)));
            }
            Some(_) => {}
            None => self.refresh_min_vruntime(None),
        }
        next
    }

    fn put_prev_task(
        &mut self,
        prev: Self::SchedItem,
        preempt: bool,
    ) -> Result<(), SchedulerError> {
        self.enqueue_with_action(prev, CfsEnqueueAction::Requeue { preempt })
    }

    fn enqueue_task(
        &mut self,
        task: Self::SchedItem,
        reason: EnqueueReason,
    ) -> Result<(), SchedulerError> {
        let action = match reason {
            EnqueueReason::New => CfsEnqueueAction::New,
            EnqueueReason::Wakeup => CfsEnqueueAction::Wakeup,
            EnqueueReason::Yield => CfsEnqueueAction::ExplicitYield,
            EnqueueReason::Preempt => CfsEnqueueAction::Requeue { preempt: true },
            EnqueueReason::Migrate => CfsEnqueueAction::Migrate,
        };
        self.enqueue_with_action(task, action)
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        let current_params = current.sched_params();
        if matches!(
            current_params.class,
            CfsTaskClass::RoundRobin | CfsTaskClass::Fifo
        ) {
            let current_priority = current_params.rt_priority;
            if self.has_ready_rt_with_higher_priority(current_priority) {
                return true;
            }

            return match current_params.class {
                CfsTaskClass::Fifo => false,
                CfsTaskClass::RoundRobin => {
                    let old_slice = current.task_tick_rr();
                    if old_slice <= 1 {
                        if self.has_ready_rt_with_same_priority(current_priority) {
                            return true;
                        }
                        current.reset_rr_time_slice();
                    }
                    false
                }
                CfsTaskClass::Normal | CfsTaskClass::Batch | CfsTaskClass::Idle => false,
            };
        }

        if self
            .ready_queue
            .front()
            .get()
            .is_some_and(CFSTask::ready_is_rt)
        {
            return true;
        }

        current.task_tick();
        let current_vruntime = current.get_vruntime_with(current_params);
        self.refresh_min_vruntime(Some(current_vruntime));

        match self.min_ready_vruntime() {
            // Keep the current fair task running for a small vruntime window
            // after a wakeup burst so it can complete short follow-up work
            // instead of being displaced immediately by freshly woken peers.
            Some(ready_min) => {
                current_vruntime > ready_min.saturating_add(FAIR_PREEMPT_GRANULARITY_TICKS)
            }
            None => false,
        }
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> Result<(), SchedulerError> {
        if !(-20..=19).contains(&prio) {
            return Err(SchedulerError::InvalidParameters);
        }
        match task.owner() {
            UNOWNED => task.configure_nice(prio as i8),
            CONFIGURING => Err(SchedulerError::TaskBusy),
            owner if owner != self.id || self.id == UNOWNED => Err(SchedulerError::ForeignQueue),
            _ => {
                let current = task.sched_params();
                if is_realtime(current) {
                    return Err(SchedulerError::IncompatibleClass);
                }
                self.set_task_params(
                    task,
                    CfsTaskParams {
                        class: current.class,
                        nice: prio as i8,
                        rt_priority: 0,
                    },
                )
            }
        }
    }
}

impl<T> Default for CFScheduler<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for CFScheduler<T> {
    fn drop(&mut self) {
        while let Some(task) = self.ready_queue.front_mut().remove() {
            task.transfer_owner(self.id, UNOWNED)
                .expect("CFS queue owner invariant violated during scheduler drop");
        }
    }
}

#[cfg(test)]
mod sequence_tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    fn rr_params(priority: u8) -> CfsTaskParams {
        CfsTaskParams {
            class: CfsTaskClass::RoundRobin,
            nice: 0,
            rt_priority: priority,
        }
    }

    #[test]
    fn fair_sequence_exhaustion_is_explicit_and_side_effect_free() {
        let mut scheduler = CFScheduler::new();
        let first = Arc::new(CFSTask::new(1));
        let second = Arc::new(CFSTask::new(2));
        scheduler.add_task(first.clone()).unwrap();
        scheduler.fair_sequence = isize::MAX;
        let before = second.snapshot_reconfiguration_state();

        assert_eq!(
            scheduler.add_task(second.clone()),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(second.snapshot_reconfiguration_state(), before);
        assert_eq!(second.owner(), UNOWNED);

        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &first));
        assert!(scheduler.pick_next_task().is_none());
    }

    #[test]
    fn realtime_back_sequence_exhaustion_is_explicit_and_side_effect_free() {
        let mut scheduler = CFScheduler::new();
        let first = Arc::new(CFSTask::new(1));
        let second = Arc::new(CFSTask::new(2));
        for task in [&first, &second] {
            task.configure(rr_params(10)).unwrap();
        }
        scheduler.add_task(first.clone()).unwrap();
        scheduler.rt_back_seq = isize::MAX;
        let before = second.snapshot_reconfiguration_state();

        assert_eq!(
            scheduler.add_task(second.clone()),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(second.snapshot_reconfiguration_state(), before);
        assert_eq!(second.owner(), UNOWNED);

        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &first));
        assert!(scheduler.pick_next_task().is_none());
    }

    #[test]
    fn realtime_front_sequence_exhaustion_preserves_running_task_state() {
        let mut scheduler = CFScheduler::new();
        let first = Arc::new(CFSTask::new(1));
        let second = Arc::new(CFSTask::new(2));
        for task in [&first, &second] {
            task.configure(rr_params(10)).unwrap();
            scheduler.add_task(task.clone()).unwrap();
        }
        let running = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&running, &first));
        scheduler.rt_front_seq = isize::MIN;
        let before = running.snapshot_reconfiguration_state();

        assert_eq!(
            scheduler.put_prev_task(running.clone(), true),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(running.snapshot_reconfiguration_state(), before);
        assert_eq!(running.owner(), UNOWNED);
        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &second));
    }

    #[test]
    fn enqueue_claim_excludes_configuration_before_snapshot_and_mutation() {
        let mut scheduler = CFScheduler::new();
        let task = Arc::new(CFSTask::new(()));
        let before = task.snapshot_reconfiguration_state();
        let claim = scheduler.claim_enqueue_task(task.clone()).unwrap();

        assert_eq!(
            task.configure(rr_params(42)),
            Err(SchedulerError::AlreadyQueued)
        );
        assert_eq!(task.snapshot_reconfiguration_state(), before);
        drop(claim);

        task.configure(rr_params(42)).unwrap();
    }

    #[test]
    fn nice_update_claim_excludes_a_competing_class_change() {
        let mut scheduler = CFScheduler::new();
        let task = Arc::new(CFSTask::new(()));
        let claim = task.claim_configuration().unwrap();
        let before = task.sched_params();

        assert_eq!(
            scheduler.set_priority(&task, 5),
            Err(SchedulerError::TaskBusy)
        );
        assert_eq!(task.sched_params(), before);
        drop(claim);

        scheduler.set_priority(&task, 5).unwrap();
        assert_eq!(task.sched_params().nice, 5);
    }

    #[test]
    fn rejected_foreign_enqueue_preserves_rr_slice_and_ready_key() {
        let mut owner = CFScheduler::new();
        let mut foreign = CFScheduler::new();
        let task = Arc::new(CFSTask::new(1));
        let peer = Arc::new(CFSTask::new(2));
        for task in [&task, &peer] {
            task.configure(rr_params(42)).unwrap();
            owner.add_task(task.clone()).unwrap();
        }

        let running = owner.pick_next_task().unwrap();
        for _ in 0..RR_TIMESLICE_TICKS - 1 {
            assert!(!owner.task_tick(&running));
        }
        owner.put_prev_task(running, true).unwrap();
        let before = task.snapshot_reconfiguration_state();
        assert_eq!(before.rr_time_slice, 1);

        assert_eq!(
            foreign.add_task(task.clone()),
            Err(SchedulerError::ForeignQueue)
        );
        assert_eq!(task.snapshot_reconfiguration_state(), before);

        let running = owner.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&running, &task));
        assert!(owner.task_tick(&running));
    }

    #[test]
    fn failed_ready_reconfiguration_restores_exact_state_and_order() {
        let mut scheduler = CFScheduler::new();
        let task = Arc::new(CFSTask::new(1));
        let peer = Arc::new(CFSTask::new(2));
        for task in [&task, &peer] {
            task.configure(rr_params(42)).unwrap();
            scheduler.add_task(task.clone()).unwrap();
        }

        let running = scheduler.pick_next_task().unwrap();
        for _ in 0..RR_TIMESLICE_TICKS - 1 {
            assert!(!scheduler.task_tick(&running));
        }
        scheduler.put_prev_task(running, true).unwrap();
        let before = task.snapshot_reconfiguration_state();
        let before_min_vruntime = scheduler.min_vruntime;
        scheduler.rt_back_seq = isize::MAX;

        assert_eq!(
            scheduler.set_task_params(&task, rr_params(43)),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(task.snapshot_reconfiguration_state(), before);
        assert_eq!(scheduler.min_vruntime, before_min_vruntime);
        assert_eq!(task.owner(), scheduler.id);

        let running = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&running, &task));
        assert!(scheduler.task_tick(&running));
    }

    #[test]
    fn concurrent_parameter_snapshots_are_never_torn() {
        const FAIR: CfsTaskParams = CfsTaskParams {
            class: CfsTaskClass::Normal,
            nice: -20,
            rt_priority: 0,
        };
        const REALTIME: CfsTaskParams = CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: u8::MAX,
        };

        let task = Arc::new(CFSTask::new(()));
        let done = Arc::new(AtomicBool::new(false));
        let writer_task = task.clone();
        let writer_done = done.clone();
        let writer = thread::spawn(move || {
            for iteration in 0..100_000 {
                writer_task
                    .configure(if iteration % 2 == 0 { FAIR } else { REALTIME })
                    .unwrap();
            }
            writer_done.store(true, Ordering::Release);
        });

        while !done.load(Ordering::Acquire) {
            let observed = task.sched_params();
            assert!(
                observed == CfsTaskParams::default() || observed == FAIR || observed == REALTIME
            );
        }
        writer.join().unwrap();
    }

    #[test]
    fn cancelled_new_task_reservation_never_publishes_or_retains_ownership() {
        let mut scheduler = CFScheduler::new();
        let task = Arc::new(CFSTask::new(()));

        let reservation = scheduler.reserve_new_task(&task).unwrap();
        assert_eq!(scheduler.fair_sequence, 1);
        assert!(scheduler.pick_next_task().is_none());
        assert_eq!(
            task.configure(CfsTaskParams::default()),
            Err(SchedulerError::TaskBusy)
        );
        let returned = reservation.cancel();
        assert!(Arc::ptr_eq(&returned, &task));

        task.configure(CfsTaskParams::default()).unwrap();
        scheduler.add_task(task.clone()).unwrap();
        assert_eq!(task.ready_key().2, 1, "cancelled sequence is never reused");
        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &task));
    }

    #[test]
    fn committed_new_task_reservation_publishes_the_exact_owner() {
        let mut scheduler = CFScheduler::new();
        let task = Arc::new(CFSTask::new(()));
        let reservation = scheduler.reserve_new_task(&task).unwrap();

        let published = scheduler.commit_reserved_task(reservation).unwrap();
        assert!(Arc::ptr_eq(&published, &task));
        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &task));
    }

    #[test]
    fn wrong_scheduler_commit_returns_the_live_reservation() {
        let mut owner = CFScheduler::new();
        let mut foreign = CFScheduler::new();
        let task = Arc::new(CFSTask::new(()));
        let reservation = owner.reserve_new_task(&task).unwrap();

        let error = match foreign.commit_reserved_task(reservation) {
            Ok(_) => panic!("foreign scheduler committed a private reservation"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), SchedulerError::ForeignQueue);
        assert!(Arc::ptr_eq(error.reservation().task(), &task));
        assert_eq!(
            task.configure(CfsTaskParams::default()),
            Err(SchedulerError::TaskBusy)
        );

        let published = owner
            .commit_reserved_task(error.into_reservation())
            .unwrap();
        assert!(Arc::ptr_eq(&published, &task));
    }

    #[test]
    fn fair_reservation_remains_committable_after_sequence_exhaustion() {
        let mut scheduler = CFScheduler::new();
        let first = Arc::new(CFSTask::new(1));
        let reserved = Arc::new(CFSTask::new(2));
        let rejected = Arc::new(CFSTask::new(3));
        scheduler.add_task(first.clone()).unwrap();
        let reservation = scheduler.reserve_new_task(&reserved).unwrap();
        scheduler.fair_sequence = isize::MAX;
        let rejected_before = rejected.snapshot_reconfiguration_state();

        assert_eq!(
            scheduler.add_task(rejected.clone()),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(rejected.snapshot_reconfiguration_state(), rejected_before);
        let published = scheduler.commit_reserved_task(reservation).unwrap();
        assert!(Arc::ptr_eq(&published, &reserved));

        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &first));
        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &reserved));
    }

    #[test]
    fn realtime_reservation_remains_committable_after_sequence_exhaustion() {
        let mut scheduler = CFScheduler::new();
        let first = Arc::new(CFSTask::new(1));
        let reserved = Arc::new(CFSTask::new(2));
        let rejected = Arc::new(CFSTask::new(3));
        for task in [&first, &reserved, &rejected] {
            task.configure(rr_params(42)).unwrap();
        }
        scheduler.add_task(first.clone()).unwrap();
        let reservation = scheduler.reserve_new_task(&reserved).unwrap();
        scheduler.rt_back_seq = isize::MAX;
        let rejected_before = rejected.snapshot_reconfiguration_state();

        assert_eq!(
            scheduler.add_task(rejected.clone()),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(rejected.snapshot_reconfiguration_state(), rejected_before);
        let published = scheduler.commit_reserved_task(reservation).unwrap();
        assert!(Arc::ptr_eq(&published, &reserved));

        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &first));
        assert!(Arc::ptr_eq(&scheduler.pick_next_task().unwrap(), &reserved));
    }

    #[test]
    fn ready_migration_preserves_vruntime_offset_at_destination_floor() {
        let mut source = CFScheduler::new();
        let migrating = Arc::new(CFSTask::new("migrating"));
        let source_peer = Arc::new(CFSTask::new("source-peer"));

        source.add_task(migrating.clone()).unwrap();
        let running = source.pick_next_task().unwrap();
        for _ in 0..10 {
            assert!(!source.task_tick(&running));
        }
        source.put_prev_task(running, false).unwrap();
        source.add_task(source_peer).unwrap();

        let running = source.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&running, &migrating));
        for _ in 0..5 {
            let _ = source.task_tick(&running);
        }
        source.put_prev_task(running, false).unwrap();
        assert_eq!(source.queue_floor(), 10);

        let migrated = source
            .remove_task_for_migration(&migrating)
            .unwrap()
            .unwrap();
        assert!(migrating
            .migration_vruntime_offset_valid
            .load(Ordering::Acquire));

        let mut destination = CFScheduler::new();
        let destination_peer = Arc::new(CFSTask::new("destination-peer"));
        destination.add_task(destination_peer.clone()).unwrap();
        let running = destination.pick_next_task().unwrap();
        for _ in 0..100 {
            assert!(!destination.task_tick(&running));
        }
        destination.put_prev_task(running, false).unwrap();
        assert_eq!(destination.queue_floor(), 100);

        destination
            .enqueue_task(migrated, EnqueueReason::Migrate)
            .unwrap();
        assert_eq!(migrating.ready_key().1, 105);
        assert!(!migrating
            .migration_vruntime_offset_valid
            .load(Ordering::Acquire));
        assert!(Arc::ptr_eq(
            &destination.pick_next_task().unwrap(),
            &destination_peer
        ));
        assert!(Arc::ptr_eq(
            &destination.pick_next_task().unwrap(),
            &migrating
        ));
    }

    #[test]
    fn failed_migration_enqueue_retains_rebase_state_for_retry() {
        let mut source = CFScheduler::new();
        let migrating = Arc::new(CFSTask::new(()));
        source.add_task(migrating.clone()).unwrap();
        let migrated = source
            .remove_task_for_migration(&migrating)
            .unwrap()
            .unwrap();

        let mut exhausted = CFScheduler::new();
        exhausted.fair_sequence = isize::MAX;
        assert_eq!(
            exhausted.enqueue_task(migrated.clone(), EnqueueReason::Migrate),
            Err(SchedulerError::SequenceExhausted)
        );
        assert!(migrating
            .migration_vruntime_offset_valid
            .load(Ordering::Acquire));

        let mut retry = CFScheduler::new();
        retry
            .enqueue_task(migrated, EnqueueReason::Migrate)
            .unwrap();
        assert!(!migrating
            .migration_vruntime_offset_valid
            .load(Ordering::Acquire));
    }

    #[test]
    fn rr_migration_retry_preserves_positive_remaining_slice() {
        let mut source = CFScheduler::new();
        let migrating = Arc::new(CFSTask::new("migrating"));
        let peer = Arc::new(CFSTask::new("peer"));
        for task in [&migrating, &peer] {
            task.configure(rr_params(42)).unwrap();
            source.add_task(task.clone()).unwrap();
        }

        let running = source.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&running, &migrating));
        for _ in 0..RR_TIMESLICE_TICKS - 2 {
            assert!(!source.task_tick(&running));
        }
        source.put_prev_task(running, true).unwrap();
        assert_eq!(migrating.rr_time_slice(), 2);

        let migrated = source
            .remove_task_for_migration(&migrating)
            .unwrap()
            .unwrap();
        assert_eq!(migrating.owner(), UNOWNED);
        assert_eq!(migrating.rr_time_slice(), 2);

        let mut exhausted = CFScheduler::new();
        exhausted.rt_back_seq = isize::MAX;
        let before = migrating.snapshot_reconfiguration_state();
        assert_eq!(
            exhausted.enqueue_task(migrated.clone(), EnqueueReason::Migrate),
            Err(SchedulerError::SequenceExhausted)
        );
        assert_eq!(migrating.snapshot_reconfiguration_state(), before);
        assert_eq!(migrating.owner(), UNOWNED);

        source
            .enqueue_task(migrated, EnqueueReason::Migrate)
            .unwrap();
        assert_eq!(migrating.rr_time_slice(), 2);
        assert_eq!(migrating.owner(), source.id);
    }

    #[test]
    fn rr_migration_resets_an_exhausted_slice_at_destination_tail() {
        let mut source = CFScheduler::new();
        let migrating = Arc::new(CFSTask::new(()));
        migrating.configure(rr_params(42)).unwrap();
        source.add_task(migrating.clone()).unwrap();
        let migrated = source
            .remove_task_for_migration(&migrating)
            .unwrap()
            .unwrap();
        migrating.rr_time_slice.store(0, Ordering::Release);

        let mut destination = CFScheduler::new();
        destination
            .enqueue_task(migrated, EnqueueReason::Migrate)
            .unwrap();

        assert_eq!(migrating.rr_time_slice(), RR_TIMESLICE_TICKS as isize);
        assert_eq!(migrating.owner(), destination.id);
    }

    #[test]
    fn running_migration_and_sleep_have_distinct_lifecycle_state() {
        let mut source = CFScheduler::new();
        let task = Arc::new(CFSTask::new(()));
        source.add_task(task.clone()).unwrap();
        let running = source.pick_next_task().unwrap();
        for _ in 0..7 {
            assert!(!source.task_tick(&running));
        }

        source.deactivate_task(&running, DeactivateReason::Migrate);
        assert!(task.migration_vruntime_offset_valid.load(Ordering::Acquire));
        source.deactivate_task(&running, DeactivateReason::Sleep);
        assert!(!task.migration_vruntime_offset_valid.load(Ordering::Acquire));
    }
}
