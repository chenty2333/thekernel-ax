#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate std;

use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

const CPU_OFFLINE: u8 = 0;
const CPU_ONLINE: u8 = 1;
const CPU_DRAINING: u8 = 2;

/// Reserved mailbox reason for a TLB shootdown request.
pub const TLB_SHOOTDOWN_REASON: IpiReason = IpiReason { index: 0 };

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
    /// An offline transition still has an unacknowledged TLB request.
    ShootdownPending,
    /// An offline transition still has an undispatched IPI reason.
    ReasonPending,
}

/// Failure while issuing a TLB shootdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShootdownIssueError {
    /// The issuer index is outside this domain's fixed storage.
    InvalidCpu,
    /// The issuer has not been published online.
    IssuerOffline,
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

/// Failure while posting a fixed non-TLB IPI reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReasonPostError {
    /// The CPU index is outside this domain's fixed storage.
    InvalidCpu,
    /// The target is offline or already draining toward offline.
    CpuUnavailable,
    /// The reserved TLB reason must be published with a shootdown epoch.
    ReservedTlbReason,
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

/// Opaque proof that every target acknowledged the request epoch.
#[derive(Debug, PartialEq, Eq)]
pub struct TlbGrace {
    epoch: u64,
}

impl TlbGrace {
    /// Returns the completed global epoch for bounded diagnostics.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// Result of servicing all TLB work visible in one mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlbService {
    completed_epoch: u64,
    flush_count: usize,
}

impl TlbService {
    /// Returns the greatest epoch acknowledged by this service call.
    pub const fn completed_epoch(self) -> u64 {
        self.completed_epoch
    }

    /// Returns how many local full flushes were required.
    pub const fn flush_count(self) -> usize {
        self.flush_count
    }
}

/// Read-only mailbox facts used by timeout diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuSnapshot {
    online: bool,
    draining: bool,
    admissions: usize,
    pending_reasons: usize,
    requested_epoch: u64,
    completed_epoch: u64,
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

    /// Returns the greatest requested TLB epoch.
    pub const fn requested_epoch(self) -> u64 {
        self.requested_epoch
    }

    /// Returns the greatest acknowledged TLB epoch.
    pub const fn completed_epoch(self) -> u64 {
        self.completed_epoch
    }
}

struct CpuMailbox {
    state: AtomicU8,
    admissions: AtomicUsize,
    pending_reasons: AtomicUsize,
    requested_epoch: AtomicU64,
    completed_epoch: AtomicU64,
}

impl CpuMailbox {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(CPU_OFFLINE),
            admissions: AtomicUsize::new(0),
            pending_reasons: AtomicUsize::new(0),
            requested_epoch: AtomicU64::new(0),
            completed_epoch: AtomicU64::new(0),
        }
    }

    fn try_admit(&self) -> bool {
        if self.state.load(Ordering::Acquire) != CPU_ONLINE {
            return false;
        }
        self.admissions.fetch_add(1, Ordering::AcqRel);
        if self.state.load(Ordering::Acquire) == CPU_ONLINE {
            true
        } else {
            self.admissions.fetch_sub(1, Ordering::Release);
            false
        }
    }

    fn release_admission(&self) {
        let previous = self.admissions.fetch_sub(1, Ordering::Release);
        debug_assert!(previous > 0);
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
        let state = self.state.load(Ordering::Acquire);
        CpuSnapshot {
            online: state == CPU_ONLINE,
            draining: state == CPU_DRAINING,
            admissions: self.admissions.load(Ordering::Acquire),
            pending_reasons: self.pending_reasons.load(Ordering::Acquire),
            requested_epoch: self.requested_epoch.load(Ordering::Acquire),
            completed_epoch: self.completed_epoch.load(Ordering::Acquire),
        }
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

    /// Publishes a fully initialized, locally flushed CPU as an IPI target.
    pub fn publish_online(&self, cpu: usize) -> Result<(), CpuLifecycleError> {
        let mailbox = self.cpu(cpu)?;
        if mailbox.admissions.load(Ordering::Acquire) != 0
            || mailbox.pending_reasons.load(Ordering::Acquire) != 0
            || mailbox.requested_epoch.load(Ordering::Acquire)
                != mailbox.completed_epoch.load(Ordering::Acquire)
        {
            return Err(CpuLifecycleError::MailboxNotDrained);
        }
        mailbox
            .state
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
        self.cpu(cpu)?
            .state
            .compare_exchange(
                CPU_ONLINE,
                CPU_DRAINING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| CpuLifecycleError::InvalidState)
    }

    /// Commits offline only after admission, reasons, and TLB work are drained.
    pub fn complete_offline(&self, cpu: usize) -> Result<(), CpuLifecycleError> {
        let mailbox = self.cpu(cpu)?;
        if mailbox.state.load(Ordering::Acquire) != CPU_DRAINING {
            return Err(CpuLifecycleError::InvalidState);
        }
        if mailbox.admissions.load(Ordering::Acquire) != 0 {
            return Err(CpuLifecycleError::AdmissionInProgress);
        }
        if mailbox.requested_epoch.load(Ordering::Acquire)
            > mailbox.completed_epoch.load(Ordering::Acquire)
        {
            return Err(CpuLifecycleError::ShootdownPending);
        }
        if mailbox.pending_reasons.load(Ordering::Acquire) != 0 {
            return Err(CpuLifecycleError::ReasonPending);
        }
        mailbox
            .state
            .compare_exchange(
                CPU_DRAINING,
                CPU_OFFLINE,
                Ordering::Release,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| CpuLifecycleError::InvalidState)
    }

    /// Posts a fixed non-TLB reason to one online CPU.
    pub fn post_non_tlb_reason(
        &self,
        cpu: usize,
        reason: IpiReason,
    ) -> Result<ReasonPost, ReasonPostError> {
        if reason == TLB_SHOOTDOWN_REASON {
            return Err(ReasonPostError::ReservedTlbReason);
        }
        let mailbox = self.cpus.get(cpu).ok_or(ReasonPostError::InvalidCpu)?;
        if !mailbox.try_admit() {
            return Err(ReasonPostError::CpuUnavailable);
        }
        let post = mailbox.post_reason(reason);
        mailbox.release_admission();
        Ok(post)
    }

    /// Issues one global request after the caller's PTE stores and local flush.
    ///
    /// Every online CPU except `issuer_cpu` becomes a target. Hardware IPIs must
    /// be sent only to CPUs for which [`ShootdownRequest::needs_kick`] is true.
    pub fn issue_after_local_flush(
        &self,
        issuer_cpu: usize,
    ) -> Result<ShootdownRequest<'_, MAX_CPUS>, ShootdownIssueError> {
        let Some(issuer) = self.cpus.get(issuer_cpu) else {
            return Err(ShootdownIssueError::InvalidCpu);
        };
        if issuer.state.load(Ordering::Acquire) != CPU_ONLINE {
            return Err(ShootdownIssueError::IssuerOffline);
        }
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
            if cpu == issuer_cpu || !mailbox.try_admit() {
                continue;
            }
            mailbox.requested_epoch.fetch_max(epoch, Ordering::Release);
            let post = mailbox.post_reason(TLB_SHOOTDOWN_REASON);
            targets[cpu] = true;
            kicks[cpu] = post.needs_kick();
            mailbox.release_admission();
        }

        Ok(ShootdownRequest {
            domain: self,
            epoch,
            targets,
            kicks,
        })
    }

    /// Atomically takes all currently pending reason bits for one CPU.
    pub fn take_pending_reasons(&self, cpu: usize) -> Result<usize, MailboxError> {
        Ok(self.mailbox(cpu)?.pending_reasons.swap(0, Ordering::AcqRel))
    }

    /// Services every TLB epoch visible to one CPU with local full flushes.
    ///
    /// `flush_local_all` runs without any lock owned by this crate. The adapter
    /// must keep it allocation-free and must not acquire address-space, frame,
    /// pin, or mailbox locks.
    pub fn service_tlb(
        &self,
        cpu: usize,
        mut flush_local_all: impl FnMut(),
    ) -> Result<TlbService, MailboxError> {
        let mailbox = self.mailbox(cpu)?;
        let mut flush_count = 0;

        loop {
            let requested = mailbox.requested_epoch.load(Ordering::Acquire);
            let completed = mailbox.completed_epoch.load(Ordering::Acquire);
            if completed >= requested {
                return Ok(TlbService {
                    completed_epoch: completed,
                    flush_count,
                });
            }
            flush_local_all();
            flush_count = flush_count.saturating_add(1);
            mailbox
                .completed_epoch
                .fetch_max(requested, Ordering::Release);
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
/// different domain to manufacture grace for an unrelated epoch.
///
/// ```compile_fail
/// use axtlb::TlbShootdown;
///
/// let first = TlbShootdown::<1>::new();
/// let second = TlbShootdown::<1>::new();
/// first.publish_online(0).unwrap();
/// second.publish_online(0).unwrap();
/// let request = first.issue_after_local_flush(0).unwrap();
/// let _wrong_domain_grace = second.try_complete(&request);
/// ```
pub struct ShootdownRequest<'domain, const MAX_CPUS: usize> {
    domain: &'domain TlbShootdown<MAX_CPUS>,
    epoch: u64,
    targets: [bool; MAX_CPUS],
    kicks: [bool; MAX_CPUS],
}

impl<const MAX_CPUS: usize> ShootdownRequest<'_, MAX_CPUS> {
    /// Returns this request's nonzero global epoch.
    pub const fn epoch(&self) -> u64 {
        self.epoch
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
    pub fn try_complete(&self) -> Option<TlbGrace> {
        for (cpu, targeted) in self.targets.iter().copied().enumerate() {
            if targeted
                && self.domain.cpus[cpu]
                    .completed_epoch
                    .load(Ordering::Acquire)
                    < self.epoch
            {
                return None;
            }
        }
        Some(TlbGrace { epoch: self.epoch })
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::{
        sync::{Arc, Barrier},
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
            TLB_SHOOTDOWN_REASON.bit()
        );

        let flushes = AtomicUsize::new(0);
        let service = domain
            .service_tlb(1, || {
                flushes.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(service.completed_epoch(), second.epoch());
        assert_eq!(service.flush_count(), 1);
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
            TLB_SHOOTDOWN_REASON.bit()
        );
        domain.service_tlb(1, || {}).unwrap();
        assert_eq!(request.try_complete().unwrap().epoch(), 1);
    }

    #[test]
    fn non_tlb_reasons_share_the_fixed_bit_mailbox() {
        let domain = TlbShootdown::<1>::new();
        online(&domain);
        let reason = IpiReason::try_new(3).unwrap();

        assert!(domain.post_non_tlb_reason(0, reason).unwrap().needs_kick());
        assert!(!domain.post_non_tlb_reason(0, reason).unwrap().needs_kick());
        assert_eq!(domain.take_pending_reasons(0).unwrap(), reason.bit());
        assert_eq!(
            domain.post_non_tlb_reason(0, TLB_SHOOTDOWN_REASON),
            Err(ReasonPostError::ReservedTlbReason)
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
            TLB_SHOOTDOWN_REASON.bit()
        );
        domain.service_tlb(1, || {}).unwrap();
        domain.complete_offline(1).unwrap();
        assert!(request.try_complete().is_some());
        assert!(!domain.cpu_snapshot(1).unwrap().is_online());

        let next = domain.issue_after_local_flush(0).unwrap();
        assert_eq!(next.target_count(), 0);
        assert!(next.try_complete().is_some());
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
            if reasons & TLB_SHOOTDOWN_REASON.bit() != 0 {
                domain.service_tlb(cpu, || {}).unwrap();
            }
        }
        let completed = domain.cpu_snapshot(2).unwrap().completed_epoch();
        assert!(completed >= first.0);
        assert!(completed >= second.0);
    }
}
