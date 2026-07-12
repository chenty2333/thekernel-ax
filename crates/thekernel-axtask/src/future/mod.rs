//! Future support.

use alloc::sync::Arc;
use core::{
    fmt,
    future::poll_fn,
    mem::ManuallyDrop,
    pin::pin,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use axerrno::AxError;
use kernel_guard::NoPreemptIrqSave;

use crate::{
    AxTask, AxTaskRef, current, current_run_queue,
    run_queue::WakeTaskOutcome,
    select_run_queue,
    task::{BeginBlockWaitError, BlockWaitCommit, BlockWakeAction, EndBlockWaitError},
};

#[cfg(feature = "irq")]
mod poll;
#[cfg(feature = "irq")]
pub use poll::*;

mod time;
pub use time::*;

fn wake_task(task: &AxTaskRef) {
    match task.mark_block_woken() {
        BlockWakeAction::Unblock => {
            let mut rq = select_run_queue::<NoPreemptIrqSave>(task);
            let outcome = rq.unblock_task(task.clone(), true);
            drop(rq);
            if let WakeTaskOutcome::Rejected(error) = outcome {
                // The runqueue already restored a safe state and recorded a
                // durable TaskWakeFault. Consume the typed detail outside its
                // IRQ guard so this failure is neither silent nor dropped in
                // a critical section.
                error!(
                    "raw task waker contained enqueue failure for task {}: {:?}",
                    error.task.id().as_u64(),
                    error.kind
                );
            }
        }
        BlockWakeAction::BlockOwnerWillConsume | BlockWakeAction::Inactive => {}
    }
}

unsafe fn clone_task_waker(data: *const ()) -> RawWaker {
    // SAFETY: every RawWaker data pointer is created from `Arc<AxTask>` below,
    // and the source strong reference remains live for this clone operation.
    unsafe { Arc::<AxTask>::increment_strong_count(data.cast::<AxTask>()) };
    RawWaker::new(data, &TASK_WAKER_VTABLE)
}

unsafe fn wake_task_waker(data: *const ()) {
    // SAFETY: this callback consumes exactly the strong reference owned by the
    // RawWaker instance.
    let task = unsafe { Arc::<AxTask>::from_raw(data.cast::<AxTask>()) };
    wake_task(&task);
}

unsafe fn wake_task_waker_by_ref(data: *const ()) {
    // SAFETY: ManuallyDrop keeps the RawWaker's strong reference owned by the
    // caller while this borrowed callback runs.
    let task = ManuallyDrop::new(unsafe { Arc::<AxTask>::from_raw(data.cast::<AxTask>()) });
    wake_task(&task);
}

unsafe fn drop_task_waker(data: *const ()) {
    // SAFETY: this callback releases exactly the strong reference owned by the
    // RawWaker instance and never dereferences it afterwards.
    unsafe { Arc::<AxTask>::decrement_strong_count(data.cast::<AxTask>()) };
}

static TASK_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    clone_task_waker,
    wake_task_waker,
    wake_task_waker_by_ref,
    drop_task_waker,
);

fn task_waker(task: &AxTaskRef) -> Waker {
    let data = Arc::into_raw(task.clone()).cast::<()>();
    // SAFETY: `data` owns one Arc strong reference, and every vtable operation
    // preserves the RawWaker ownership rules documented above.
    unsafe { Waker::from_raw(RawWaker::new(data, &TASK_WAKER_VTABLE)) }
}

fn contain_block_state_loss(
    task: &AxTaskRef,
    token: crate::task::BlockWaitToken,
    stage: &'static str,
) -> BlockOnError {
    let cleanup = task.end_block_wait(token);
    task.record_wake_fault(crate::TaskWakeFault::SchedulerInvariant);
    error!(
        "task {} lost block ownership during {}: cleanup={:?}",
        task.id().as_u64(),
        stage,
        cleanup
    );
    BlockOnError::StateLost
}

/// Failure to drive a future synchronously on the current task.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockOnError {
    /// The current task already owns another synchronous block session.
    Busy,
    /// The per-task block-session generation space is exhausted.
    GenerationExhausted,
    /// The task/runqueue block transition lost its internal ownership state.
    StateLost,
}

impl fmt::Display for BlockOnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("the current task is already blocking"),
            Self::GenerationExhausted => {
                formatter.write_str("block-session generation space is exhausted")
            }
            Self::StateLost => formatter.write_str("block-session ownership state was lost"),
        }
    }
}

impl core::error::Error for BlockOnError {}

impl From<BlockOnError> for AxError {
    fn from(error: BlockOnError) -> Self {
        match error {
            BlockOnError::Busy => AxError::ResourceBusy,
            BlockOnError::GenerationExhausted => AxError::OutOfRange,
            BlockOnError::StateLost => AxError::BadState,
        }
    }
}

/// Blocks the current task until the given future is resolved.
///
/// Note that this doesn't handle interruption and is not recommended for direct
/// use in most cases.
pub fn block_on<F: IntoFuture>(f: F) -> Result<F::Output, BlockOnError> {
    let mut fut = pin!(f.into_future());

    let curr = current();
    // It's necessary to keep a strong reference to the current task
    // to prevent it from being dropped while blocking.
    let task = curr.clone();
    let token = task.begin_block_wait().map_err(|error| match error {
        BeginBlockWaitError::Busy => BlockOnError::Busy,
        BeginBlockWaitError::GenerationExhausted => BlockOnError::GenerationExhausted,
    })?;

    let waker = task_waker(&task);
    let mut cx = Context::from_waker(&waker);

    loop {
        if task.prepare_block_poll(token).is_err() {
            return Err(contain_block_state_loss(&task, token, "poll preparation"));
        }
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => {
                // A generic future may retain subsystem state across Pending,
                // so this is not a proven deferred-work safe point. Kernel
                // entry/exit, yield, idle, and syscall boundaries perform the
                // dispatcher wakeups instead.
                if task.is_block_woken(token) {
                    crate::yield_now();
                    continue;
                }
                let mut rq = current_run_queue::<NoPreemptIrqSave>();
                match rq.blocked_resched_atomic(token) {
                    BlockWaitCommit::Blocked => {}
                    BlockWaitCommit::Woken => {
                        drop(rq);
                        crate::yield_now();
                    }
                    BlockWaitCommit::Stale => {
                        drop(rq);
                        return Err(contain_block_state_loss(
                            &task,
                            token,
                            "blocked-state commit",
                        ));
                    }
                }
            }
            Poll::Ready(output) => {
                task.end_block_wait(token).map_err(|error| match error {
                    EndBlockWaitError::Stale | EndBlockWaitError::TransitionInProgress => {
                        BlockOnError::StateLost
                    }
                })?;
                return Ok(output);
            }
        }
    }
}

/// Error returned by [`interruptible`].
#[derive(Debug, PartialEq, Eq)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "interrupted")
    }
}

impl core::error::Error for Interrupted {}

impl From<Interrupted> for AxError {
    fn from(_: Interrupted) -> Self {
        AxError::Interrupted
    }
}

/// Makes a future interruptible while giving completed work priority.
///
/// The wrapped future is polled before the task interrupt is consumed. After
/// installing the interrupt waker, it is polled once more to close the race
/// between the operation becoming ready and interruption. If both become
/// ready in that window, the operation wins and the interrupt is restored for
/// the caller's next interruption boundary.
pub async fn interruptible<F: IntoFuture>(f: F) -> Result<F::Output, Interrupted> {
    let mut f = pin!(f.into_future());
    let curr = current();
    poll_fn(|cx| {
        if let Poll::Ready(output) = f.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }

        let interrupted = curr.poll_interrupt(cx).is_ready();
        if let Poll::Ready(output) = f.as_mut().poll(cx) {
            if interrupted {
                curr.interrupt();
            }
            return Poll::Ready(Ok(output));
        }
        if interrupted {
            Poll::Ready(Err(Interrupted))
        } else {
            Poll::Pending
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_task_waker_owns_exactly_one_arc_per_instance() {
        let task = crate::TaskInner::new_init("raw-waker".into())
            .unwrap()
            .into_arc()
            .unwrap();
        let baseline = Arc::strong_count(&task);

        let waker = task_waker(&task);
        assert_eq!(Arc::strong_count(&task), baseline + 1);
        let clone = waker.clone();
        assert_eq!(Arc::strong_count(&task), baseline + 2);
        clone.wake_by_ref();
        assert_eq!(Arc::strong_count(&task), baseline + 2);
        clone.wake();
        assert_eq!(Arc::strong_count(&task), baseline + 1);
        drop(waker);
        assert_eq!(Arc::strong_count(&task), baseline);
    }
}
