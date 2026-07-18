//! Explicit IRQ-return integration for preemptive scheduling.
//!
//! Layer 0 (`axhal`) owns the per-CPU interrupt nesting state and invokes the
//! single callback only after the outermost IRQ handler has released its
//! `NoPreempt` guard. This module keeps the scheduler policy at Layer 1: a
//! pending preemption is consumed at that boundary, while every ordinary
//! guard release inside an IRQ remains non-rescheduling.

use crate::{TaskInner, TaskRuntimeInitError};

/// Claims the one scheduler-owned IRQ-return callback.
pub(crate) fn register() -> Result<(), TaskRuntimeInitError> {
    if axhal::irq::register_irq_exit_hook(on_irq_exit) {
        Ok(())
    } else {
        Err(TaskRuntimeInitError::IrqExitHookUnavailable)
    }
}

/// Returns whether the current CPU is in an IRQ handler.
#[inline]
pub(crate) fn in_irq_context() -> bool {
    axhal::irq::in_irq_context()
}

/// Returns whether the scheduler is currently consuming the outermost IRQ
/// exit callback. Internal preemption checks may run in this phase, while
/// ordinary IRQ-context checks must still defer scheduling.
#[inline]
pub(crate) fn in_irq_exit_phase() -> bool {
    axhal::irq::in_irq_exit_phase()
}

/// Decides whether a pending-preemption check is legal at the current phase.
///
/// Normal IRQ handlers defer the check. The one outermost exit callback is the
/// sole exception: it is the handoff point after the IRQ guard has dropped.
#[inline]
pub(crate) const fn may_check_preempt(in_irq: bool, in_exit_phase: bool) -> bool {
    !in_irq || in_exit_phase
}

fn on_irq_exit() {
    // `axhal` invokes this only for the 1 -> 0 IRQ nesting transition, after
    // its IRQ guard has dropped. `current_check_preempt_pending` remains the
    // single scheduler transition point and performs its own run-queue guard
    // and deferred-work handoff.
    if crate::current_may_uninit().is_some() {
        TaskInner::current_check_preempt_pending();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pending_check_is_only_allowed_at_irq_exit_or_task_context() {
        assert!(super::may_check_preempt(false, false));
        assert!(!super::may_check_preempt(true, false));
        assert!(super::may_check_preempt(true, true));
        assert!(super::may_check_preempt(false, true));
    }
}
