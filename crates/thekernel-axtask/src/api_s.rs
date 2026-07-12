//! Task APIs for single-task configuration.

use axerrno::{AxError, AxResult};

/// For single-task situation, we just relax the CPU and wait for incoming
/// interrupts.
pub fn yield_now() {
    if cfg!(feature = "irq") {
        axhal::asm::wait_for_irqs();
    } else {
        core::hint::spin_loop();
    }
}

/// Returns whether the current single-task context may block.
pub fn can_block_current() -> bool {
    false
}

/// For single-task situation, we just busy wait for the given duration.
pub fn sleep(dur: core::time::Duration) -> AxResult<()> {
    let deadline = axhal::time::wall_time()
        .checked_add(dur)
        .ok_or(AxError::OutOfRange)?;
    sleep_until(deadline)
}

/// For single-task situation, we just busy wait until reaching the given
/// deadline.
pub fn sleep_until(deadline: axhal::time::TimeValue) -> AxResult<()> {
    axhal::time::busy_wait_until(deadline);
    Ok(())
}
