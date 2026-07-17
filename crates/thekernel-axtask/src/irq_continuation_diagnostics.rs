use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) const EVENT_PREEMPT_DISABLE_IRQ_OFF: u64 = 1;
pub(crate) const EVENT_PREEMPT_ENABLE_IRQ_OFF: u64 = 2;
pub(crate) const EVENT_PREEMPT_ENABLE_RETURN_IRQ_OFF: u64 = 3;
pub(crate) const EVENT_PREEMPT_CHECK_IRQ_OFF: u64 = 4;
pub(crate) const EVENT_PREEMPT_CHECK_RETURN_IRQ_OFF: u64 = 5;
pub(crate) const EVENT_CONTEXT_SWITCH: u64 = 6;
pub(crate) const EVENT_YIELD_ENTER_IRQ_OFF: u64 = 7;
pub(crate) const EVENT_YIELD_RETURN_IRQ_OFF: u64 = 8;
pub(crate) const EVENT_IDLE_AFTER_YIELD_IRQ_OFF: u64 = 9;
pub(crate) const EVENT_CONTEXT_SWITCH_RETURN: u64 = 10;

pub(crate) const FLAG_IRQS_ENABLED: u64 = 1 << 0;
pub(crate) const FLAG_IDLE: u64 = 1 << 1;
pub(crate) const FLAG_NEED_RESCHED: u64 = 1 << 2;
pub(crate) const FLAG_RESCHED_ALLOWED: u64 = 1 << 3;
pub(crate) const FLAG_PEER_IDLE: u64 = 1 << 4;

const TRACE_DEPTH: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct IrqContinuationDiagnosticEvent {
    pub sequence: u64,
    pub kind: u64,
    pub task_id: u64,
    pub peer_task_id: u64,
    pub flags: u64,
    pub preempt_disable_count: u64,
}

impl IrqContinuationDiagnosticEvent {
    #[cfg(test)]
    const EMPTY: Self = Self {
        sequence: 0,
        kind: 0,
        task_id: 0,
        peer_task_id: 0,
        flags: 0,
        preempt_disable_count: 0,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct IrqContinuationDiagnosticSnapshot {
    pub latest_sequence: u64,
    pub timer_events: u64,
    pub context_switches: u64,
    pub context_switch_returns: u64,
    pub irq_off_preempt_disables: u64,
    pub irq_off_preempt_enables: u64,
    pub irq_off_outermost_preempt_enables: u64,
    pub irq_off_preempt_enable_returns: u64,
    pub irq_off_preempt_checks: u64,
    pub irq_off_preempt_check_returns: u64,
    pub irq_off_yield_entries: u64,
    pub irq_off_yield_returns: u64,
    pub irq_off_idle_boundaries: u64,
}

struct EventSlot {
    sequence: AtomicU64,
    kind: AtomicU64,
    task_id: AtomicU64,
    peer_task_id: AtomicU64,
    flags: AtomicU64,
    preempt_disable_count: AtomicU64,
}

impl EventSlot {
    const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            kind: AtomicU64::new(0),
            task_id: AtomicU64::new(0),
            peer_task_id: AtomicU64::new(0),
            flags: AtomicU64::new(0),
            preempt_disable_count: AtomicU64::new(0),
        }
    }

    fn publish(
        &self,
        sequence: u64,
        kind: u64,
        task_id: u64,
        peer_task_id: u64,
        flags: u64,
        preempt_disable_count: usize,
    ) {
        self.sequence.store(0, Ordering::Relaxed);
        self.kind.store(kind, Ordering::Relaxed);
        self.task_id.store(task_id, Ordering::Relaxed);
        self.peer_task_id.store(peer_task_id, Ordering::Relaxed);
        self.flags.store(flags, Ordering::Relaxed);
        self.preempt_disable_count
            .store(preempt_disable_count as u64, Ordering::Relaxed);
        self.sequence.store(sequence, Ordering::Release);
    }

    fn snapshot(&self, expected_sequence: u64) -> Option<IrqContinuationDiagnosticEvent> {
        if expected_sequence == 0 || self.sequence.load(Ordering::Acquire) != expected_sequence {
            return None;
        }
        let event = IrqContinuationDiagnosticEvent {
            sequence: expected_sequence,
            kind: self.kind.load(Ordering::Relaxed),
            task_id: self.task_id.load(Ordering::Relaxed),
            peer_task_id: self.peer_task_id.load(Ordering::Relaxed),
            flags: self.flags.load(Ordering::Relaxed),
            preempt_disable_count: self.preempt_disable_count.load(Ordering::Relaxed),
        };
        (self.sequence.load(Ordering::Acquire) == expected_sequence).then_some(event)
    }
}

#[repr(align(64))]
struct CpuDiagnostics {
    next_sequence: AtomicU64,
    published_sequence: AtomicU64,
    timer_events: AtomicU64,
    context_switches: AtomicU64,
    context_switch_returns: AtomicU64,
    irq_off_preempt_disables: AtomicU64,
    irq_off_preempt_enables: AtomicU64,
    irq_off_outermost_preempt_enables: AtomicU64,
    irq_off_preempt_enable_returns: AtomicU64,
    irq_off_preempt_checks: AtomicU64,
    irq_off_preempt_check_returns: AtomicU64,
    irq_off_yield_entries: AtomicU64,
    irq_off_yield_returns: AtomicU64,
    irq_off_idle_boundaries: AtomicU64,
    events: [EventSlot; TRACE_DEPTH],
}

impl CpuDiagnostics {
    const fn new() -> Self {
        Self {
            next_sequence: AtomicU64::new(0),
            published_sequence: AtomicU64::new(0),
            timer_events: AtomicU64::new(0),
            context_switches: AtomicU64::new(0),
            context_switch_returns: AtomicU64::new(0),
            irq_off_preempt_disables: AtomicU64::new(0),
            irq_off_preempt_enables: AtomicU64::new(0),
            irq_off_outermost_preempt_enables: AtomicU64::new(0),
            irq_off_preempt_enable_returns: AtomicU64::new(0),
            irq_off_preempt_checks: AtomicU64::new(0),
            irq_off_preempt_check_returns: AtomicU64::new(0),
            irq_off_yield_entries: AtomicU64::new(0),
            irq_off_yield_returns: AtomicU64::new(0),
            irq_off_idle_boundaries: AtomicU64::new(0),
            events: [const { EventSlot::new() }; TRACE_DEPTH],
        }
    }

    fn record(
        &self,
        kind: u64,
        task_id: u64,
        peer_task_id: u64,
        flags: u64,
        preempt_disable_count: usize,
    ) {
        match kind {
            EVENT_PREEMPT_DISABLE_IRQ_OFF => {
                self.irq_off_preempt_disables
                    .fetch_add(1, Ordering::Relaxed);
            }
            EVENT_PREEMPT_ENABLE_IRQ_OFF => {
                self.irq_off_preempt_enables.fetch_add(1, Ordering::Relaxed);
                if preempt_disable_count == 1 {
                    self.irq_off_outermost_preempt_enables
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            EVENT_PREEMPT_ENABLE_RETURN_IRQ_OFF => {
                self.irq_off_preempt_enable_returns
                    .fetch_add(1, Ordering::Relaxed);
            }
            EVENT_PREEMPT_CHECK_IRQ_OFF => {
                self.irq_off_preempt_checks.fetch_add(1, Ordering::Relaxed);
            }
            EVENT_PREEMPT_CHECK_RETURN_IRQ_OFF => {
                self.irq_off_preempt_check_returns
                    .fetch_add(1, Ordering::Relaxed);
            }
            EVENT_CONTEXT_SWITCH => {
                self.context_switches.fetch_add(1, Ordering::Relaxed);
            }
            EVENT_CONTEXT_SWITCH_RETURN => {
                self.context_switch_returns.fetch_add(1, Ordering::Relaxed);
            }
            EVENT_YIELD_ENTER_IRQ_OFF => {
                self.irq_off_yield_entries.fetch_add(1, Ordering::Relaxed);
            }
            EVENT_YIELD_RETURN_IRQ_OFF => {
                self.irq_off_yield_returns.fetch_add(1, Ordering::Relaxed);
            }
            EVENT_IDLE_AFTER_YIELD_IRQ_OFF => {
                self.irq_off_idle_boundaries.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }

        let sequence = self
            .next_sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if sequence == 0 {
            return;
        }
        self.events[sequence as usize % TRACE_DEPTH].publish(
            sequence,
            kind,
            task_id,
            peer_task_id,
            flags,
            preempt_disable_count,
        );
        self.published_sequence.store(sequence, Ordering::Release);
    }
}

static CPU_DIAGNOSTICS: [CpuDiagnostics; axconfig::plat::MAX_CPU_NUM] =
    [const { CpuDiagnostics::new() }; axconfig::plat::MAX_CPU_NUM];

fn current_cpu_diagnostics() -> Option<&'static CpuDiagnostics> {
    CPU_DIAGNOSTICS.get(axhal::percpu::this_cpu_id())
}

pub(crate) fn record_event(
    kind: u64,
    task_id: u64,
    peer_task_id: u64,
    flags: u64,
    preempt_disable_count: usize,
) {
    if let Some(diagnostics) = current_cpu_diagnostics() {
        diagnostics.record(kind, task_id, peer_task_id, flags, preempt_disable_count);
    }
}

pub(crate) fn record_timer_event() {
    if let Some(diagnostics) = current_cpu_diagnostics() {
        diagnostics.timer_events.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn irq_continuation_diagnostic_snapshot(
    cpu: usize,
) -> Option<IrqContinuationDiagnosticSnapshot> {
    let diagnostics = CPU_DIAGNOSTICS.get(cpu)?;
    Some(IrqContinuationDiagnosticSnapshot {
        latest_sequence: diagnostics.published_sequence.load(Ordering::Acquire),
        timer_events: diagnostics.timer_events.load(Ordering::Acquire),
        context_switches: diagnostics.context_switches.load(Ordering::Acquire),
        context_switch_returns: diagnostics.context_switch_returns.load(Ordering::Acquire),
        irq_off_preempt_disables: diagnostics.irq_off_preempt_disables.load(Ordering::Acquire),
        irq_off_preempt_enables: diagnostics.irq_off_preempt_enables.load(Ordering::Acquire),
        irq_off_outermost_preempt_enables: diagnostics
            .irq_off_outermost_preempt_enables
            .load(Ordering::Acquire),
        irq_off_preempt_enable_returns: diagnostics
            .irq_off_preempt_enable_returns
            .load(Ordering::Acquire),
        irq_off_preempt_checks: diagnostics.irq_off_preempt_checks.load(Ordering::Acquire),
        irq_off_preempt_check_returns: diagnostics
            .irq_off_preempt_check_returns
            .load(Ordering::Acquire),
        irq_off_yield_entries: diagnostics.irq_off_yield_entries.load(Ordering::Acquire),
        irq_off_yield_returns: diagnostics.irq_off_yield_returns.load(Ordering::Acquire),
        irq_off_idle_boundaries: diagnostics.irq_off_idle_boundaries.load(Ordering::Acquire),
    })
}

pub fn irq_continuation_diagnostic_event(
    cpu: usize,
    sequence: u64,
) -> Option<IrqContinuationDiagnosticEvent> {
    let diagnostics = CPU_DIAGNOSTICS.get(cpu)?;
    diagnostics.events[sequence as usize % TRACE_DEPTH].snapshot(sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_slot_rejects_overwritten_sequence() {
        let slot = EventSlot::new();
        slot.publish(7, 3, 11, 13, 5, 2);
        assert!(slot.snapshot(6).is_none());
        let event = slot
            .snapshot(7)
            .unwrap_or(IrqContinuationDiagnosticEvent::EMPTY);
        assert_eq!(event.sequence, 7);
        assert_eq!(event.kind, 3);
        assert_eq!(event.task_id, 11);
        assert_eq!(event.peer_task_id, 13);
        assert_eq!(event.flags, 5);
        assert_eq!(event.preempt_disable_count, 2);
    }
}
