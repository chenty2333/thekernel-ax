//! Small, bounded RCU for immutable read-mostly kernel state.
//!
//! The read side is deliberately narrower than a general purpose reclamation
//! library. A reader pins the current CPU with preemption disabled, records an
//! even epoch, and validates that epoch again before it may turn an atomic
//! `Arc` pointer into a reader-owned `Arc`; local IRQ masking is limited to
//! nested active-state transitions. Publishers
//! reserve a retire slot before making a pointer visible.  Retired ownership
//! is queued in a fixed FIFO and is only dropped by an explicit task-context
//! drain.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::sync::Arc;
use core::{
    marker::PhantomData,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
};

use kspin::SpinNoIrq;

/// Architecture/runtime boundary required by the generic epoch algorithm.
///
/// Implementors must make `pin_current_cpu` a preemption-stable pin, make
/// `with_local_irqs_disabled` nest with the platform's IRQ state, and report
/// the CPU selected by the pin. The trait is unsafe because violating those
/// guarantees can make an active reader appear to be on a different CPU and
/// permit premature reclamation.
///
/// # Safety
///
/// Implementors must keep the returned pin guard alive on one CPU for the
/// entire read-side critical section, report that same CPU from
/// `current_cpu`, and preserve the caller's interrupt state while nesting
/// `with_local_irqs_disabled`. Breaking any of these guarantees can let the
/// domain reclaim an object that is still reachable by a reader.
pub unsafe trait EpochPlatform {
    /// Guard held for the complete read-side critical section. This guard
    /// disables preemption but does not mask local IRQs for its lifetime.
    type PinGuard;

    /// Pins the current task to its CPU without disabling local IRQs.
    fn pin_current_cpu() -> Self::PinGuard;

    /// Returns the CPU selected by the current pin.
    fn current_cpu() -> usize;

    /// Runs a short active-reader state transition with nested local IRQ
    /// masking. Implementations must restore the previous IRQ state.
    fn with_local_irqs_disabled<R>(operation: impl FnOnce() -> R) -> R;

    /// Notifies the task-context reclaimer after an outermost reader becomes
    /// quiescent. The callback must be allocation-free and may run from IRQ
    /// context; it is the wake edge for a previously grace-blocked FIFO.
    fn reader_quiescent();

    /// Returns true only in a context permitted to run object destructors.
    fn in_task_context() -> bool;

    /// Returns true only when the current task may wait for a grace period.
    /// This must exclude IRQ/exception context and any caller-owned
    /// non-preemptible section which could prevent a local reader from
    /// quiescing.
    fn in_preemptible_task_context() -> bool;
}

/// Errors returned by bounded RCU operations and CPU lifecycle transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RcuError {
    /// The fixed retire FIFO has no slot available for another publication.
    RetireCapacity,
    /// A reader attempted to run on a CPU that is not currently registered.
    UnregisteredCpu,
    /// Reclamation was requested from IRQ/exception context.
    NotTaskContext,
    /// A CPU registration was requested for an already online CPU.
    CpuAlreadyRegistered,
    /// An online CPU could not be taken offline because a reader is active.
    CpuBusy,
    /// An unregister was requested for an offline CPU.
    CpuNotRegistered,
    /// Reader nesting reached the fixed representable depth.
    ReaderNestingOverflow,
    /// The monotonic epoch domain cannot represent another transition.
    EpochExhausted,
    /// A required load found an empty slot.
    EmptySlot,
}

/// Failure before a slot swap. The returned replacement remains caller-owned
/// and the retire reservation is released.
#[derive(Debug)]
pub enum PublishError<T> {
    /// Another writer installed a different pointer first.
    Stale(Arc<T>),
    /// The monotonically increasing epoch domain is exhausted.
    EpochExhausted(Arc<T>),
}

/// Failure while clearing an atomic RCU slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearError {
    /// Another writer changed the expected pointer first.
    Stale,
    /// The monotonic epoch domain is exhausted.
    EpochExhausted,
    /// The operation was attempted from IRQ/exception context.
    NotTaskContext,
    /// The caller cannot safely wait for a grace period, or already holds a
    /// reader in this domain on the current CPU.
    BadContext,
}

/// Outcome of one bounded task-context reclaim pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainStatus {
    /// Number of Arc owners dropped by this pass.
    pub dropped: usize,
    /// Whether any retired owner remains queued.
    pub pending: bool,
    /// Whether the FIFO front is waiting for an active reader grace period.
    /// A blocked queue must be reawakened by reader quiescence.
    pub blocked: bool,
}

const MAX_READER_DEPTH: u32 = DEPTH_MASK;

/// x86_64 cache-line-sized reader state.
///
/// Read-side entry and exit write admission depth and `active_epoch` on every outer
/// critical section. Keeping one CPU's fields together while separating
/// different CPUs prevents those local writes from bouncing a shared cache
/// line between cores. Registration is cold boot state, so sharing this line
/// with the same CPU's hot fields does not add steady-state contention.
#[repr(C, align(64))]
struct ReaderState {
    /// The high bit is the online-registration state; the low bits are the
    /// admission depth. Keeping these in one atomic word makes registration
    /// transitions linearizable with reader admission without a contended
    /// lock on the normal read path.
    registration: AtomicU32,
    active_epoch: AtomicU64,
}

impl ReaderState {
    const fn new() -> Self {
        Self {
            registration: AtomicU32::new(0),
            active_epoch: AtomicU64::new(0),
        }
    }
}

const REGISTERED_BIT: u32 = 1 << 31;
const DEPTH_MASK: u32 = REGISTERED_BIT - 1;

struct Retired {
    pointer: *const (),
    drop_pointer: unsafe fn(*const ()),
    epoch: u64,
}

// `Retired` is deliberately type-erased so the bounded FIFO stays compact.
// Its only safe constructors are the `T: Send + Sync` publish/clear paths;
// the bound is what makes this cross-CPU ownership transfer sound.
// Raw pointers are only moved while the domain's IRQ-safe queue lock is held;
// the pointed-to Arc strong count is retained by the queue until task-context
// reclamation.  This is the same ownership boundary as Arc::into_raw.
unsafe impl Send for Retired {}
unsafe impl Sync for Retired {}

struct RetireQueue<const CAPACITY: usize> {
    entries: [MaybeUninit<Retired>; CAPACITY],
    head: usize,
    len: usize,
    reserved: usize,
}

impl<const CAPACITY: usize> RetireQueue<CAPACITY> {
    const fn new() -> Self {
        Self {
            entries: [const { MaybeUninit::uninit() }; CAPACITY],
            head: 0,
            len: 0,
            reserved: 0,
        }
    }

    fn reserve(&mut self) -> Result<(), RcuError> {
        if self
            .len
            .checked_add(self.reserved)
            .is_none_or(|used| used >= CAPACITY)
        {
            return Err(RcuError::RetireCapacity);
        }
        self.reserved += 1;
        Ok(())
    }

    fn cancel_reservation(&mut self) {
        debug_assert!(self.reserved != 0);
        self.reserved = self
            .reserved
            .checked_sub(1)
            .expect("RCU retire reservation accounting underflow");
    }

    fn commit(&mut self, retired: Retired) {
        debug_assert!(self.reserved != 0);
        debug_assert!(self.len < CAPACITY);
        let tail = (self.head + self.len) % CAPACITY;
        self.entries[tail].write(retired);
        self.len += 1;
        self.reserved -= 1;
    }

    fn front(&self) -> Option<&Retired> {
        (self.len != 0).then(|| {
            // SAFETY: `len != 0` means the FIFO slot was initialized by
            // `commit` and has not yet been removed.
            unsafe { self.entries[self.head].assume_init_ref() }
        })
    }

    fn pop(&mut self) -> Option<Retired> {
        if self.len == 0 {
            return None;
        }
        let entry = {
            // SAFETY: the front slot is initialized while `len != 0`, and is
            // moved out exactly once before the slot is reused.
            unsafe { self.entries[self.head].assume_init_read() }
        };
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        Some(entry)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const CAPACITY: usize> Drop for RetireQueue<CAPACITY> {
    fn drop(&mut self) {
        while let Some(entry) = self.pop() {
            // A domain is normally static for the lifetime of the kernel. If
            // a test drops one, preserve ownership rather than leaking it.
            unsafe { (entry.drop_pointer)(entry.pointer) };
        }
    }
}

/// A bounded epoch domain. `MAX_CPUS` is a compile-time bound and `CAPACITY`
/// is the complete number of in-flight or queued retire reservations.
pub struct EpochDomain<P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize> {
    platform: PhantomData<P>,
    epoch: AtomicU64,
    readers: [ReaderState; MAX_CPUS],
    writer: SpinNoIrq<()>,
    retire: SpinNoIrq<RetireQueue<CAPACITY>>,
    retire_pending: AtomicBool,
    /// Retire epoch of the FIFO front, or zero while the queue is empty.
    /// Writers and the task-context reclaimer update this hint under the FIFO
    /// lock; reader exits consume it without taking that global lock.
    front_retire_epoch: AtomicU64,
    /// One outstanding edge from a grace transition to the task-context
    /// reclaimer.  Reader exits are per-CPU hot-path events; coalescing them
    /// here prevents every later quiescent reader from bouncing the same
    /// platform wake cache line while the FIFO front is already reclaimable.
    reclaim_wake_queued: AtomicBool,
}

impl<P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize>
    EpochDomain<P, MAX_CPUS, CAPACITY>
{
    /// Creates an empty domain. CPUs must be registered during boot before
    /// production readers run.
    pub const fn new() -> Self {
        Self {
            platform: PhantomData,
            epoch: AtomicU64::new(0),
            readers: [const { ReaderState::new() }; MAX_CPUS],
            writer: SpinNoIrq::new(()),
            retire: SpinNoIrq::new(RetireQueue::new()),
            retire_pending: AtomicBool::new(false),
            front_retire_epoch: AtomicU64::new(0),
            reclaim_wake_queued: AtomicBool::new(false),
        }
    }

    /// Registers one online CPU.
    ///
    /// Registration and reader admission share one atomic state word. A
    /// successful unregister therefore cannot race a reader which has already
    /// claimed admission, and a reader cannot start after the CPU has gone
    /// offline. Re-registration is deliberately explicit rather than silently
    /// treating a duplicate call as success.
    pub fn register_cpu(&self, cpu: usize) -> Result<(), RcuError> {
        if cpu >= MAX_CPUS {
            return Err(RcuError::UnregisteredCpu);
        }
        self.readers[cpu]
            .registration
            .compare_exchange(0, REGISTERED_BIT, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|state| {
                if state & REGISTERED_BIT != 0 {
                    RcuError::CpuAlreadyRegistered
                } else {
                    // No valid state has the registration bit clear while
                    // carrying a reader depth. Keep this explicit rather
                    // than turning a corrupted lifecycle transition into a
                    // false successful registration.
                    RcuError::CpuBusy
                }
            })
    }

    /// Unregisters one online CPU after it has reached a quiescent state.
    ///
    /// This operation never waits for a reader. `CpuBusy` is returned while
    /// any reader is admitted; callers can retry after the owning task has
    /// released its guard. The offline state is published only after the
    /// depth has reached zero, so grace scans may safely ignore that CPU.
    pub fn unregister_cpu(&self, cpu: usize) -> Result<(), RcuError> {
        if cpu >= MAX_CPUS {
            return Err(RcuError::UnregisteredCpu);
        }
        let reader = &self.readers[cpu];
        match reader.registration.compare_exchange(
            REGISTERED_BIT,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // The active marker is cleared before the depth reaches zero
                // in `reader_exit`. Keep this check as a defensive lifecycle
                // assertion and roll back if a platform violates that order.
                if reader.active_epoch.load(Ordering::Acquire) == 0 {
                    Ok(())
                } else {
                    let _ = reader.registration.compare_exchange(
                        0,
                        REGISTERED_BIT,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    Err(RcuError::CpuBusy)
                }
            }
            Err(0) => Err(RcuError::CpuNotRegistered),
            Err(state) if state & DEPTH_MASK != 0 => Err(RcuError::CpuBusy),
            Err(_) => Err(RcuError::CpuBusy),
        }
    }

    fn read_enter(&self) -> Result<RcuReadGuard<'_, P, MAX_CPUS, CAPACITY>, RcuError> {
        let pin = P::pin_current_cpu();
        let cpu = P::current_cpu();
        if cpu >= MAX_CPUS {
            return Err(RcuError::UnregisteredCpu);
        }

        let reader = &self.readers[cpu];
        loop {
            let admission = P::with_local_irqs_disabled(|| {
                let mut state = reader.registration.load(Ordering::Acquire);
                loop {
                    if state & REGISTERED_BIT == 0 {
                        return Err(RcuError::UnregisteredCpu);
                    }
                    let depth = state & DEPTH_MASK;
                    if depth == MAX_READER_DEPTH {
                        return Err(RcuError::ReaderNestingOverflow);
                    }
                    match reader.registration.compare_exchange(
                        state,
                        state + 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) if depth != 0 => {
                            // Local IRQ masking keeps an interrupt on this
                            // CPU from observing the admission word between
                            // the increment and the outer marker publication.
                            let active = reader.active_epoch.load(Ordering::Acquire);
                            debug_assert_ne!(active, 0);
                            return Ok(Some((true, active.saturating_sub(1))));
                        }
                        Ok(_) => break,
                        Err(next) => state = next,
                    }
                }

                let epoch = self.epoch.load(Ordering::SeqCst);
                if epoch & 1 != 0 {
                    reader.registration.fetch_sub(1, Ordering::Release);
                    return Ok(None);
                }
                // Zero is the quiescent sentinel; store epoch+1 so epoch 0
                // remains an ordinary active reader epoch.
                let Some(active_epoch) = epoch.checked_add(1) else {
                    reader.registration.fetch_sub(1, Ordering::Release);
                    return Err(RcuError::EpochExhausted);
                };
                reader.active_epoch.store(active_epoch, Ordering::SeqCst);
                // The second SeqCst load closes the race where a writer begins
                // between the first epoch load and reader admission.
                if self.epoch.load(Ordering::SeqCst) == epoch {
                    Ok(Some((false, epoch)))
                } else {
                    reader.active_epoch.store(0, Ordering::SeqCst);
                    reader.registration.fetch_sub(1, Ordering::Release);
                    Ok(None)
                }
            });
            match admission {
                Ok(Some((nested, epoch))) => {
                    return Ok(RcuReadGuard {
                        domain: self,
                        cpu,
                        epoch,
                        nested,
                        pin: Some(pin),
                    });
                }
                Ok(None) => {
                    // A writer is closing an epoch. The admission word was
                    // rolled back before leaving the IRQ-masked transition.
                    core::hint::spin_loop();
                }
                Err(error) => {
                    drop(pin);
                    return Err(error);
                }
            }
        }
    }

    fn reader_exit(&self, cpu: usize, nested: bool) {
        let outermost = !nested;
        let reader = &self.readers[cpu];
        P::with_local_irqs_disabled(|| {
            if outermost {
                reader.active_epoch.store(0, Ordering::SeqCst);
            }
            let state = reader.registration.load(Ordering::Acquire);
            debug_assert!(state & DEPTH_MASK != 0);
            reader.registration.fetch_sub(1, Ordering::Release);
        });
        if outermost {
            self.note_reader_quiescent();
        }
    }

    fn note_reader_quiescent(&self) {
        if !self.retire_pending.load(Ordering::Acquire) {
            return;
        }

        // Only the reader which makes the FIFO front reclaimable publishes a
        // wake edge.  A blocked front is re-armed by `drain`; until then one
        // queued edge is sufficient no matter how many other CPUs leave
        // newer read-side sections. The front epoch is a writer-maintained
        // atomic hint, so this hot path never contends on the retire FIFO.
        let front_epoch = self.front_retire_epoch.load(Ordering::Acquire);
        if front_epoch != 0
            && self.grace_complete(front_epoch)
            && self
                .reclaim_wake_queued
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            P::reader_quiescent();
        }
    }

    fn stable(&self, epoch: u64) -> bool {
        self.epoch.load(Ordering::SeqCst) == epoch
    }

    /// Reserves one fixed FIFO entry before a pointer publication is made.
    pub fn reserve_retire(&self) -> Result<RetireReservation<'_, P, MAX_CPUS, CAPACITY>, RcuError> {
        if self.epoch.load(Ordering::Acquire).checked_add(2).is_none() {
            return Err(RcuError::EpochExhausted);
        }
        self.retire.lock().reserve()?;
        Ok(RetireReservation {
            domain: self,
            active: true,
        })
    }

    fn cancel_reservation(&self) {
        self.retire.lock().cancel_reservation();
    }

    fn publish<T: Send + Sync>(
        &self,
        slot: &AtomicPtr<T>,
        replacement: Arc<T>,
        expected: *const T,
        mut reservation: RetireReservation<'_, P, MAX_CPUS, CAPACITY>,
    ) -> Result<Arc<T>, PublishError<T>> {
        let _writer = self.writer.lock();
        if slot.load(Ordering::Acquire).cast_const() != expected {
            drop(reservation);
            return Err(PublishError::Stale(replacement));
        }

        let old_epoch = self.epoch.load(Ordering::SeqCst);
        debug_assert_eq!(old_epoch & 1, 0);
        let Some(closing_epoch) = old_epoch.checked_add(1) else {
            drop(reservation);
            return Err(PublishError::EpochExhausted(replacement));
        };
        let Some(retire_epoch) = old_epoch.checked_add(2) else {
            drop(reservation);
            return Err(PublishError::EpochExhausted(replacement));
        };
        self.epoch.store(closing_epoch, Ordering::SeqCst);
        let replacement = Arc::into_raw(replacement).cast_mut();
        let old = slot.swap(replacement, Ordering::SeqCst);
        self.epoch.store(retire_epoch, Ordering::SeqCst);

        // The slot's strong count is transferred to the return value. Keep a
        // second strong count in the retire FIFO until a grace period passes.
        unsafe { Arc::increment_strong_count(old) };
        let returned = unsafe { Arc::from_raw(old) };
        let retired = Retired {
            pointer: old.cast(),
            drop_pointer: drop_arc::<T>,
            epoch: retire_epoch,
        };
        reservation.active = false;
        let mut queue = self.retire.lock();
        let was_empty = queue.is_empty();
        queue.commit(retired);
        if was_empty {
            self.front_retire_epoch
                .store(retire_epoch, Ordering::Release);
        }
        self.retire_pending.store(true, Ordering::Release);
        Ok(returned)
    }

    fn publish_if_empty<T: Send + Sync>(
        &self,
        slot: &AtomicPtr<T>,
        replacement: Arc<T>,
    ) -> Result<(), PublishError<T>> {
        let _writer = self.writer.lock();
        if !slot.load(Ordering::Acquire).is_null() {
            return Err(PublishError::Stale(replacement));
        }
        let old_epoch = self.epoch.load(Ordering::SeqCst);
        debug_assert_eq!(old_epoch & 1, 0);
        let Some(closing_epoch) = old_epoch.checked_add(1) else {
            return Err(PublishError::EpochExhausted(replacement));
        };
        let Some(next_epoch) = old_epoch.checked_add(2) else {
            return Err(PublishError::EpochExhausted(replacement));
        };
        self.epoch.store(closing_epoch, Ordering::SeqCst);
        slot.store(Arc::into_raw(replacement).cast_mut(), Ordering::SeqCst);
        self.epoch.store(next_epoch, Ordering::SeqCst);
        Ok(())
    }

    fn clear<T: Send + Sync>(
        &self,
        slot: &AtomicPtr<T>,
        expected: *const T,
        mut reservation: RetireReservation<'_, P, MAX_CPUS, CAPACITY>,
    ) -> Result<Arc<T>, ClearError> {
        let _writer = self.writer.lock();
        if slot.load(Ordering::Acquire).cast_const() != expected {
            drop(reservation);
            return Err(ClearError::Stale);
        }
        let old_epoch = self.epoch.load(Ordering::SeqCst);
        debug_assert_eq!(old_epoch & 1, 0);
        let Some(closing_epoch) = old_epoch.checked_add(1) else {
            drop(reservation);
            return Err(ClearError::EpochExhausted);
        };
        let Some(retire_epoch) = old_epoch.checked_add(2) else {
            drop(reservation);
            return Err(ClearError::EpochExhausted);
        };
        self.epoch.store(closing_epoch, Ordering::SeqCst);
        let old = slot.swap(core::ptr::null_mut(), Ordering::SeqCst);
        self.epoch.store(retire_epoch, Ordering::SeqCst);
        if old.is_null() {
            drop(reservation);
            return Err(ClearError::Stale);
        }
        unsafe { Arc::increment_strong_count(old) };
        let returned = unsafe { Arc::from_raw(old) };
        let retired = Retired {
            pointer: old.cast(),
            drop_pointer: drop_arc::<T>,
            epoch: retire_epoch,
        };
        reservation.active = false;
        let mut queue = self.retire.lock();
        let was_empty = queue.is_empty();
        queue.commit(retired);
        if was_empty {
            self.front_retire_epoch
                .store(retire_epoch, Ordering::Release);
        }
        self.retire_pending.store(true, Ordering::Release);
        Ok(returned)
    }

    fn grace_complete(&self, epoch: u64) -> bool {
        self.readers.iter().all(|reader| {
            let state = reader.registration.load(Ordering::SeqCst);
            let observed = reader.active_epoch.load(Ordering::SeqCst);
            if state & DEPTH_MASK != 0 {
                // Admission increments the depth before publishing the epoch
                // marker. Treat that short transition as active rather than
                // allowing a concurrent writer to reclaim through a zero
                // marker observation.
                observed != 0 && observed > epoch
            } else if state & REGISTERED_BIT == 0 {
                // `unregister_cpu` can publish the offline state only after
                // the admission depth reaches zero and the active marker is
                // cleared. Keep the marker check here as the grace-scan side
                // of that lifecycle contract instead of blindly ignoring an
                // offline bit.
                observed == 0
            } else {
                observed == 0 || observed > epoch
            }
        })
    }

    /// Drops at most `limit` retired Arc owners outside the queue lock. The
    /// caller must be a preemptible task context; this method never runs a
    /// destructor while holding the domain's IRQ-safe lock.
    pub fn drain(&self, limit: usize) -> Result<DrainStatus, RcuError> {
        if !P::in_task_context() || !P::in_preemptible_task_context() {
            return Err(RcuError::NotTaskContext);
        }
        // Acknowledge the edge which brought this consumer here before
        // testing grace state.  If the final reader exited just before this
        // store, the scan below observes a reclaimable front.  If it exits
        // afterwards, its false->true transition publishes a fresh edge.
        // Thus clearing cannot lose the last-reader wake.
        if limit != 0 {
            self.reclaim_wake_queued.store(false, Ordering::Release);
        }
        let mut dropped = 0;
        while dropped < limit {
            let entry = {
                let mut queue = self.retire.lock();
                let Some(front) = queue.front() else { break };
                if !self.grace_complete(front.epoch) {
                    break;
                }
                let entry = queue.pop();
                let next_epoch = queue.front().map_or(0, |front| front.epoch);
                self.front_retire_epoch.store(next_epoch, Ordering::Release);
                entry
            };
            let Some(entry) = entry else { break };
            unsafe { (entry.drop_pointer)(entry.pointer) };
            dropped += 1;
        }
        let (pending, blocked) = {
            let queue = self.retire.lock();
            let pending = !queue.is_empty();
            let blocked = pending
                && queue
                    .front()
                    .is_some_and(|front| !self.grace_complete(front.epoch));
            if !pending {
                self.retire_pending.store(false, Ordering::Release);
                self.front_retire_epoch.store(0, Ordering::Release);
                self.reclaim_wake_queued.store(false, Ordering::Release);
            }
            (pending, blocked)
        };
        if blocked {
            // The consumed edge may have belonged to an older FIFO front.
            // Re-arm for the newly exposed blocked front, then recheck after
            // clearing. If its final reader raced before the clear, this
            // thread observes readiness and republishes the edge; if it races
            // after the clear, the reader wins the same false->true CAS.
            self.reclaim_wake_queued.store(false, Ordering::Release);
            let front_epoch = self.front_retire_epoch.load(Ordering::Acquire);
            if front_epoch != 0
                && self.grace_complete(front_epoch)
                && self
                    .reclaim_wake_queued
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                P::reader_quiescent();
            }
        }
        Ok(DrainStatus {
            dropped,
            pending,
            blocked,
        })
    }

    /// Returns whether a task-context drain has work pending. This is only a
    /// hint; a concurrent reader may keep the FIFO temporarily unreclaimable.
    pub fn has_pending(&self) -> bool {
        !self.retire.lock().is_empty()
    }

    /// Returns whether the FIFO front can be reclaimed now. This is the
    /// readiness predicate used to arm the deferred-work worker; blocked
    /// entries are awakened by [`EpochPlatform::reader_quiescent`].
    pub fn has_reclaimable_pending(&self) -> bool {
        let queue = self.retire.lock();
        queue
            .front()
            .is_some_and(|front| self.grace_complete(front.epoch))
    }
}

impl<P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize> Default
    for EpochDomain<P, MAX_CPUS, CAPACITY>
{
    fn default() -> Self {
        Self::new()
    }
}

/// A pre-reserved publication slot. Dropping it before commit returns the
/// capacity to the FIFO without changing the visible pointer.
pub struct RetireReservation<'d, P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize> {
    domain: &'d EpochDomain<P, MAX_CPUS, CAPACITY>,
    active: bool,
}

impl<P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize> Drop
    for RetireReservation<'_, P, MAX_CPUS, CAPACITY>
{
    fn drop(&mut self) {
        if self.active {
            self.domain.cancel_reservation();
            self.active = false;
        }
    }
}

/// An atomic, Arc-owned immutable pointer published through an epoch domain.
pub struct RcuSlot<'d, T, P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize> {
    domain: &'d EpochDomain<P, MAX_CPUS, CAPACITY>,
    pointer: AtomicPtr<T>,
    // `AtomicPtr<T>` is intentionally not the ownership marker: raw pointer
    // containers do not carry T's auto-traits. The slot owns one Arc strong
    // count and may be shared between CPUs only when Arc<T> is Send + Sync.
    // Keeping that fact in the type prevents a non-thread-safe T from
    // crossing the RCU publication boundary.
    owner: PhantomData<Arc<T>>,
}

impl<'d, T, P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize>
    RcuSlot<'d, T, P, MAX_CPUS, CAPACITY>
{
    /// Takes ownership of the initial `Arc` as the slot's first strong count.
    pub fn new(domain: &'d EpochDomain<P, MAX_CPUS, CAPACITY>, initial: Arc<T>) -> Self {
        Self {
            domain,
            pointer: AtomicPtr::new(Arc::into_raw(initial).cast_mut()),
            owner: PhantomData,
        }
    }

    /// Creates a slot whose atomic pointer starts empty. Empty slots are used
    /// for immutable states such as disabled seccomp, so the disabled fast
    /// path has one pointer load and no dummy `Arc` ownership.
    pub fn empty(domain: &'d EpochDomain<P, MAX_CPUS, CAPACITY>) -> Self {
        Self {
            domain,
            pointer: AtomicPtr::new(core::ptr::null_mut()),
            owner: PhantomData,
        }
    }

    /// Returns whether the slot currently contains no published object.
    pub fn is_empty(&self) -> bool {
        // Pointer presence is a fast bit, but it must be read through the
        // publication epoch. Otherwise a writer could begin between the
        // pointer load and the caller's disabled fast-path return, creating a
        // filter-bypass window. A matching even epoch gives this observation
        // a valid linearization point before or after the publication.
        loop {
            let before = self.domain.epoch.load(Ordering::Acquire);
            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let empty = self.pointer.load(Ordering::Acquire).is_null();
            if self.domain.epoch.load(Ordering::Acquire) == before {
                return empty;
            }
        }
    }

    /// Publishes the first object into an empty slot without reserving a
    /// retire entry: there is no old owner to grace-period reclaim.
    pub fn publish_if_empty(&self, replacement: Arc<T>) -> Result<(), PublishError<T>>
    where
        T: Send + Sync,
    {
        self.domain.publish_if_empty(&self.pointer, replacement)
    }

    /// Atomically clears a published object and queues its old owner for the
    /// supplied pre-reserved grace-period retirement.
    pub fn clear(
        &self,
        expected: &Arc<T>,
        reservation: RetireReservation<'d, P, MAX_CPUS, CAPACITY>,
    ) -> Result<Arc<T>, ClearError>
    where
        T: Send + Sync,
    {
        self.domain
            .clear(&self.pointer, Arc::as_ptr(expected), reservation)
    }

    /// Loads an immutable snapshot. The short RCU guard protects the raw Arc
    /// increment; the returned Arc then owns the object independently.
    ///
    /// The caller must have registered its current CPU. Use
    /// [`Self::try_load`] when CPU lifecycle transitions are possible.
    pub fn load(&self) -> Arc<T> {
        self.try_load().unwrap_or_else(|error| match error {
            RcuError::EmptySlot => panic!("RCU slot unexpectedly empty"),
            _ => panic!("RCU current CPU is not registered"),
        })
    }

    /// Fallible form of [`Self::load`].
    pub fn try_load(&self) -> Result<Arc<T>, RcuError> {
        self.try_load_if_present()?.ok_or(RcuError::EmptySlot)
    }

    /// Loads an immutable snapshot if the slot currently contains an object.
    /// The pointer presence check and Arc strong-count increment share one
    /// RCU read-side admission, so a concurrent clear cannot turn a null
    /// check followed by `load` into a raw-pointer panic.
    ///
    /// The infallible wrapper requires a registered current CPU; callers that
    /// may race CPU offline transitions should use [`Self::try_load_if_present`].
    pub fn load_if_present(&self) -> Option<Arc<T>> {
        self.try_load_if_present()
            .unwrap_or_else(|_| panic!("RCU current CPU is not registered"))
    }

    /// Fallible form of [`Self::load_if_present`].
    pub fn try_load_if_present(&self) -> Result<Option<Arc<T>>, RcuError> {
        loop {
            let guard = self.domain.read_enter()?;
            let pointer = self.pointer.load(Ordering::Acquire);
            if pointer.is_null() {
                drop(guard);
                return Ok(None);
            }
            unsafe { Arc::increment_strong_count(pointer) };
            let value = unsafe { Arc::from_raw(pointer) };
            if guard.stable() {
                let result = guard.finish(value);
                return Ok(Some(result));
            }
            drop(value);
            drop(guard);
        }
    }

    /// Runs a read-only operation from an owned immutable snapshot.
    ///
    /// The Arc strong-count increment and RCU guard are completed before the
    /// callback starts. The callback may therefore yield, block, or migrate;
    /// it never receives a raw pointer whose lifetime depends on a
    /// non-schedulable section.
    pub fn with_current<R>(&self, operation: impl for<'a> FnOnce(&'a T) -> R) -> R {
        let value = self.load();
        operation(&value)
    }

    /// Fallible callback form for callers that may observe CPU lifecycle
    /// transitions. The callback still runs only after an owned `Arc` has
    /// replaced the short RCU read-side guard.
    pub fn try_with_current<R>(
        &self,
        operation: impl for<'a> FnOnce(&'a T) -> R,
    ) -> Result<R, RcuError> {
        let value = self.try_load()?;
        Ok(operation(&value))
    }

    /// Runs a read-only operation if an object is currently published. This
    /// closes the empty-slot race used by disabled seccomp: a caller may load
    /// the pointer as empty, then observe a concurrent publication or clear
    /// without consulting a second independent active bit.
    pub fn with_current_if_present<R>(
        &self,
        operation: impl for<'a> FnOnce(&'a T) -> R,
    ) -> Option<R> {
        let value = self.load_if_present()?;
        Some(operation(&value))
    }

    /// Fallible form of [`Self::with_current_if_present`].
    pub fn try_with_current_if_present<R>(
        &self,
        operation: impl for<'a> FnOnce(&'a T) -> R,
    ) -> Result<Option<R>, RcuError> {
        let value = self.try_load_if_present()?;
        Ok(value.map(|value| operation(&value)))
    }

    /// Reserves one retire entry before preparing a publication.
    ///
    /// The reservation is intentionally gated by `T: Send + Sync`: the
    /// eventual `Retired` entry erases T and may be dropped by a task-context
    /// drain running on another CPU. A local non-thread-safe slot may own and
    /// read its current value, but it cannot enter that type-erased queue.
    ///
    /// ```compile_fail
    /// # extern crate alloc;
    /// # use alloc::{rc::Rc, sync::Arc};
    /// # use axrcu::{EpochDomain, EpochPlatform, RcuSlot};
    /// # struct Platform;
    /// # unsafe impl EpochPlatform for Platform {
    /// #     type PinGuard = ();
    /// #     fn pin_current_cpu() {}
    /// #     fn current_cpu() -> usize { 0 }
    /// #     fn with_local_irqs_disabled<R>(operation: impl FnOnce() -> R) -> R { operation() }
    /// #     fn reader_quiescent() {}
    /// #     fn in_task_context() -> bool { true }
    /// #     fn in_preemptible_task_context() -> bool { true }
    /// # }
    /// # let domain: EpochDomain<Platform, 1, 1> = EpochDomain::new();
    /// # let slot = RcuSlot::new(&domain, Arc::new(Rc::new(1_u32)));
    /// let _reservation = slot.reserve_retire();
    /// ```
    pub fn reserve_retire(&self) -> Result<RetireReservation<'d, P, MAX_CPUS, CAPACITY>, RcuError>
    where
        T: Send + Sync,
    {
        self.domain.reserve_retire()
    }

    /// Publishes `replacement` if the exact old pointer is still installed.
    /// A failed check leaves both the slot and reservation unchanged except
    /// for releasing the reservation capacity; no visible partial update is
    /// possible.
    pub fn publish(
        &self,
        replacement: Arc<T>,
        expected: &Arc<T>,
        reservation: RetireReservation<'d, P, MAX_CPUS, CAPACITY>,
    ) -> Result<Arc<T>, PublishError<T>>
    where
        T: Send + Sync,
    {
        self.domain.publish(
            &self.pointer,
            replacement,
            Arc::as_ptr(expected),
            reservation,
        )
    }
}

impl<T, P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize> Drop
    for RcuSlot<'_, T, P, MAX_CPUS, CAPACITY>
{
    fn drop(&mut self) {
        // Every safe slot read borrows `self` for the complete operation, so
        // Rust's lifetime rules keep this slot alive until that read-side
        // guard has released its raw borrow. `load_if_present` returns an Arc
        // owner before releasing the guard, while the domain's retire FIFO
        // owns independent Arc counts for replaced values. Therefore this
        // final slot owner can be dropped without a slot-local reader count;
        // that count was never a reclamation proof and would only add a
        // contended RMW to every release-build read.
        let pointer = self.pointer.load(Ordering::Relaxed);
        if !pointer.is_null() {
            unsafe { drop(Arc::from_raw(pointer)) };
        }
    }
}

/// Internal read-side state retained until an `Arc` snapshot has been formed.
/// No safe method exposes the guarded raw pointer to callers.
pub struct RcuReadGuard<'d, P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize> {
    domain: &'d EpochDomain<P, MAX_CPUS, CAPACITY>,
    cpu: usize,
    epoch: u64,
    nested: bool,
    pin: Option<P::PinGuard>,
}

impl<P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize>
    RcuReadGuard<'_, P, MAX_CPUS, CAPACITY>
{
    /// Returns whether the epoch remained unchanged since admission.
    pub fn stable(&self) -> bool {
        // A nested reader inherits the outer reader's grace-period pin. The
        // outer guard may legitimately span a concurrent publication, so
        // requiring the nested epoch to equal the new global epoch would make
        // nested loads spin forever until the outer guard is released.
        self.nested || self.domain.stable(self.epoch)
    }

    fn finish<T>(self, value: Arc<T>) -> Arc<T> {
        drop(self);
        value
    }
}

impl<P: EpochPlatform, const MAX_CPUS: usize, const CAPACITY: usize> Drop
    for RcuReadGuard<'_, P, MAX_CPUS, CAPACITY>
{
    fn drop(&mut self) {
        self.domain.reader_exit(self.cpu, self.nested);
        let _ = self.pin.take();
    }
}

unsafe fn drop_arc<T: Send + Sync>(pointer: *const ()) {
    // SAFETY: every pointer passed here was produced by Arc::into_raw for the
    // same T and retained as exactly one queue-owned strong count.
    unsafe { drop(Arc::from_raw(pointer.cast::<T>())) };
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::{boxed::Box, sync::Arc};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use static_assertions::{assert_impl_all, assert_not_impl_any};

    std::thread_local! {
        static CONCURRENT_TEST_CPU: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }

    static TEST_QUIESCENT_EVENTS: AtomicUsize = AtomicUsize::new(0);
    static CONCURRENT_QUIESCENT_EVENTS: AtomicUsize = AtomicUsize::new(0);
    static TEST_PIN_COUNT: AtomicUsize = AtomicUsize::new(0);
    static PREEMPTIBLE_TEST_CONTEXT: AtomicBool = AtomicBool::new(true);

    struct TestPlatform;

    unsafe impl EpochPlatform for TestPlatform {
        type PinGuard = ();

        fn pin_current_cpu() -> Self::PinGuard {}

        fn current_cpu() -> usize {
            0
        }

        fn with_local_irqs_disabled<R>(operation: impl FnOnce() -> R) -> R {
            operation()
        }

        fn reader_quiescent() {
            TEST_QUIESCENT_EVENTS.fetch_add(1, Ordering::SeqCst);
        }

        fn in_task_context() -> bool {
            true
        }

        fn in_preemptible_task_context() -> bool {
            true
        }
    }

    type TestDomain = EpochDomain<TestPlatform, 1, 2>;

    struct ConcurrentTestPlatform;

    unsafe impl EpochPlatform for ConcurrentTestPlatform {
        type PinGuard = ();

        fn pin_current_cpu() -> Self::PinGuard {}

        fn current_cpu() -> usize {
            CONCURRENT_TEST_CPU.with(core::cell::Cell::get)
        }

        fn with_local_irqs_disabled<R>(operation: impl FnOnce() -> R) -> R {
            operation()
        }

        fn reader_quiescent() {
            CONCURRENT_QUIESCENT_EVENTS.fetch_add(1, Ordering::SeqCst);
        }

        fn in_task_context() -> bool {
            true
        }

        fn in_preemptible_task_context() -> bool {
            true
        }
    }

    type ConcurrentTestDomain = EpochDomain<ConcurrentTestPlatform, 2, 8>;

    std::thread_local! {
        static MIGRATION_TEST_CPU: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }

    struct MigrationPin;

    impl Drop for MigrationPin {
        fn drop(&mut self) {
            TEST_PIN_COUNT.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct MigrationTestPlatform;

    unsafe impl EpochPlatform for MigrationTestPlatform {
        type PinGuard = MigrationPin;

        fn pin_current_cpu() -> Self::PinGuard {
            TEST_PIN_COUNT.fetch_add(1, Ordering::SeqCst);
            MigrationPin
        }

        fn current_cpu() -> usize {
            MIGRATION_TEST_CPU.with(core::cell::Cell::get)
        }

        fn with_local_irqs_disabled<R>(operation: impl FnOnce() -> R) -> R {
            operation()
        }

        fn reader_quiescent() {}

        fn in_task_context() -> bool {
            true
        }

        fn in_preemptible_task_context() -> bool {
            true
        }
    }

    type MigrationTestDomain = EpochDomain<MigrationTestPlatform, 2, 2>;

    #[test]
    fn per_cpu_reader_state_has_an_exclusive_x86_cache_line() {
        assert_eq!(core::mem::align_of::<ReaderState>(), 64);
        assert_eq!(core::mem::size_of::<ReaderState>(), 64);

        let domain = ConcurrentTestDomain::new();
        let first = core::ptr::addr_of!(domain.readers[0]) as usize;
        let second = core::ptr::addr_of!(domain.readers[1]) as usize;
        assert_eq!(second - first, 64);
    }

    type NotThreadSafe = core::cell::Cell<u32>;

    assert_impl_all!(RcuSlot<'static, usize, TestPlatform, 1, 2>: Send, Sync);
    assert_not_impl_any!(RcuSlot<'static, NotThreadSafe, TestPlatform, 1, 2>: Send, Sync);

    #[test]
    fn rcu_slot_auto_traits_follow_arc_payload() {
        // This assertion is intentionally on the slot rather than AtomicPtr:
        // the PhantomData<Arc<T>> owner marker is what makes the compiler
        // require T: Send + Sync before a slot can cross CPU/thread bounds.
        let _ = core::mem::size_of::<RcuSlot<'static, usize, TestPlatform, 1, 2>>();
    }

    #[test]
    fn with_current_releases_pin_before_yield_or_migration_attempt() {
        let domain = MigrationTestDomain::new();
        domain.register_cpu(0).unwrap();
        domain.register_cpu(1).unwrap();
        MIGRATION_TEST_CPU.with(|cpu| cpu.set(0));
        TEST_PIN_COUNT.store(0, Ordering::SeqCst);
        let slot = RcuSlot::new(&domain, Arc::new(7usize));

        let observed = slot.with_current(|value| {
            // The callback owns a strong Arc, so an explicit scheduler edge
            // cannot move it away from the object it dereferences.
            assert_eq!(TEST_PIN_COUNT.load(Ordering::SeqCst), 0);
            MIGRATION_TEST_CPU.with(|cpu| cpu.set(1));
            assert_eq!(*value, 7);
            7
        });
        assert_eq!(observed, 7);
        assert_eq!(TEST_PIN_COUNT.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unregister_requires_quiescent_depth_and_allows_reregister() {
        let domain = MigrationTestDomain::new();
        domain.register_cpu(0).unwrap();
        assert_eq!(domain.register_cpu(0), Err(RcuError::CpuAlreadyRegistered));
        let reader = domain.read_enter().unwrap();
        assert_eq!(domain.unregister_cpu(0), Err(RcuError::CpuBusy));
        drop(reader);
        assert_eq!(domain.unregister_cpu(0), Ok(()));
        assert_eq!(domain.unregister_cpu(0), Err(RcuError::CpuNotRegistered));
        assert_eq!(domain.readers[0].registration.load(Ordering::Acquire), 0);
        assert_eq!(
            domain.read_enter().map(|_| ()),
            Err(RcuError::UnregisteredCpu)
        );
        let slot = RcuSlot::new(&domain, Arc::new(11usize));
        assert_eq!(slot.try_load_if_present(), Err(RcuError::UnregisteredCpu));
        domain.register_cpu(0).unwrap();
        assert!(domain.read_enter().is_ok());
    }

    #[test]
    fn grace_scan_accepts_only_quiescent_offline_cpu() {
        let domain = MigrationTestDomain::new();
        domain.register_cpu(0).unwrap();
        let slot = RcuSlot::new(&domain, Arc::new(3usize));
        let expected = slot.load();
        let reservation = slot.reserve_retire().unwrap();
        let retired = slot.clear(&expected, reservation).unwrap();
        drop(expected);
        drop(retired);
        domain.unregister_cpu(0).unwrap();
        assert_eq!(domain.drain(1).unwrap().dropped, 1);
        domain.register_cpu(0).unwrap();
    }

    struct IrqPlatform;

    unsafe impl EpochPlatform for IrqPlatform {
        type PinGuard = ();

        fn pin_current_cpu() -> Self::PinGuard {}

        fn current_cpu() -> usize {
            0
        }

        fn with_local_irqs_disabled<R>(operation: impl FnOnce() -> R) -> R {
            operation()
        }

        fn reader_quiescent() {}

        fn in_task_context() -> bool {
            false
        }

        fn in_preemptible_task_context() -> bool {
            false
        }
    }

    struct NonPreemptibleTaskPlatform;

    unsafe impl EpochPlatform for NonPreemptibleTaskPlatform {
        type PinGuard = ();

        fn pin_current_cpu() -> Self::PinGuard {}

        fn current_cpu() -> usize {
            0
        }

        fn with_local_irqs_disabled<R>(operation: impl FnOnce() -> R) -> R {
            operation()
        }

        fn reader_quiescent() {}

        fn in_task_context() -> bool {
            true
        }

        fn in_preemptible_task_context() -> bool {
            PREEMPTIBLE_TEST_CONTEXT.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn publication_reserves_capacity_and_reclaims_after_reader() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        #[derive(Debug)]
        struct Probe(Arc<AtomicUsize>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let slot = RcuSlot::new(&domain, Arc::new(Probe(drops.clone())));
        let reader = domain.read_enter().unwrap();
        let old = slot.load();
        let replacement = Arc::new(Probe(drops.clone()));
        let reservation = slot.reserve_retire().unwrap();
        let retired = slot.publish(replacement, &old, reservation).unwrap();
        drop(retired);
        let quiescent_before = TEST_QUIESCENT_EVENTS.load(Ordering::SeqCst);
        let blocked = domain.drain(2).unwrap();
        assert_eq!(blocked.dropped, 0);
        assert!(blocked.pending);
        assert!(blocked.blocked);
        assert!(!domain.has_reclaimable_pending());
        drop(reader);
        assert!(TEST_QUIESCENT_EVENTS.load(Ordering::SeqCst) > quiescent_before);
        assert!(domain.has_reclaimable_pending());
        drop(old);
        let status = domain.drain(2).unwrap();
        assert_eq!(status.dropped, 1);
        assert!(!status.pending);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn grace_wake_is_coalesced_and_rearmed_for_the_next_blocked_front() {
        let domain = ConcurrentTestDomain::new();
        domain.register_cpu(0).unwrap();
        domain.register_cpu(1).unwrap();
        CONCURRENT_QUIESCENT_EVENTS.store(0, Ordering::SeqCst);

        let slot = RcuSlot::new(&domain, Arc::new(1usize));
        let expected = slot.load();
        CONCURRENT_TEST_CPU.with(|cpu| cpu.set(0));
        let first_reader = domain.read_enter().unwrap();
        CONCURRENT_TEST_CPU.with(|cpu| cpu.set(1));
        let last_reader = domain.read_enter().unwrap();

        let reservation = slot.reserve_retire().unwrap();
        let retired = slot
            .publish(Arc::new(2usize), &expected, reservation)
            .unwrap();
        drop(retired);
        assert!(domain.drain(1).unwrap().blocked);

        // A non-final quiescent state cannot wake the consumer. A newer
        // reader also cannot do so while the old epoch is still held.
        drop(first_reader);
        assert_eq!(CONCURRENT_QUIESCENT_EVENTS.load(Ordering::SeqCst), 0);
        CONCURRENT_TEST_CPU.with(|cpu| cpu.set(0));
        let newer_reader = domain.read_enter().unwrap();
        drop(newer_reader);
        assert_eq!(CONCURRENT_QUIESCENT_EVENTS.load(Ordering::SeqCst), 0);

        // The final old reader publishes exactly one edge. Further outer
        // exits before the consumer acknowledges it must not bounce the wake
        // cache line again.
        drop(last_reader);
        assert_eq!(CONCURRENT_QUIESCENT_EVENTS.load(Ordering::SeqCst), 1);
        let extra_reader = domain.read_enter().unwrap();
        drop(extra_reader);
        assert_eq!(CONCURRENT_QUIESCENT_EVENTS.load(Ordering::SeqCst), 1);
        drop(expected);
        assert_eq!(domain.drain(1).unwrap().dropped, 1);

        // A later blocked retirement is independently armed. This also
        // covers the ordering where the final reader exits before the next
        // drain acknowledges the edge: the drain observes reclaimability and
        // reclaims directly, so no wake can be lost.
        let expected = slot.load();
        let reader = domain.read_enter().unwrap();
        let reservation = slot.reserve_retire().unwrap();
        let retired = slot
            .publish(Arc::new(3usize), &expected, reservation)
            .unwrap();
        drop(retired);
        assert!(domain.drain(1).unwrap().blocked);
        drop(reader);
        assert_eq!(CONCURRENT_QUIESCENT_EVENTS.load(Ordering::SeqCst), 2);
        drop(expected);
        assert_eq!(domain.drain(1).unwrap().dropped, 1);

        // The opposite ordering is safe as well: if the last reader exits
        // before drain acknowledges its edge, clearing the edge at drain
        // entry is followed by a grace scan that observes the ready front.
        let expected = slot.load();
        let reader = domain.read_enter().unwrap();
        let reservation = slot.reserve_retire().unwrap();
        let retired = slot
            .publish(Arc::new(4usize), &expected, reservation)
            .unwrap();
        drop(retired);
        drop(reader);
        assert_eq!(CONCURRENT_QUIESCENT_EVENTS.load(Ordering::SeqCst), 3);
        drop(expected);
        assert_eq!(domain.drain(1).unwrap().dropped, 1);
    }

    #[test]
    fn draining_a_ready_front_rearms_the_blocked_front_behind_it() {
        let domain = ConcurrentTestDomain::new();
        domain.register_cpu(0).unwrap();
        CONCURRENT_TEST_CPU.with(|cpu| cpu.set(0));
        CONCURRENT_QUIESCENT_EVENTS.store(0, Ordering::SeqCst);

        let slot = RcuSlot::new(&domain, Arc::new(1usize));
        let first_expected = slot.load();
        let first_reservation = slot.reserve_retire().unwrap();
        let first_retired = slot
            .publish(Arc::new(2usize), &first_expected, first_reservation)
            .unwrap();
        drop(first_retired);
        drop(first_expected);

        // This reader starts after the first retirement, so it cannot block
        // that front, but it does block the second retirement behind it.
        let reader = domain.read_enter().unwrap();
        let second_expected = slot.load();
        let second_reservation = slot.reserve_retire().unwrap();
        let second_retired = slot
            .publish(Arc::new(3usize), &second_expected, second_reservation)
            .unwrap();
        drop(second_retired);
        drop(second_expected);

        // Model the already queued edge which brought the worker to the ready
        // first front. The bounded drain consumes it, exposes the blocked
        // second front, and must re-arm before returning.
        domain.reclaim_wake_queued.store(true, Ordering::Release);
        let status = domain.drain(1).unwrap();
        assert_eq!(status.dropped, 1);
        assert!(status.pending);
        assert!(status.blocked);
        assert!(!domain.reclaim_wake_queued.load(Ordering::Acquire));

        drop(reader);
        assert_eq!(CONCURRENT_QUIESCENT_EVENTS.load(Ordering::SeqCst), 1);
        assert_eq!(domain.drain(1).unwrap().dropped, 1);
    }

    #[test]
    fn failed_publication_releases_reservation_without_changing_slot() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let first = Arc::new(1usize);
        let second = Arc::new(2usize);
        let slot = RcuSlot::new(&domain, first.clone());
        let reservation = slot.reserve_retire().unwrap();
        let replacement = Arc::new(3usize);
        assert!(slot.publish(replacement, &second, reservation).is_err());
        assert_eq!(*slot.load(), 1);
        assert!(!domain.has_pending());
        assert!(slot.reserve_retire().is_ok());
    }

    #[test]
    fn retire_queue_capacity_is_reserved_before_publication() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let slot = RcuSlot::new(&domain, Arc::new(0usize));
        let mut expected = slot.load();
        let mut retired = alloc::vec::Vec::new();
        for value in [1usize, 2] {
            let reservation = slot.reserve_retire().unwrap();
            let old = slot
                .publish(Arc::new(value), &expected, reservation)
                .unwrap_or_else(|_| panic!("fresh bounded publication failed"));
            retired.push(old);
            expected = slot.load();
        }
        assert!(matches!(
            domain.reserve_retire(),
            Err(RcuError::RetireCapacity)
        ));
        drop(expected);
        drop(retired);
        assert_eq!(domain.drain(2).unwrap().dropped, 2);
    }

    #[test]
    fn nested_readers_share_one_active_epoch() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let outer = domain.read_enter().unwrap();
        let inner = domain.read_enter().unwrap();
        assert!(outer.stable());
        assert!(inner.stable());
        drop(inner);
        assert!(outer.stable());
        drop(outer);
        assert_eq!(domain.drain(1).unwrap().dropped, 0);
    }

    #[test]
    fn empty_slot_has_a_single_publication_state_and_bounded_terminal_clear() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let slot = RcuSlot::empty(&domain);
        assert!(slot.is_empty());
        assert_eq!(slot.with_current_if_present(|_| 1), None);

        slot.publish_if_empty(Arc::new(7usize)).unwrap();
        assert!(!slot.is_empty());
        assert_eq!(slot.with_current_if_present(|value| *value), Some(7));

        let expected = slot.load();
        let reservation = slot.reserve_retire().unwrap();
        let taken = slot.clear(&expected, reservation).unwrap();
        drop(expected);
        assert!(slot.is_empty());
        assert_eq!(slot.with_current_if_present(|_| 1), None);
        assert_eq!(*taken, 7);
        drop(taken);
        assert_eq!(domain.drain(1).unwrap().dropped, 1);
    }

    #[test]
    fn terminal_clear_is_bounded_while_reader_blocks_grace() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let slot = RcuSlot::new(&domain, Arc::new(9usize));
        let expected = slot.load();
        let reader = domain.read_enter().unwrap();
        let reservation = slot.reserve_retire().unwrap();
        let taken = slot.clear(&expected, reservation).unwrap();
        assert!(slot.is_empty());
        assert!(domain.drain(1).unwrap().blocked);
        drop(reader);
        drop(expected);
        drop(taken);
        assert_eq!(domain.drain(1).unwrap().dropped, 1);
    }

    #[test]
    fn publication_overlapping_evaluation_keeps_the_old_state_valid() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let slot = RcuSlot::new(&domain, Arc::new(1usize));
        let expected = slot.load();

        let observed = slot.with_current_if_present(|state| {
            assert_eq!(*state, 1);
            let reservation = slot.reserve_retire().unwrap();
            let retired = slot
                .publish(Arc::new(2usize), &expected, reservation)
                .unwrap();
            // The publication happened while this evaluation still held its
            // epoch pin; the old immutable object remains dereferenceable.
            let old_value = *state;
            drop(retired);
            old_value
        });
        assert_eq!(observed, Some(1));
        drop(expected);
        assert_eq!(slot.with_current_if_present(|value| *value), Some(2));
        let current = slot.load();
        drop(current);
        assert_eq!(domain.drain(1).unwrap().dropped, 1);
    }

    #[test]
    fn load_if_present_is_safe_while_another_cpu_publishes_and_clears() {
        use std::sync::atomic::AtomicBool;

        let domain: &'static ConcurrentTestDomain =
            Box::leak(Box::new(ConcurrentTestDomain::new()));
        domain.register_cpu(0).unwrap();
        domain.register_cpu(1).unwrap();
        let slot = Arc::new(RcuSlot::new(domain, Arc::new(1usize)));
        let running = Arc::new(AtomicBool::new(true));
        let reader_slot = slot.clone();
        let reader_running = running.clone();
        let reader = std::thread::spawn(move || {
            CONCURRENT_TEST_CPU.with(|cpu| cpu.set(1));
            while reader_running.load(Ordering::Relaxed) {
                let _ = reader_slot.load_if_present();
            }
        });

        CONCURRENT_TEST_CPU.with(|cpu| cpu.set(0));
        for value in 2..16 {
            let expected = slot.load_if_present().unwrap();
            let reservation = loop {
                match slot.reserve_retire() {
                    Ok(reservation) => break reservation,
                    Err(RcuError::RetireCapacity) => {
                        let _ = domain.drain(8);
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("unexpected RCU reservation error: {error:?}"),
                }
            };
            let retired = slot
                .publish(Arc::new(value), &expected, reservation)
                .unwrap();
            drop(expected);
            drop(retired);
            let _ = domain.drain(8);
        }

        let expected = slot.load_if_present().unwrap();
        let reservation = loop {
            match slot.reserve_retire() {
                Ok(reservation) => break reservation,
                Err(RcuError::RetireCapacity) => {
                    let _ = domain.drain(8);
                    std::thread::yield_now();
                }
                Err(error) => panic!("unexpected RCU reservation error: {error:?}"),
            }
        };
        let retired = slot.clear(&expected, reservation).unwrap();
        drop(expected);
        drop(retired);
        assert!(slot.load_if_present().is_none());
        running.store(false, Ordering::Release);
        reader.join().unwrap();
        let _ = domain.drain(8);
    }

    #[test]
    fn drain_rejects_irq_context_before_touching_queue() {
        let domain = EpochDomain::<IrqPlatform, 1, 1>::new();
        assert_eq!(domain.drain(1), Err(RcuError::NotTaskContext));
    }

    #[test]
    fn drain_rejects_non_preemptible_task_context_without_dropping() {
        #[derive(Debug)]
        struct Probe(Arc<AtomicUsize>);

        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let domain = EpochDomain::<NonPreemptibleTaskPlatform, 1, 1>::new();
        domain.register_cpu(0).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let slot = RcuSlot::new(&domain, Arc::new(Probe(drops.clone())));
        let expected = slot.load();
        let reservation = slot.reserve_retire().unwrap();
        let retired = slot
            .publish(Arc::new(Probe(drops.clone())), &expected, reservation)
            .unwrap();
        drop(expected);
        drop(retired);

        PREEMPTIBLE_TEST_CONTEXT.store(false, Ordering::SeqCst);
        assert_eq!(domain.drain(1), Err(RcuError::NotTaskContext));
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        PREEMPTIBLE_TEST_CONTEXT.store(true, Ordering::SeqCst);
        assert_eq!(domain.drain(1).unwrap().dropped, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn terminal_clear_stays_queued_until_task_context() {
        let domain = EpochDomain::<IrqPlatform, 1, 1>::new();
        domain.register_cpu(0).unwrap();
        let slot = RcuSlot::new(&domain, Arc::new(11usize));
        let expected = slot.load();
        let reservation = slot.reserve_retire().unwrap();
        let retired = slot.clear(&expected, reservation).unwrap();
        drop(expected);
        drop(retired);
        assert!(slot.is_empty());
        assert_eq!(domain.drain(1), Err(RcuError::NotTaskContext));
    }

    #[test]
    fn epoch_overflow_rejects_reservation_and_publications_without_mutation() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let slot = RcuSlot::new(&domain, Arc::new(1usize));
        let terminal_epoch = u64::MAX - 1;

        domain.epoch.store(terminal_epoch, Ordering::SeqCst);
        assert!(matches!(
            domain.reserve_retire(),
            Err(RcuError::EpochExhausted)
        ));
        assert_eq!(domain.epoch.load(Ordering::SeqCst), terminal_epoch);
        assert_eq!(domain.retire.lock().len, 0);
        assert_eq!(domain.retire.lock().reserved, 0);

        domain.epoch.store(0, Ordering::SeqCst);
        let expected = slot.load();
        let reservation = slot.reserve_retire().unwrap();
        domain.epoch.store(terminal_epoch, Ordering::SeqCst);
        let result = slot.publish(Arc::new(2usize), &expected, reservation);
        assert!(matches!(result, Err(PublishError::EpochExhausted(_))));
        assert_eq!(*slot.load(), 1);
        assert_eq!(domain.epoch.load(Ordering::SeqCst), terminal_epoch);
        assert_eq!(domain.retire.lock().len, 0);
        assert_eq!(domain.retire.lock().reserved, 0);
        drop(expected);

        domain.epoch.store(0, Ordering::SeqCst);
        let expected = slot.load();
        let reservation = slot.reserve_retire().unwrap();
        domain.epoch.store(terminal_epoch, Ordering::SeqCst);
        assert!(matches!(
            slot.clear(&expected, reservation),
            Err(ClearError::EpochExhausted)
        ));
        assert_eq!(*slot.load(), 1);
        assert_eq!(domain.epoch.load(Ordering::SeqCst), terminal_epoch);
        assert_eq!(domain.retire.lock().len, 0);
        assert_eq!(domain.retire.lock().reserved, 0);
        drop(expected);
    }

    #[test]
    fn empty_publication_rejects_epoch_overflow_without_mutation() {
        let domain = TestDomain::new();
        domain.register_cpu(0).unwrap();
        let slot = RcuSlot::empty(&domain);
        let terminal_epoch = u64::MAX - 1;
        domain.epoch.store(terminal_epoch, Ordering::SeqCst);

        assert!(matches!(
            slot.publish_if_empty(Arc::new(2usize)),
            Err(PublishError::EpochExhausted(_))
        ));
        assert!(slot.is_empty());
        assert_eq!(domain.epoch.load(Ordering::SeqCst), terminal_epoch);
        assert_eq!(domain.retire.lock().len, 0);
        assert_eq!(domain.retire.lock().reserved, 0);
    }

    #[test]
    fn reader_nesting_overflow_is_rejected_without_changing_admission() {
        let domain = TestDomain::new();
        domain.readers[0]
            .registration
            .store(REGISTERED_BIT | MAX_READER_DEPTH, Ordering::SeqCst);
        assert_eq!(
            domain.read_enter().map(|_| ()),
            Err(RcuError::ReaderNestingOverflow)
        );
        assert_eq!(
            domain.readers[0].registration.load(Ordering::SeqCst),
            REGISTERED_BIT | MAX_READER_DEPTH
        );
        assert_eq!(domain.readers[0].active_epoch.load(Ordering::SeqCst), 0);
    }
}
