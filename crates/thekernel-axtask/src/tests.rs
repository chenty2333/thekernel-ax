use core::{
    future::{pending, poll_fn},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};
use std::sync::{Mutex, Once};

use axerrno::AxError;

#[cfg(feature = "task-ext")]
use crate::TaskInner;
use crate::{WaitQueue, api as axtask, current};

static INIT: Once = Once::new();
static DEFERRED_INIT: Once = Once::new();
static SERIAL: Mutex<()> = Mutex::new(());
static DEFERRED_CALLS: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_REENTER: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "task-ext")]
const GC_DROP_OK: usize = 1;
#[cfg(feature = "task-ext")]
const GC_DROP_NESTED_BUSY: usize = 2;
#[cfg(feature = "task-ext")]
const GC_DROP_OTHER_ERROR: usize = 3;

#[cfg(feature = "task-ext")]
struct GcDropProbe(alloc::sync::Arc<AtomicUsize>);

#[cfg(feature = "task-ext")]
#[extern_trait::extern_trait]
impl crate::TaskExt for GcDropProbe {}

#[cfg(feature = "task-ext")]
impl Drop for GcDropProbe {
    fn drop(&mut self) {
        let result = crate::future::block_on(async {});
        let outcome = match result {
            Ok(()) => GC_DROP_OK,
            Err(crate::future::BlockOnError::Busy) => GC_DROP_NESTED_BUSY,
            Err(_) => GC_DROP_OTHER_ERROR,
        };
        self.0.store(outcome, Ordering::Release);
    }
}

fn init_for_test() {
    INIT.call_once(|| axtask::init_scheduler().unwrap());
}

fn test_deferred_dispatcher() {
    assert!(axtask::can_block_current());
    DEFERRED_CALLS.fetch_add(1, Ordering::Release);
    if DEFERRED_REENTER.swap(false, Ordering::AcqRel) {
        axtask::run_deferred_work();
    }
}

#[test]
fn current_handle_owns_an_independent_strong_reference() {
    let _lock = SERIAL.lock();
    init_for_test();

    let raw: *const crate::AxTask = axhal::percpu::current_task_ptr();
    assert!(!raw.is_null());

    // SAFETY: the initialized per-CPU current-task slot keeps `raw` live. Each
    // temporary probe below creates and releases exactly one additional strong
    // reference without consuming the slot's ownership.
    unsafe { alloc::sync::Arc::increment_strong_count(raw) };
    let probe = unsafe { crate::AxTaskRef::from_raw(raw) };
    let baseline_with_probe = alloc::sync::Arc::strong_count(&probe);
    drop(probe);

    let handle = current();
    unsafe { alloc::sync::Arc::increment_strong_count(raw) };
    let probe = unsafe { crate::AxTaskRef::from_raw(raw) };
    assert_eq!(
        alloc::sync::Arc::strong_count(&probe),
        baseline_with_probe + 1
    );
    drop(probe);
    drop(handle);
}

#[test]
fn interruptible_ready_result_preserves_a_simultaneous_interrupt() {
    let _lock = SERIAL.lock();
    init_for_test();
    let curr = current();
    curr.clear_interrupt();
    curr.interrupt();

    let result = crate::future::block_on(crate::future::interruptible(async { 7_u32 })).unwrap();

    assert_eq!(result, Ok(7));
    assert!(curr.is_interrupted());
    curr.clear_interrupt();
}

#[test]
fn interruptible_second_check_restores_a_consumed_interrupt() {
    let _lock = SERIAL.lock();
    init_for_test();
    let curr = current();
    curr.clear_interrupt();
    curr.interrupt();
    let polls = AtomicUsize::new(0);

    let result = crate::future::block_on(crate::future::interruptible(poll_fn(|_| {
        if polls.fetch_add(1, Ordering::AcqRel) == 0 {
            Poll::Pending
        } else {
            Poll::Ready(11_u32)
        }
    })))
    .unwrap();

    assert_eq!(result, Ok(11));
    assert_eq!(polls.load(Ordering::Acquire), 2);
    assert!(curr.is_interrupted());
    curr.clear_interrupt();
}

#[test]
fn interruptible_pending_result_consumes_the_interrupt() {
    let _lock = SERIAL.lock();
    init_for_test();
    let curr = current();
    curr.clear_interrupt();
    curr.interrupt();

    let result = crate::future::block_on(crate::future::interruptible(pending::<()>())).unwrap();

    assert_eq!(result, Err(crate::future::Interrupted));
    assert!(!curr.is_interrupted());
}

#[test]
fn block_on_consumes_a_self_wake_without_losing_the_session() {
    let _lock = SERIAL.lock();
    init_for_test();
    let polls = AtomicUsize::new(0);

    let result = crate::future::block_on(poll_fn(|cx| {
        if polls.fetch_add(1, Ordering::AcqRel) == 0 {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(23_u32)
        }
    }))
    .unwrap();

    assert_eq!(result, 23);
    assert_eq!(polls.load(Ordering::Acquire), 2);
    assert_eq!(current().wake_fault(), None);
}

#[test]
fn block_on_rejects_pending_work_while_preemption_is_disabled() {
    let _lock = SERIAL.lock();
    init_for_test();
    let curr = current();
    let polls = AtomicUsize::new(0);

    curr.disable_preempt();
    let result = crate::future::block_on(poll_fn(|_| {
        polls.fetch_add(1, Ordering::AcqRel);
        Poll::<()>::Pending
    }));
    curr.enable_preempt(false);

    assert_eq!(result, Err(crate::future::BlockOnError::CannotBlock));
    assert_eq!(polls.load(Ordering::Acquire), 1);
    assert_eq!(curr.wake_fault(), None);
}

fn other_deferred_dispatcher() {}

#[test]
fn deferred_work_runs_at_yield_and_suppresses_same_task_recursion() {
    let _lock = SERIAL.lock();
    init_for_test();
    DEFERRED_INIT.call_once(|| {
        assert!(axtask::set_deferred_work_dispatcher(
            test_deferred_dispatcher
        ));
    });
    assert!(!axtask::set_deferred_work_dispatcher(
        other_deferred_dispatcher
    ));

    DEFERRED_CALLS.store(0, Ordering::Release);
    DEFERRED_REENTER.store(true, Ordering::Release);
    axtask::run_deferred_work();
    assert_eq!(DEFERRED_CALLS.load(Ordering::Acquire), 1);

    DEFERRED_CALLS.store(0, Ordering::Release);
    axtask::yield_now();
    assert!(DEFERRED_CALLS.load(Ordering::Acquire) >= 2);
}

#[test]
fn test_sched_fifo() {
    let _lock = SERIAL.lock();
    init_for_test();

    const NUM_TASKS: usize = 10;
    static FINISHED_TASKS: AtomicUsize = AtomicUsize::new(0);

    for i in 0..NUM_TASKS {
        axtask::spawn_raw(
            move || {
                println!(
                    "sched-fifo: Hello, task {}! ({})",
                    i,
                    current().id_name().unwrap()
                );
                axtask::yield_now();
                let order = FINISHED_TASKS.fetch_add(1, Ordering::Release);
                assert_eq!(order, i); // FIFO scheduler
            },
            format!("T{i}"),
            crate::MIN_KERNEL_STACK_SIZE,
        )
        .unwrap();
    }

    while FINISHED_TASKS.load(Ordering::Acquire) < NUM_TASKS {
        axtask::yield_now();
    }
}

#[test]
fn test_fp_state_switch() {
    let _lock = SERIAL.lock();
    init_for_test();

    const NUM_TASKS: usize = 5;
    const FLOATS: [f64; NUM_TASKS] = [
        std::f64::consts::PI,
        std::f64::consts::E,
        -std::f64::consts::SQRT_2,
        0.0,
        0.618033988749895,
    ];
    static FINISHED_TASKS: AtomicUsize = AtomicUsize::new(0);

    for (i, float) in FLOATS.iter().enumerate() {
        axtask::spawn(move || {
            let mut value = float + i as f64;
            axtask::yield_now();
            value -= i as f64;

            println!("fp_state_switch: Float {i} = {value}");
            assert!((value - float).abs() < 1e-9);
            FINISHED_TASKS.fetch_add(1, Ordering::Release);
        })
        .unwrap();
    }
    while FINISHED_TASKS.load(Ordering::Acquire) < NUM_TASKS {
        axtask::yield_now();
    }
}

#[test]
fn test_wait_queue() {
    let _lock = SERIAL.lock();
    init_for_test();

    const NUM_TASKS: usize = 10;

    static WQ1: WaitQueue = WaitQueue::new();
    static WQ2: WaitQueue = WaitQueue::new();
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    for _ in 0..NUM_TASKS {
        axtask::spawn(move || {
            COUNTER.fetch_add(1, Ordering::Release);
            println!("wait_queue: task {:?} started", current().id());
            WQ1.notify_one(true); // WQ1.wait_until()
            WQ2.wait().unwrap();

            COUNTER.fetch_sub(1, Ordering::Release);
            println!("wait_queue: task {:?} finished", current().id());
            WQ1.notify_one(true); // WQ1.wait_until()
        })
        .unwrap();
    }

    println!("task {:?} is waiting for tasks to start...", current().id());
    WQ1.wait_until(|| COUNTER.load(Ordering::Acquire) == NUM_TASKS)
        .unwrap();
    axtask::yield_now();
    assert_eq!(COUNTER.load(Ordering::Acquire), NUM_TASKS);
    WQ2.notify_all(true); // WQ2.wait()

    println!(
        "task {:?} is waiting for tasks to finish...",
        current().id()
    );
    WQ1.wait_until(|| COUNTER.load(Ordering::Acquire) == 0)
        .unwrap();
    assert_eq!(COUNTER.load(Ordering::Acquire), 0);
}

#[test]
fn interruptible_timed_wait_rechecks_condition_after_listener_publication() {
    let _lock = SERIAL.lock();
    init_for_test();

    let wait_queue = WaitQueue::new();
    let mut checks = 0;
    let timed_out = wait_queue
        .wait_timeout_until_interruptible(Duration::from_secs(1), || {
            checks += 1;
            checks >= 2
        })
        .unwrap();

    assert!(!timed_out);
    assert_eq!(checks, 2);
}

#[test]
fn interruptible_timed_wait_condition_notification_cancels_timer() {
    let _lock = SERIAL.lock();
    init_for_test();

    static WAIT: WaitQueue = WaitQueue::new();
    static READY: AtomicBool = AtomicBool::new(false);
    READY.store(false, Ordering::Release);
    let timer_count = crate::future::timer_future_count_for_test();
    let notifier = axtask::spawn(|| {
        READY.store(true, Ordering::Release);
        WAIT.notify_one(false);
    })
    .unwrap();

    assert!(
        !WAIT
            .wait_timeout_until_interruptible(Duration::from_secs(1), || {
                READY.load(Ordering::Acquire)
            })
            .unwrap()
    );
    notifier.join().unwrap();
    assert_eq!(crate::future::timer_future_count_for_test(), timer_count);
}

#[test]
fn interruptible_timed_wait_satisfied_condition_needs_no_timer_or_interrupt() {
    let _lock = SERIAL.lock();
    init_for_test();

    let curr = current();
    curr.interrupt();
    let timer_count = crate::future::timer_future_count_for_test();
    assert_eq!(
        WaitQueue::new().wait_timeout_until_interruptible(Duration::MAX, || true),
        Ok(false)
    );
    assert!(curr.is_interrupted());
    assert_eq!(crate::future::timer_future_count_for_test(), timer_count);
    curr.clear_interrupt();
}

#[test]
fn interruptible_timed_wait_reports_capacity_and_releases_all_test_timers() {
    let _lock = SERIAL.lock();
    init_for_test();

    assert_eq!(crate::future::timer_future_count_for_test(), 0);
    let deadline = axhal::time::wall_time()
        .checked_add(Duration::from_secs(3600))
        .unwrap();
    let mut timers = Vec::with_capacity(crate::future::TIMER_FUTURE_CAPACITY);
    for _ in 0..crate::future::TIMER_FUTURE_CAPACITY {
        timers.push(
            crate::future::reserve_timer_for_test(deadline)
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(
        crate::future::timer_future_count_for_test(),
        crate::future::TIMER_FUTURE_CAPACITY
    );
    assert_eq!(
        WaitQueue::new().wait_timeout_until_interruptible(Duration::from_secs(1), || false),
        Err(crate::WaitError::Timer(
            crate::future::TimerRegistrationError::CapacityExhausted
        ))
    );

    drop(timers);
    assert_eq!(crate::future::timer_future_count_for_test(), 0);
}

#[test]
fn interruptible_timed_wait_reports_interrupt_without_slice_polling() {
    let _lock = SERIAL.lock();
    init_for_test();

    static WAIT: WaitQueue = WaitQueue::new();
    let waiter = current().clone();
    let interrupter = axtask::spawn(move || waiter.interrupt()).unwrap();

    assert_eq!(
        WAIT.wait_timeout_until_interruptible(Duration::from_secs(1), || false),
        Err(crate::WaitError::Interrupted)
    );
    interrupter.join().unwrap();
}

#[test]
fn interruptible_timed_wait_reports_one_complete_deadline() {
    let _lock = SERIAL.lock();
    init_for_test();

    assert!(
        WaitQueue::new()
            .wait_timeout_until_interruptible(Duration::ZERO, || false)
            .unwrap()
    );
}

#[test]
fn test_task_join() {
    let _lock = SERIAL.lock();
    init_for_test();

    const NUM_TASKS: usize = 10;
    let mut tasks = Vec::with_capacity(NUM_TASKS);

    for i in 0..NUM_TASKS {
        tasks.push(
            axtask::spawn_raw(
                move || {
                    println!("task_join: task {}! ({})", i, current().id_name().unwrap());
                    axtask::yield_now();
                    axtask::exit(i as _);
                },
                format!("T{i}"),
                crate::MIN_KERNEL_STACK_SIZE,
            )
            .unwrap(),
        );
    }

    for (i, task) in tasks.into_iter().enumerate() {
        assert_eq!(task.join().unwrap(), i as _);
    }
    let returned = axtask::spawn(|| {}).unwrap();
    assert_eq!(returned.join().unwrap(), 0);
    drop(returned);
    assert!(!axtask::reclaim_exited_tasks_until_clear(128));
}

#[test]
fn direct_exit_releases_internal_current_task_owners() {
    let _lock = SERIAL.lock();
    init_for_test();

    let task = axtask::spawn(|| axtask::exit(37)).unwrap();
    assert_eq!(task.join().unwrap(), 37);
    drop(task);
    assert!(!axtask::reclaim_exited_tasks_until_clear(128));
}

#[cfg(feature = "task-ext")]
#[test]
fn gc_runs_task_ext_destructors_outside_its_block_session() {
    let _lock = SERIAL.lock();
    init_for_test();

    let outcome = alloc::sync::Arc::new(AtomicUsize::new(0));
    let mut inner = crate::TaskInner::new(
        || {},
        "gc-drop-boundary".into(),
        crate::MIN_KERNEL_STACK_SIZE,
    )
    .unwrap();
    *inner.task_ext_mut() = Some(crate::AxTaskExt::from_impl(GcDropProbe(outcome.clone())));

    let task = axtask::spawn_task(inner).unwrap();
    assert_eq!(task.join().unwrap(), 0);
    drop(task);

    // A retained exit is deliberately requeued without self-waking. Publish a
    // fresh exit after releasing the external handle so the dedicated GC task
    // gets a deterministic new edge and revisits the queue.
    let kick = axtask::spawn(|| {}).unwrap();
    assert_eq!(kick.join().unwrap(), 0);
    drop(kick);

    for _ in 0..256 {
        if outcome.load(Ordering::Acquire) != 0 {
            break;
        }
        axtask::yield_now();
    }

    assert_eq!(outcome.load(Ordering::Acquire), GC_DROP_OK);
    assert!(!axtask::reclaim_exited_tasks_until_clear(128));
}

#[test]
fn task_join_closes_exit_before_waker_registration_race() {
    let task = crate::TaskInner::new_init("join-race".into()).unwrap();
    let mut context = Context::from_waker(Waker::noop());

    // Force exactly the lost-wake ordering: the first state check observes a
    // live task, then exit publishes and wakes before AtomicWaker::register.
    // No later wake exists, so only the post-registration state check can make
    // this poll complete.
    assert_ne!(task.state(), crate::TaskState::Exited);
    task.notify_exit(73);
    let result = task.register_join_waiter_and_recheck_for_test(&mut context);

    assert_eq!(result, Poll::Ready(73));
}

#[test]
fn reclaim_driver_yields_until_exited_queue_drains() {
    let mut reclaim_calls = 0;
    let mut yield_calls = 0;

    let remains = axtask::drive_reclaim_until_clear(
        8,
        || {
            reclaim_calls += 1;
            reclaim_calls < 4
        },
        || {
            yield_calls += 1;
        },
    );

    assert_eq!(reclaim_calls, 4);
    assert_eq!(yield_calls, 3);
    assert!(!remains);
}

#[test]
fn reclaim_driver_is_bounded_when_scheduler_refs_remain() {
    let mut reclaim_calls = 0;
    let mut yield_calls = 0;

    let remains = axtask::drive_reclaim_until_clear(
        8,
        || {
            reclaim_calls += 1;
            true
        },
        || {
            yield_calls += 1;
        },
    );

    assert_eq!(reclaim_calls, 9);
    assert_eq!(yield_calls, 8);
    assert!(remains);
}

#[test]
fn fallible_task_construction_rejects_invalid_stack_sizes() {
    assert!(crate::TaskInner::try_new(|| {}, String::new(), 0).is_err());
    assert!(crate::TaskInner::try_new(|| {}, String::new(), 0x1000).is_err());
    assert!(crate::TaskInner::try_new(|| {}, String::new(), usize::MAX).is_err());
}

#[cfg(feature = "sched-cfs")]
#[test]
fn sched_state_update_preserves_typed_failure_causes() {
    let _lock = SERIAL.lock();
    init_for_test();

    let task = crate::TaskInner::new_init("sched-update".into())
        .unwrap()
        .into_arc()
        .unwrap();
    let invalid = crate::SchedState {
        nice: 20,
        ..Default::default()
    };
    assert_eq!(
        axtask::set_sched_state(&task, invalid),
        Err(crate::TaskSchedError::Scheduler(
            axsched::SchedulerError::InvalidParameters
        ))
    );

    let current = current().clone();
    let original = axtask::sched_state(&current);
    assert_eq!(
        axtask::set_priority(20),
        Err(crate::TaskSchedError::Scheduler(
            axsched::SchedulerError::InvalidParameters
        ))
    );
    axtask::set_priority(5).unwrap();
    assert_eq!(axtask::sched_state(&current).nice, 5);
    axtask::set_sched_state(&current, original).unwrap();

    task.notify_exit(0);
    assert_eq!(
        axtask::set_sched_state(&task, Default::default()),
        Err(crate::TaskSchedError::TaskExited)
    );
}

#[cfg(feature = "sched-cfs")]
#[test]
fn failed_publication_reservation_returns_the_exact_task_owner() {
    let _lock = SERIAL.lock();
    init_for_test();

    let task = crate::TaskInner::new_init("publish-owner".into())
        .unwrap()
        .into_arc()
        .unwrap();
    let id = task.id();
    task.notify_exit(0);

    let error = axtask::reserve_prepared_task(task).unwrap_err();
    assert_eq!(error.kind(), crate::TaskEnqueueErrorKind::TaskNotReady);
    let returned = error.into_task();
    assert_eq!(returned.id(), id);
    assert_eq!(returned.state(), crate::TaskState::Exited);
    assert_eq!(
        axtask::set_task_affinity(&returned, returned.cpumask()),
        Err(AxError::NoSuchProcess)
    );
}

#[cfg(feature = "sched-cfs")]
#[test]
fn publication_reservation_cancel_returns_the_unpublished_task() {
    let _lock = SERIAL.lock();
    init_for_test();

    let task = crate::TaskInner::new_init("reserved-owner".into())
        .unwrap()
        .into_arc()
        .unwrap();
    let id = task.id();
    let reservation = axtask::reserve_prepared_task(task).unwrap();
    assert_eq!(reservation.task().id(), id);
    let reserved = reservation.task().clone();
    let original_affinity = reserved.cpumask();
    assert_eq!(
        axtask::set_task_affinity(&reserved, original_affinity),
        Err(AxError::ResourceBusy)
    );
    assert_eq!(reserved.cpumask(), original_affinity);
    assert_eq!(
        axtask::set_sched_state(&reserved, Default::default()),
        Err(crate::TaskSchedError::Scheduler(
            axsched::SchedulerError::TaskBusy
        ))
    );
    drop(reserved);

    let task = reservation.cancel();
    assert_eq!(task.id(), id);
    assert_eq!(task.state(), crate::TaskState::Ready);
    axtask::set_task_affinity(&task, original_affinity).unwrap();
    axtask::set_sched_state(&task, Default::default()).unwrap();
}

#[cfg(feature = "sched-cfs")]
#[test]
fn publication_reservation_commits_and_auto_exit_is_reclaimable() {
    let _lock = SERIAL.lock();
    init_for_test();

    static RAN: AtomicBool = AtomicBool::new(false);
    RAN.store(false, Ordering::Release);
    let parent = current().clone();
    let prepared = axtask::prepare_task_with_sched_from(
        crate::TaskInner::new(
            || RAN.store(true, Ordering::Release),
            "reserved-publish".into(),
            crate::MIN_KERNEL_STACK_SIZE,
        )
        .unwrap(),
        Default::default(),
        &parent,
    )
    .unwrap();
    let id = prepared.id();

    let reservation = axtask::reserve_prepared_task(prepared).unwrap();
    let published = axtask::publish_prepared_task(reservation);
    assert_eq!(published.id(), id);
    assert_eq!(published.join().unwrap(), 0);
    assert!(RAN.load(Ordering::Acquire));
    drop(published);
    assert!(!axtask::reclaim_exited_tasks_until_clear(128));
}

#[cfg(not(feature = "sched-cfs"))]
#[test]
fn priority_update_is_honestly_unsupported_by_fifo_and_rr() {
    assert_eq!(
        axtask::set_priority(0),
        Err(crate::TaskSchedError::Unsupported)
    );
}

#[test]
fn current_affinity_admission_failure_does_not_publish() {
    let mut published = false;
    let result = axtask::admit_affinity_then_publish(
        true,
        || Err::<usize, AxError>(AxError::NoMemory),
        |_| published = true,
    );

    assert_eq!(result, Err(AxError::NoMemory));
    assert!(!published);
}

#[test]
fn remote_running_affinity_admission_failure_does_not_publish() {
    let mut published_mask = None;
    let requested_mask = 0b10usize;
    let result = axtask::admit_affinity_then_publish(
        true,
        || Err::<usize, AxError>(AxError::NoMemory),
        |_| published_mask = Some(requested_mask),
    );

    assert_eq!(result, Err(AxError::NoMemory));
    assert_eq!(published_mask, None);
}

#[test]
fn later_affinity_publication_returns_replaced_helper_for_out_of_lock_drop() {
    struct Helper<'a>(&'a AtomicUsize, usize);

    impl Drop for Helper<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    let drops = AtomicUsize::new(0);
    let mut pending = Some(Helper(&drops, 1));
    let displaced = axtask::admit_affinity_then_publish(
        true,
        || Ok::<_, ()>(Helper(&drops, 2)),
        |replacement| core::mem::replace(&mut pending, replacement),
    )
    .unwrap();

    assert_eq!(drops.load(Ordering::Acquire), 0);
    assert_eq!(displaced.as_ref().map(|helper| helper.1), Some(1));
    assert_eq!(pending.as_ref().map(|helper| helper.1), Some(2));
    drop(displaced);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    drop(pending);
    assert_eq!(drops.load(Ordering::Acquire), 2);
}
