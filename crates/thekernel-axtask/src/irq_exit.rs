//! Explicit IRQ-return integration for preemptive scheduling.
//!
//! Layer 0 (`axhal`) owns the per-CPU interrupt nesting state and invokes the
//! single callback only after the outermost IRQ handler has released its
//! `NoPreempt` guard. This module keeps the scheduler policy at Layer 1: a
//! pending preemption is consumed at that boundary, while every ordinary
//! guard release inside an IRQ remains non-rescheduling.

use crate::{TaskInner, TaskRuntimeInitError};

/// Consumer-provided Layer 0 transport for the explicit IRQ-return boundary.
///
/// The generic scheduler deliberately does not name a concrete HAL fork. A
/// kernel enabling `irq-exit` must provide this interface from the same release
/// set that supplies its trap-entry and per-CPU IRQ nesting implementation.
#[doc(hidden)]
#[crate_interface::def_interface]
pub trait IrqExitIf {
    /// Claims the one outermost IRQ-exit callback.
    fn register_irq_exit_hook(hook: fn()) -> bool;

    /// Returns a migration-safe snapshot of whether the current CPU is inside
    /// a hardware IRQ handler.
    fn in_irq_context() -> bool;
}

/// Claims the one scheduler-owned IRQ-return callback.
pub(crate) fn register() -> Result<(), TaskRuntimeInitError> {
    if crate_interface::call_interface!(IrqExitIf::register_irq_exit_hook, on_irq_exit) {
        Ok(())
    } else {
        Err(TaskRuntimeInitError::IrqExitHookUnavailable)
    }
}

/// Returns whether the current CPU is in an IRQ handler.
#[inline]
pub(crate) fn in_irq_context() -> bool {
    crate_interface::call_interface!(IrqExitIf::in_irq_context)
}

/// Decides whether a pending-preemption check is legal in the current context.
///
/// Ordinary task safe points require enabled local interrupts. `axhal` invokes
/// the one explicit outermost-exit callback after nesting reaches zero but
/// before restoring interrupts; that callback is the sole IRQ-off exception.
#[inline]
pub(crate) const fn may_check_preempt(in_irq: bool, irqs_enabled: bool, at_irq_exit: bool) -> bool {
    !in_irq && (irqs_enabled || at_irq_exit)
}

fn on_irq_exit() {
    // `axhal` invokes this only for the 1 -> 0 IRQ nesting transition, after
    // its IRQ guard has dropped. `current_check_preempt_pending` remains the
    // single scheduler transition point and performs its own run-queue guard
    // and deferred-work handoff.
    if crate::current_may_uninit().is_some() {
        TaskInner::current_check_preempt_pending_at_irq_exit();
    }
}

#[cfg(test)]
struct TestIrqExitIf;

#[cfg(test)]
#[crate_interface::impl_interface]
impl IrqExitIf for TestIrqExitIf {
    fn register_irq_exit_hook(_hook: fn()) -> bool {
        true
    }

    fn in_irq_context() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pending_check_requires_a_task_or_explicit_irq_exit_safe_point() {
        assert!(super::may_check_preempt(false, true, false));
        assert!(!super::may_check_preempt(false, false, false));
        assert!(super::may_check_preempt(false, false, true));
        assert!(!super::may_check_preempt(true, false, true));
        assert!(!super::may_check_preempt(true, true, false));
    }
}
