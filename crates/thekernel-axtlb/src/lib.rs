#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate std;

use core::{
    fmt,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

const CPU_OFFLINE: usize = 0;
const CPU_ONLINE: usize = 1;
const CPU_DRAINING: usize = 2;
const CPU_STATE_MASK: usize = 0b11;
const ADMISSION_ONE: usize = CPU_STATE_MASK + 1;

/// Reserved mailbox reason for a CPU-maintenance shootdown request.
pub const CPU_MAINTENANCE_REASON: IpiReason = IpiReason { index: 0 };

/// Fixed set of remotely acknowledged CPU-maintenance operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuMaintenance {
    bits: u8,
}

impl CpuMaintenance {
    /// Page-table translation invalidation only.
    pub const TLB: Self = Self { bits: 1 << 0 };
    /// Instruction-stream synchronization only.
    pub const ICACHE: Self = Self { bits: 1 << 1 };
    /// Both translation invalidation and instruction-stream synchronization.
    pub const TLB_AND_ICACHE: Self = Self {
        bits: Self::TLB.bits | Self::ICACHE.bits,
    };

    /// Returns whether translation invalidation is required.
    pub const fn needs_tlb(self) -> bool {
        self.bits & Self::TLB.bits != 0
    }

    /// Returns whether instruction-stream synchronization is required.
    pub const fn needs_icache(self) -> bool {
        self.bits & Self::ICACHE.bits != 0
    }

    fn from_pending(tlb: bool, icache: bool) -> Option<Self> {
        match (tlb, icache) {
            (false, false) => None,
            (true, false) => Some(Self::TLB),
            (false, true) => Some(Self::ICACHE),
            (true, true) => Some(Self::TLB_AND_ICACHE),
        }
    }

    const fn is_empty(self) -> bool {
        self.bits == 0
    }
}

/// A fixed mailbox reason represented by one machine-word bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpiReason {
    index: u8,
}

impl IpiReason {
    /// Constructs a reason if `index` fits in the fixed machine-word mailbox.
    pub const fn try_new(index: usize) -> Option<Self> {
        if index < usize::BITS as usize {
            Some(Self { index: index as u8 })
        } else {
            None
        }
    }

    /// Returns the stable bit index of this reason.
    pub const fn index(self) -> usize {
        self.index as usize
    }

    /// Returns the single mailbox bit representing this reason.
    pub const fn bit(self) -> usize {
        1usize << self.index
    }
}

/// CPU lifecycle or identity failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CpuLifecycleError {
    /// The CPU index is outside this domain's fixed storage.
    InvalidCpu,
    /// The requested transition does not match the CPU's current state.
    InvalidState,
    /// A CPU cannot publish online while old mailbox state remains.
    MailboxNotDrained,
    /// An offline transition still has a target-admission reader.
    AdmissionInProgress,
    /// An offline transition still has unacknowledged CPU-maintenance work.
    ShootdownPending,
    /// An offline transition still has an undispatched IPI reason.
    ReasonPending,
}

/// Failure while issuing a CPU-maintenance shootdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShootdownIssueError {
    /// The issuer index is outside this domain's fixed storage.
    InvalidCpu,
    /// The issuer has not been published online.
    IssuerOffline,
    /// The fixed lifecycle word cannot represent another concurrent admission.
    AdmissionExhausted,
    /// The global non-wrapping epoch space is exhausted.
    EpochExhausted,
}

/// Failure while reading or servicing one CPU mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MailboxError {
    /// The CPU index is outside this domain's fixed storage.
    InvalidCpu,
}

/// Failure while posting a fixed non-maintenance IPI reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReasonPostError {
    /// The CPU index is outside this domain's fixed storage.
    InvalidCpu,
    /// The target is offline or already draining toward offline.
    CpuUnavailable,
    /// The fixed lifecycle word cannot represent another concurrent admission.
    AdmissionExhausted,
    /// The reserved maintenance reason must be published with a shootdown epoch.
    ReservedMaintenanceReason,
}

/// Result of posting one fixed IPI reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasonPost {
    needs_kick: bool,
}

impl ReasonPost {
    /// Returns whether hardware must be kicked for the newly pending bit.
    pub const fn needs_kick(self) -> bool {
        self.needs_kick
    }
}

/// Opaque proof that every target acknowledged one domain's requested work.
#[must_use = "affected state may be reclaimed or published only after observing this grace"]
pub struct ShootdownGrace<'domain, const MAX_CPUS: usize> {
    domain: &'domain TlbShootdown<MAX_CPUS>,
    epoch: u64,
    maintenance: CpuMaintenance,
}

impl<const MAX_CPUS: usize> ShootdownGrace<'_, MAX_CPUS> {
    /// Returns the completed global epoch for bounded diagnostics.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the exact maintenance operations covered by this grace.
    pub const fn maintenance(&self) -> CpuMaintenance {
        self.maintenance
    }
}

impl<const MAX_CPUS: usize> fmt::Debug for ShootdownGrace<'_, MAX_CPUS> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShootdownGrace")
            .field("epoch", &self.epoch)
            .field("maintenance", &self.maintenance)
            .finish_non_exhaustive()
    }
}

impl<const MAX_CPUS: usize> PartialEq for ShootdownGrace<'_, MAX_CPUS> {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch
            && self.maintenance == other.maintenance
            && core::ptr::eq(self.domain, other.domain)
    }
}

impl<const MAX_CPUS: usize> Eq for ShootdownGrace<'_, MAX_CPUS> {}

/// Result of servicing all CPU-maintenance work visible in one mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShootdownService {
    completed_tlb_epoch: u64,
    completed_icache_epoch: u64,
    tlb_flush_count: usize,
    icache_flush_count: usize,
}

impl ShootdownService {
    /// Returns the greatest translation-invalidation epoch acknowledged.
    pub const fn completed_tlb_epoch(self) -> u64 {
        self.completed_tlb_epoch
    }

    /// Returns the greatest instruction-sync epoch acknowledged.
    pub const fn completed_icache_epoch(self) -> u64 {
        self.completed_icache_epoch
    }

    /// Returns how many callback rounds requested a full local TLB flush.
    pub const fn tlb_flush_count(self) -> usize {
        self.tlb_flush_count
    }

    /// Returns how many callback rounds requested a local instruction sync.
    pub const fn icache_flush_count(self) -> usize {
        self.icache_flush_count
    }
}

/// Read-only mailbox facts used by timeout diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuSnapshot {
    online: bool,
    draining: bool,
    admissions: usize,
    pending_reasons: usize,
    requested_tlb_epoch: u64,
    completed_tlb_epoch: u64,
    requested_icache_epoch: u64,
    completed_icache_epoch: u64,
}

impl CpuSnapshot {
    /// Returns whether this CPU accepts new shootdown targets.
    pub const fn is_online(self) -> bool {
        self.online
    }

    /// Returns whether this CPU is closing target admission before offline.
    pub const fn is_draining(self) -> bool {
        self.draining
    }

    /// Returns the number of issuers currently publishing to this mailbox.
    pub const fn admissions(self) -> usize {
        self.admissions
    }

    /// Returns the pending reason bitset.
    pub const fn pending_reasons(self) -> usize {
        self.pending_reasons
    }

    /// Returns the greatest requested translation-invalidation epoch.
    pub const fn requested_tlb_epoch(self) -> u64 {
        self.requested_tlb_epoch
    }

    /// Returns the greatest acknowledged translation-invalidation epoch.
    pub const fn completed_tlb_epoch(self) -> u64 {
        self.completed_tlb_epoch
    }

    /// Returns the greatest requested instruction-sync epoch.
    pub const fn requested_icache_epoch(self) -> u64 {
        self.requested_icache_epoch
    }

    /// Returns the greatest acknowledged instruction-sync epoch.
    pub const fn completed_icache_epoch(self) -> u64 {
        self.completed_icache_epoch
    }
}

struct CpuMailbox {
    // State and reader count share one CAS word. Separate atomics would leave
    // a load-online/increment gap in which an offline/re-online cycle could
    // miss an in-flight admission.
    lifecycle: AtomicUsize,
    pending_reasons: AtomicUsize,
    requested_tlb_epoch: AtomicU64,
    completed_tlb_epoch: AtomicU64,
    requested_icache_epoch: AtomicU64,
    completed_icache_epoch: AtomicU64,
}

impl CpuMailbox {
    const fn new() -> Self {
        Self {
            lifecycle: AtomicUsize::new(CPU_OFFLINE),
            pending_reasons: AtomicUsize::new(0),
            requested_tlb_epoch: AtomicU64::new(0),
            completed_tlb_epoch: AtomicU64::new(0),
            requested_icache_epoch: AtomicU64::new(0),
            completed_icache_epoch: AtomicU64::new(0),
        }
    }

    fn try_admit(&self) -> Result<CpuAdmission<'_>, AdmissionError> {
        let mut lifecycle = self.lifecycle.load(Ordering::Acquire);
        loop {
            if lifecycle & CPU_STATE_MASK != CPU_ONLINE {
                return Err(AdmissionError::Unavailable);
            }
            let next = lifecycle
                .checked_add(ADMISSION_ONE)
                .ok_or(AdmissionError::Exhausted)?;
            match self.lifecycle.compare_exchange_weak(
                lifecycle,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(CpuAdmission { mailbox: self }),
                Err(observed) => lifecycle = observed,
            }
        }
    }

    fn release_admission(&self) {
        let previous = self.lifecycle.fetch_sub(ADMISSION_ONE, Ordering::Release);
        debug_assert!(previous >> 2 > 0);
    }

    fn post_reason(&self, reason: IpiReason) -> ReasonPost {
        let previous = self
            .pending_reasons
            .fetch_or(reason.bit(), Ordering::AcqRel);
        ReasonPost {
            needs_kick: previous & reason.bit() == 0,
        }
    }

    fn snapshot(&self) -> CpuSnapshot {
        let lifecycle = self.lifecycle.load(Ordering::Acquire);
        let state = lifecycle & CPU_STATE_MASK;
        CpuSnapshot {
            online: state == CPU_ONLINE,
            draining: state == CPU_DRAINING,
            admissions: lifecycle >> 2,
            pending_reasons: self.pending_reasons.load(Ordering::Acquire),
            requested_tlb_epoch: self.requested_tlb_epoch.load(Ordering::Acquire),
            completed_tlb_epoch: self.completed_tlb_epoch.load(Ordering::Acquire),
            requested_icache_epoch: self.requested_icache_epoch.load(Ordering::Acquire),
            completed_icache_epoch: self.completed_icache_epoch.load(Ordering::Acquire),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionError {
    Unavailable,
    Exhausted,
}

#[must_use = "dropping the admission releases the CPU lifecycle reader"]
struct CpuAdmission<'mailbox> {
    mailbox: &'mailbox CpuMailbox,
}

impl Drop for CpuAdmission<'_> {
    fn drop(&mut self) {
        self.mailbox.release_admission();
    }
}

/// Fixed-capacity allocation-free SMP TLB shootdown state.
pub struct TlbShootdown<const MAX_CPUS: usize> {
    next_epoch: AtomicU64,
    cpus: [CpuMailbox; MAX_CPUS],
}

impl<const MAX_CPUS: usize> TlbShootdown<MAX_CPUS> {
    /// Creates an empty domain with every CPU offline and epoch zero.
    pub const fn new() -> Self {
        assert!(
            MAX_CPUS > 0,
            "a TLB shootdown domain needs at least one CPU slot"
        );
        Self {
            next_epoch: AtomicU64::new(0),
            cpus: [const { CpuMailbox::new() }; MAX_CPUS],
        }
    }

    fn cpu(&self, cpu: usize) -> Result<&CpuMailbox, CpuLifecycleError> {
        self.cpus.get(cpu).ok_or(CpuLifecycleError::InvalidCpu)
    }

    fn mailbox(&self, cpu: usize) -> Result<&CpuMailbox, MailboxError> {
        self.cpus.get(cpu).ok_or(MailboxError::InvalidCpu)
    }

    /// Publishes a fully initialized and locally synchronized CPU as a target.
    pub fn publish_online(&self, cpu: usize) -> Result<(), CpuLifecycleError> {
        let mailbox = self.cpu(cpu)?;
        if mailbox.pending_reasons.load(Ordering::Acquire) != 0
            || mailbox.requested_tlb_epoch.load(Ordering::Acquire)
                != mailbox.completed_tlb_epoch.load(Ordering::Acquire)
            || mailbox.requested_icache_epoch.load(Ordering::Acquire)
                != mailbox.completed_icache_epoch.load(Ordering::Acquire)
        {
            return Err(CpuLifecycleError::MailboxNotDrained);
        }
        mailbox
            .lifecycle
            .compare_exchange(
                CPU_OFFLINE,
                CPU_ONLINE,
                Ordering::Release,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| CpuLifecycleError::InvalidState)
    }

    /// Stops new target admission for a CPU before its mailbox is drained.
    pub fn begin_offline(&self, cpu: usize) -> Result<(), CpuLifecycleError> {
        let mailbox = self.cpu(cpu)?;
        let mut lifecycle = mailbox.lifecycle.load(Ordering::Acquire);
        loop {
            if lifecycle & CPU_STATE_MASK != CPU_ONLINE {
                return Err(CpuLifecycleError::InvalidState);
            }
            let draining = (lifecycle & !CPU_STATE_MASK) | CPU_DRAINING;
            match mailbox.lifecycle.compare_exchange_weak(
                lifecycle,
                draining,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => lifecycle = observed,
            }
        }
    }

    /// Commits offline only after admission, reasons, and maintenance are drained.
    pub fn complete_offline(&self, cpu: usize) -> Result<(), CpuLifecycleError> {
        let mailbox = self.cpu(cpu)?;
        let lifecycle = mailbox.lifecycle.load(Ordering::Acquire);
        if lifecycle & CPU_STATE_MASK != CPU_DRAINING {
            return Err(CpuLifecycleError::InvalidState);
        }
        if lifecycle >> 2 != 0 {
            return Err(CpuLifecycleError::AdmissionInProgress);
        }
        if mailbox.requested_tlb_epoch.load(Ordering::Acquire)
            > mailbox.completed_tlb_epoch.load(Ordering::Acquire)
            || mailbox.requested_icache_epoch.load(Ordering::Acquire)
                > mailbox.completed_icache_epoch.load(Ordering::Acquire)
        {
            return Err(CpuLifecycleError::ShootdownPending);
        }
        if mailbox.pending_reasons.load(Ordering::Acquire) != 0 {
            return Err(CpuLifecycleError::ReasonPending);
        }
        mailbox
            .lifecycle
            .compare_exchange(lifecycle, CPU_OFFLINE, Ordering::Release, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| CpuLifecycleError::InvalidState)
    }

    /// Posts a fixed non-maintenance reason to one online CPU.
    pub fn post_non_maintenance_reason(
        &self,
        cpu: usize,
        reason: IpiReason,
    ) -> Result<ReasonPost, ReasonPostError> {
        if reason == CPU_MAINTENANCE_REASON {
            return Err(ReasonPostError::ReservedMaintenanceReason);
        }
        let mailbox = self.cpus.get(cpu).ok_or(ReasonPostError::InvalidCpu)?;
        let _admission = mailbox.try_admit().map_err(|error| match error {
            AdmissionError::Unavailable => ReasonPostError::CpuUnavailable,
            AdmissionError::Exhausted => ReasonPostError::AdmissionExhausted,
        })?;
        let post = mailbox.post_reason(reason);
        Ok(post)
    }

    /// Issues one global request after the caller's PTE stores and local flush.
    ///
    /// Every online CPU except `issuer_cpu` becomes a target. Hardware IPIs must
    /// be sent only to CPUs for which [`ShootdownRequest::needs_kick`] is true.
    /// Any error leaves the caller without a grace request after its local
    /// mutation, and may follow partial target publication. A real adapter must
    /// therefore fail-stop instead of reclaiming or publishing affected state.
    pub fn issue_after_local_flush(
        &self,
        issuer_cpu: usize,
    ) -> Result<ShootdownRequest<'_, MAX_CPUS>, ShootdownIssueError> {
        self.issue_after_local_maintenance(issuer_cpu, CpuMaintenance::TLB)
    }

    /// Issues one request after the caller completed the matching local work.
    ///
    /// The request uses one global epoch but records translation and
    /// instruction-sync acknowledgement independently on every target. This
    /// lets executable publication request only instruction synchronization,
    /// while `mprotect +X` can request both operations without conflating them.
    pub fn issue_after_local_maintenance(
        &self,
        issuer_cpu: usize,
        maintenance: CpuMaintenance,
    ) -> Result<ShootdownRequest<'_, MAX_CPUS>, ShootdownIssueError> {
        self.issue_after_local_maintenance_with(issuer_cpu, maintenance, || {})
    }

    #[cfg(test)]
    fn issue_after_local_flush_with(
        &self,
        issuer_cpu: usize,
        after_issuer_admission: impl FnOnce(),
    ) -> Result<ShootdownRequest<'_, MAX_CPUS>, ShootdownIssueError> {
        self.issue_after_local_maintenance_with(
            issuer_cpu,
            CpuMaintenance::TLB,
            after_issuer_admission,
        )
    }

    fn issue_after_local_maintenance_with(
        &self,
        issuer_cpu: usize,
        maintenance: CpuMaintenance,
        after_issuer_admission: impl FnOnce(),
    ) -> Result<ShootdownRequest<'_, MAX_CPUS>, ShootdownIssueError> {
        debug_assert!(!maintenance.is_empty());
        let Some(issuer) = self.cpus.get(issuer_cpu) else {
            return Err(ShootdownIssueError::InvalidCpu);
        };
        let issuer_admission = issuer.try_admit().map_err(|error| match error {
            AdmissionError::Unavailable => ShootdownIssueError::IssuerOffline,
            AdmissionError::Exhausted => ShootdownIssueError::AdmissionExhausted,
        })?;
        after_issuer_admission();
        // This RMW is sequenced after the caller's PTE stores. AcqRel makes
        // the total global epoch order carry earlier writers' stores forward:
        // acknowledging a later epoch therefore also covers every earlier one.
        let previous = self
            .next_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                epoch.checked_add(1)
            })
            .map_err(|_| ShootdownIssueError::EpochExhausted)?;
        let epoch = previous + 1;
        let mut targets = [false; MAX_CPUS];
        let mut kicks = [false; MAX_CPUS];

        for (cpu, mailbox) in self.cpus.iter().enumerate() {
            if cpu == issuer_cpu {
                continue;
            }
            let _target_admission = match mailbox.try_admit() {
                Ok(admission) => admission,
                Err(AdmissionError::Unavailable) => continue,
                Err(AdmissionError::Exhausted) => {
                    return Err(ShootdownIssueError::AdmissionExhausted);
                }
            };
            if maintenance.needs_tlb() {
                mailbox
                    .requested_tlb_epoch
                    .fetch_max(epoch, Ordering::Release);
            }
            if maintenance.needs_icache() {
                mailbox
                    .requested_icache_epoch
                    .fetch_max(epoch, Ordering::Release);
            }
            let post = mailbox.post_reason(CPU_MAINTENANCE_REASON);
            targets[cpu] = true;
            kicks[cpu] = post.needs_kick();
        }

        Ok(ShootdownRequest {
            domain: self,
            _issuer_admission: issuer_admission,
            epoch,
            maintenance,
            targets,
            kicks,
        })
    }

    /// Atomically takes all currently pending reason bits for one CPU.
    pub fn take_pending_reasons(&self, cpu: usize) -> Result<usize, MailboxError> {
        Ok(self.mailbox(cpu)?.pending_reasons.swap(0, Ordering::AcqRel))
    }

    /// Services every maintenance epoch visible to one CPU.
    ///
    /// `maintain_local` receives the exact nonempty union needed in one round.
    /// It runs without any lock owned by this crate. The adapter must execute
    /// every requested operation, stay allocation-free, and must not acquire
    /// address-space, frame, pin, or mailbox locks. When both bits are present,
    /// translation invalidation must precede instruction-stream synchronization.
    pub fn service_maintenance(
        &self,
        cpu: usize,
        mut maintain_local: impl FnMut(CpuMaintenance),
    ) -> Result<ShootdownService, MailboxError> {
        let mailbox = self.mailbox(cpu)?;
        let mut tlb_flush_count = 0usize;
        let mut icache_flush_count = 0usize;

        loop {
            let requested_tlb = mailbox.requested_tlb_epoch.load(Ordering::Acquire);
            let completed_tlb = mailbox.completed_tlb_epoch.load(Ordering::Acquire);
            let requested_icache = mailbox.requested_icache_epoch.load(Ordering::Acquire);
            let completed_icache = mailbox.completed_icache_epoch.load(Ordering::Acquire);
            let Some(maintenance) = CpuMaintenance::from_pending(
                completed_tlb < requested_tlb,
                completed_icache < requested_icache,
            ) else {
                return Ok(ShootdownService {
                    completed_tlb_epoch: completed_tlb,
                    completed_icache_epoch: completed_icache,
                    tlb_flush_count,
                    icache_flush_count,
                });
            };
            maintain_local(maintenance);
            if maintenance.needs_tlb() {
                tlb_flush_count = tlb_flush_count.saturating_add(1);
                mailbox
                    .completed_tlb_epoch
                    .fetch_max(requested_tlb, Ordering::Release);
            }
            if maintenance.needs_icache() {
                icache_flush_count = icache_flush_count.saturating_add(1);
                mailbox
                    .completed_icache_epoch
                    .fetch_max(requested_icache, Ordering::Release);
            }
        }
    }

    /// Returns a read-only mailbox snapshot for bounded timeout diagnostics.
    pub fn cpu_snapshot(&self, cpu: usize) -> Result<CpuSnapshot, MailboxError> {
        Ok(self.mailbox(cpu)?.snapshot())
    }
}

impl<const MAX_CPUS: usize> Default for TlbShootdown<MAX_CPUS> {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed target and hardware-kick facts for one global shootdown epoch.
///
/// The request borrows the domain that issued it, so safe code cannot ask a
/// different domain to manufacture grace for an unrelated epoch. The grace
/// returned by [`Self::try_complete`] carries the same domain borrow.
///
/// ```compile_fail
/// use axtlb::TlbShootdown;
///
/// let request = {
///     let domain = TlbShootdown::<1>::new();
///     domain.publish_online(0).unwrap();
///     domain.issue_after_local_flush(0).unwrap()
/// };
/// let _grace = request.try_complete();
/// ```
#[must_use = "a shootdown request must reach grace or force the adapter to stop"]
pub struct ShootdownRequest<'domain, const MAX_CPUS: usize> {
    domain: &'domain TlbShootdown<MAX_CPUS>,
    _issuer_admission: CpuAdmission<'domain>,
    epoch: u64,
    maintenance: CpuMaintenance,
    targets: [bool; MAX_CPUS],
    kicks: [bool; MAX_CPUS],
}

impl<'domain, const MAX_CPUS: usize> ShootdownRequest<'domain, MAX_CPUS> {
    /// Returns this request's nonzero global epoch.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the exact maintenance operations this request must acknowledge.
    pub const fn maintenance(&self) -> CpuMaintenance {
        self.maintenance
    }

    /// Returns whether `cpu` must acknowledge this request.
    pub fn targets(&self, cpu: usize) -> bool {
        self.targets.get(cpu).copied().unwrap_or(false)
    }

    /// Returns whether this issue must send a hardware IPI to `cpu`.
    pub fn needs_kick(&self, cpu: usize) -> bool {
        self.kicks.get(cpu).copied().unwrap_or(false)
    }

    /// Returns the number of CPUs whose acknowledgement is required.
    pub fn target_count(&self) -> usize {
        self.targets.iter().filter(|targeted| **targeted).count()
    }

    /// Returns grace only when every target in this request acknowledged it.
    pub fn try_complete(&self) -> Option<ShootdownGrace<'domain, MAX_CPUS>> {
        for (cpu, targeted) in self.targets.iter().copied().enumerate() {
            if targeted {
                let mailbox = &self.domain.cpus[cpu];
                if self.maintenance.needs_tlb()
                    && mailbox.completed_tlb_epoch.load(Ordering::Acquire) < self.epoch
                {
                    return None;
                }
                if self.maintenance.needs_icache()
                    && mailbox.completed_icache_epoch.load(Ordering::Acquire) < self.epoch
                {
                    return None;
                }
            }
        }
        Some(ShootdownGrace {
            domain: self.domain,
            epoch: self.epoch,
            maintenance: self.maintenance,
        })
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::{
        sync::{Arc, Barrier, mpsc},
        thread,
    };

    use super::*;

    fn online<const N: usize>(domain: &TlbShootdown<N>) {
        for cpu in 0..N {
            domain.publish_online(cpu).unwrap();
        }
    }

    #[test]
    fn reason_indices_are_machine_word_bounded() {
        assert_eq!(IpiReason::try_new(0).unwrap().bit(), 1);
        assert!(IpiReason::try_new(usize::BITS as usize - 1).is_some());
        assert!(IpiReason::try_new(usize::BITS as usize).is_none());
    }

    #[test]
    fn issue_targets_every_other_online_cpu() {
        let domain = TlbShootdown::<4>::new();
        online(&domain);

        let request = domain.issue_after_local_flush(1).unwrap();
        assert_eq!(request.epoch(), 1);
        assert_eq!(request.target_count(), 3);
        assert!(!request.targets(1));
        assert!(request.needs_kick(0));
        assert!(request.needs_kick(2));
        assert!(request.needs_kick(3));
        assert!(request.try_complete().is_none());
    }

    #[test]
    fn pending_tlb_requests_coalesce_to_latest_epoch() {
        let domain = TlbShootdown::<2>::new();
        online(&domain);

        let first = domain.issue_after_local_flush(0).unwrap();
        let second = domain.issue_after_local_flush(0).unwrap();
        assert!(first.needs_kick(1));
        assert!(!second.needs_kick(1));
        assert_eq!(
            domain.take_pending_reasons(1).unwrap(),
            CPU_MAINTENANCE_REASON.bit()
        );

        let flushes = AtomicUsize::new(0);
        let service = domain
            .service_maintenance(1, |maintenance| {
                assert_eq!(maintenance, CpuMaintenance::TLB);
                flushes.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(service.completed_tlb_epoch(), second.epoch());
        assert_eq!(service.completed_icache_epoch(), 0);
        assert_eq!(service.tlb_flush_count(), 1);
        assert_eq!(service.icache_flush_count(), 0);
        assert_eq!(flushes.load(Ordering::Relaxed), 1);
        assert_eq!(first.try_complete().unwrap().epoch(), first.epoch());
        assert_eq!(second.try_complete().unwrap().epoch(), second.epoch());
    }

    #[test]
    fn grace_is_never_fabricated_before_ack() {
        let domain = TlbShootdown::<2>::new();
        online(&domain);
        let request = domain.issue_after_local_flush(0).unwrap();

        assert!(request.try_complete().is_none());
        assert_eq!(
            domain.take_pending_reasons(1).unwrap(),
            CPU_MAINTENANCE_REASON.bit()
        );
        domain.service_maintenance(1, |_| {}).unwrap();
        assert_eq!(request.try_complete().unwrap().epoch(), 1);
    }

    #[test]
    fn mixed_maintenance_coalesces_but_acknowledges_each_class_epoch() {
        let domain = TlbShootdown::<2>::new();
        online(&domain);
        let icache = domain
            .issue_after_local_maintenance(0, CpuMaintenance::ICACHE)
            .unwrap();
        let tlb = domain.issue_after_local_flush(0).unwrap();

        assert!(icache.needs_kick(1));
        assert!(!tlb.needs_kick(1));
        assert_eq!(
            domain.take_pending_reasons(1).unwrap(),
            CPU_MAINTENANCE_REASON.bit()
        );
        let service = domain
            .service_maintenance(1, |maintenance| {
                assert_eq!(maintenance, CpuMaintenance::TLB_AND_ICACHE);
            })
            .unwrap();
        assert_eq!(service.completed_tlb_epoch(), tlb.epoch());
        assert_eq!(service.completed_icache_epoch(), icache.epoch());
        assert_eq!(service.tlb_flush_count(), 1);
        assert_eq!(service.icache_flush_count(), 1);
        assert_eq!(
            icache.try_complete().unwrap().maintenance(),
            CpuMaintenance::ICACHE
        );
        assert_eq!(
            tlb.try_complete().unwrap().maintenance(),
            CpuMaintenance::TLB
        );
    }

    #[test]
    fn combined_request_does_not_reach_grace_after_only_tlb_ack() {
        let domain = TlbShootdown::<2>::new();
        online(&domain);
        let request = domain
            .issue_after_local_maintenance(0, CpuMaintenance::TLB_AND_ICACHE)
            .unwrap();
        domain.cpus[1]
            .completed_tlb_epoch
            .store(request.epoch(), Ordering::Release);

        assert!(request.try_complete().is_none());
        assert_eq!(
            domain.take_pending_reasons(1).unwrap(),
            CPU_MAINTENANCE_REASON.bit()
        );
        let service = domain
            .service_maintenance(1, |maintenance| {
                assert_eq!(maintenance, CpuMaintenance::ICACHE);
            })
            .unwrap();
        assert_eq!(service.tlb_flush_count(), 0);
        assert_eq!(service.icache_flush_count(), 1);
        assert_eq!(
            request.try_complete().unwrap().maintenance(),
            CpuMaintenance::TLB_AND_ICACHE
        );
    }

    #[test]
    fn grace_identity_includes_the_issuing_domain() {
        let first = TlbShootdown::<1>::new();
        let second = TlbShootdown::<1>::new();
        online(&first);
        online(&second);

        let first_grace = first
            .issue_after_local_flush(0)
            .unwrap()
            .try_complete()
            .unwrap();
        let second_grace = second
            .issue_after_local_flush(0)
            .unwrap()
            .try_complete()
            .unwrap();
        assert_eq!(first_grace.epoch(), second_grace.epoch());
        assert_ne!(first_grace, second_grace);
    }

    #[test]
    fn non_maintenance_reasons_share_the_fixed_bit_mailbox() {
        let domain = TlbShootdown::<1>::new();
        online(&domain);
        let reason = IpiReason::try_new(3).unwrap();

        assert!(
            domain
                .post_non_maintenance_reason(0, reason)
                .unwrap()
                .needs_kick()
        );
        assert!(
            !domain
                .post_non_maintenance_reason(0, reason)
                .unwrap()
                .needs_kick()
        );
        assert_eq!(domain.take_pending_reasons(0).unwrap(), reason.bit());
        assert_eq!(
            domain.post_non_maintenance_reason(0, CPU_MAINTENANCE_REASON),
            Err(ReasonPostError::ReservedMaintenanceReason)
        );
    }

    #[test]
    fn offline_requires_admission_reason_and_ack_drain() {
        let domain = TlbShootdown::<2>::new();
        online(&domain);
        let request = domain.issue_after_local_flush(0).unwrap();
        domain.begin_offline(1).unwrap();

        assert_eq!(
            domain.complete_offline(1),
            Err(CpuLifecycleError::ShootdownPending)
        );
        assert_eq!(
            domain.take_pending_reasons(1).unwrap(),
            CPU_MAINTENANCE_REASON.bit()
        );
        domain.service_maintenance(1, |_| {}).unwrap();
        domain.complete_offline(1).unwrap();
        assert!(request.try_complete().is_some());
        assert!(!domain.cpu_snapshot(1).unwrap().is_online());

        let next = domain.issue_after_local_flush(0).unwrap();
        assert_eq!(next.target_count(), 0);
        assert!(next.try_complete().is_some());
    }

    #[test]
    fn issuer_admission_blocks_offline_completion_until_publication_finishes() {
        let domain = Arc::new(TlbShootdown::<2>::new());
        online(&domain);
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let issuer_domain = domain.clone();

        let issuer = thread::spawn(move || {
            let request = issuer_domain
                .issue_after_local_flush_with(0, || {
                    admitted_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                })
                .unwrap();
            (request.epoch(), request.targets(1))
        });

        admitted_rx.recv().unwrap();
        domain.begin_offline(0).unwrap();
        let snapshot = domain.cpu_snapshot(0).unwrap();
        assert!(snapshot.is_draining());
        assert_eq!(snapshot.admissions(), 1);
        assert_eq!(
            domain.complete_offline(0),
            Err(CpuLifecycleError::AdmissionInProgress)
        );

        resume_tx.send(()).unwrap();
        let (epoch, targeted_remote) = issuer.join().unwrap();
        assert_eq!(epoch, 1);
        assert!(targeted_remote);
        domain.complete_offline(0).unwrap();
    }

    #[test]
    fn live_request_keeps_issuer_admitted_until_grace_is_observed() {
        let domain = TlbShootdown::<2>::new();
        online(&domain);
        let request = domain.issue_after_local_flush(0).unwrap();

        domain.begin_offline(0).unwrap();
        assert_eq!(domain.cpu_snapshot(0).unwrap().admissions(), 1);
        assert_eq!(
            domain.complete_offline(0),
            Err(CpuLifecycleError::AdmissionInProgress)
        );
        assert_eq!(
            domain.take_pending_reasons(1).unwrap(),
            CPU_MAINTENANCE_REASON.bit()
        );
        domain.service_maintenance(1, |_| {}).unwrap();
        let grace = request.try_complete().unwrap();
        drop(request);
        domain.complete_offline(0).unwrap();
        assert_eq!(grace.epoch(), 1);
    }

    #[test]
    fn invalid_cpu_and_issuer_state_are_explicit() {
        let domain = TlbShootdown::<2>::new();
        assert_eq!(
            domain.issue_after_local_flush(0).err(),
            Some(ShootdownIssueError::IssuerOffline)
        );
        assert_eq!(
            domain.issue_after_local_flush(2).err(),
            Some(ShootdownIssueError::InvalidCpu)
        );
        assert_eq!(domain.publish_online(2), Err(CpuLifecycleError::InvalidCpu));
        assert_eq!(
            domain.take_pending_reasons(2),
            Err(MailboxError::InvalidCpu)
        );
    }

    #[test]
    fn epoch_exhaustion_is_explicit_and_never_wraps() {
        let domain = TlbShootdown::<1>::new();
        online(&domain);
        domain.next_epoch.store(u64::MAX, Ordering::Relaxed);

        assert_eq!(
            domain.issue_after_local_flush(0).err(),
            Some(ShootdownIssueError::EpochExhausted)
        );
        assert_eq!(domain.next_epoch.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(domain.cpu_snapshot(0).unwrap().admissions(), 0);
    }

    #[test]
    fn lifecycle_admission_exhaustion_is_explicit_and_releases_the_issuer() {
        let domain = TlbShootdown::<2>::new();
        online(&domain);
        let exhausted_online = (usize::MAX & !CPU_STATE_MASK) | CPU_ONLINE;
        domain.cpus[1]
            .lifecycle
            .store(exhausted_online, Ordering::Relaxed);

        assert_eq!(
            domain.issue_after_local_flush(0).err(),
            Some(ShootdownIssueError::AdmissionExhausted)
        );
        assert_eq!(domain.cpu_snapshot(0).unwrap().admissions(), 0);
        let reason = IpiReason::try_new(1).unwrap();
        assert_eq!(
            domain.post_non_maintenance_reason(1, reason),
            Err(ReasonPostError::AdmissionExhausted)
        );
    }

    #[test]
    fn concurrent_issuers_share_one_bounded_mailbox_bit() {
        let domain = Arc::new(TlbShootdown::<3>::new());
        online(&domain);
        let barrier = Arc::new(Barrier::new(3));
        let mut issuers = std::vec::Vec::new();

        for issuer in 0..2 {
            let domain = domain.clone();
            let barrier = barrier.clone();
            issuers.push(thread::spawn(move || {
                barrier.wait();
                let request = domain.issue_after_local_flush(issuer).unwrap();
                (request.epoch(), request.needs_kick(2))
            }));
        }
        barrier.wait();
        let first = issuers.remove(0).join().unwrap();
        let second = issuers.remove(0).join().unwrap();

        assert_ne!(first.0, second.0);
        assert_eq!(usize::from(first.1) + usize::from(second.1), 1);
        for cpu in 0..3 {
            let reasons = domain.take_pending_reasons(cpu).unwrap();
            if reasons & CPU_MAINTENANCE_REASON.bit() != 0 {
                domain.service_maintenance(cpu, |_| {}).unwrap();
            }
        }
        let completed = domain.cpu_snapshot(2).unwrap().completed_tlb_epoch();
        assert!(completed >= first.0);
        assert!(completed >= second.0);
    }
}
