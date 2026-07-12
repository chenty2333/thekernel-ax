macro_rules! def_test_sched {
    ($name:ident, $scheduler:ty, $task:ty) => {
        mod $name {
            use alloc::sync::Arc;

            use crate::*;

            #[test]
            fn test_sched() {
                const NUM_TASKS: usize = 11;

                let mut scheduler = <$scheduler>::new();
                for i in 0..NUM_TASKS {
                    scheduler.add_task(Arc::new(<$task>::new(i)));
                }

                for i in 0..NUM_TASKS * 10 - 1 {
                    let next = scheduler.pick_next_task().unwrap();
                    assert_eq!(*next.inner(), i % NUM_TASKS);
                    // pass a tick to ensure the order of tasks
                    scheduler.task_tick(&next);
                    scheduler.put_prev_task(next, false);
                }

                let mut n = 0;
                while scheduler.pick_next_task().is_some() {
                    n += 1;
                }
                assert_eq!(n, NUM_TASKS);
            }

            #[test]
            fn bench_yield() {
                const NUM_TASKS: usize = 1_000_000;
                const COUNT: usize = NUM_TASKS * 3;

                let mut scheduler = <$scheduler>::new();
                for i in 0..NUM_TASKS {
                    scheduler.add_task(Arc::new(<$task>::new(i)));
                }

                let t0 = std::time::Instant::now();
                for _ in 0..COUNT {
                    let next = scheduler.pick_next_task().unwrap();
                    scheduler.put_prev_task(next, false);
                }
                let t1 = std::time::Instant::now();
                println!(
                    "  {}: task yield speed: {:?}/task",
                    stringify!($scheduler),
                    (t1 - t0) / (COUNT as u32)
                );
            }

            #[test]
            fn bench_remove() {
                const NUM_TASKS: usize = 10_000;

                let mut scheduler = <$scheduler>::new();
                let mut tasks = Vec::new();
                for i in 0..NUM_TASKS {
                    let t = Arc::new(<$task>::new(i));
                    tasks.push(t.clone());
                    scheduler.add_task(t);
                }

                let t0 = std::time::Instant::now();
                for i in (0..NUM_TASKS).rev() {
                    let t = scheduler.remove_task(&tasks[i]).unwrap();
                    assert_eq!(*t.inner(), i);
                }
                let t1 = std::time::Instant::now();
                println!(
                    "  {}: task remove speed: {:?}/task",
                    stringify!($scheduler),
                    (t1 - t0) / (NUM_TASKS as u32)
                );
            }
        }
    };
}

def_test_sched!(fifo, FifoScheduler::<usize>, FifoTask::<usize>);
def_test_sched!(rr, RRScheduler::<usize, 5>, RRTask::<usize, 5>);
def_test_sched!(cfs, CFScheduler::<usize>, CFSTask::<usize>);

mod cfs_rt {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn rt_tasks_preempt_fair_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        assert!(rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 10,
            reset_on_fork: false,
        }));
        scheduler.add_task(fair.clone());
        scheduler.add_task(rt.clone());

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 2);
        scheduler.put_prev_task(first, false);

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(*second.inner(), 2);
    }

    #[test]
    fn ready_rt_task_preempts_running_fair_task() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        assert!(rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 50,
            reset_on_fork: false,
        }));

        scheduler.add_task(fair.clone());
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);
        scheduler.enqueue_task(rt, EnqueueReason::Wakeup);

        assert!(scheduler.task_tick(&running));
    }

    #[test]
    fn higher_rt_priority_runs_first() {
        let mut scheduler = CFScheduler::<usize>::new();
        let low = Arc::new(CFSTask::new(1));
        let high = Arc::new(CFSTask::new(2));
        assert!(low.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 10,
            reset_on_fork: false,
        }));
        assert!(high.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 20,
            reset_on_fork: false,
        }));
        scheduler.add_task(low);
        scheduler.add_task(high);

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 2);
    }

    #[test]
    fn rr_rotates_between_equal_priority_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let a = Arc::new(CFSTask::new(1));
        let b = Arc::new(CFSTask::new(2));
        for task in [&a, &b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::RoundRobin,
                nice: 0,
                rt_priority: 42,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&first), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(first, false);

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(*second.inner(), 2);
    }

    #[test]
    fn rr_timer_preemption_rotates_between_equal_priority_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let a = Arc::new(CFSTask::new(1));
        let b = Arc::new(CFSTask::new(2));
        for task in [&a, &b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::RoundRobin,
                nice: 0,
                rt_priority: 42,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&first), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(first, true);

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *second.inner(),
            2,
            "timer-driven RR preemption must rotate an expired task",
        );
    }

    #[test]
    fn fifo_same_priority_peers_do_not_time_slice() {
        let mut scheduler = CFScheduler::<usize>::new();
        let a = Arc::new(CFSTask::new(1));
        let b = Arc::new(CFSTask::new(2));
        for task in [&a, &b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: 99,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(
                !scheduler.task_tick(&first),
                "SCHED_FIFO must not rotate same-priority peers on timer ticks",
            );
        }
        scheduler.put_prev_task(first, true);

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            1,
            "a preempted SCHED_FIFO task keeps precedence over same-priority peers",
        );
    }

    #[test]
    fn fifo_rt_keeps_precedence_over_fair_task() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        assert!(rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 99,
            reset_on_fork: false,
        }));
        scheduler.add_task(fair);
        scheduler.add_task(rt);

        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 2);
        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(
                !scheduler.task_tick(&running),
                "SCHED_FIFO must not be time-slice preempted for fair work",
            );
        }
        scheduler.put_prev_task(running, true);

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            2,
            "runnable RT work must stay ahead of fair tasks in normal mixed-class operation",
        );
    }

    #[test]
    fn fifo_rt_peers_do_not_yield_to_fair_control_task() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt_a = Arc::new(CFSTask::new(2));
        let rt_b = Arc::new(CFSTask::new(3));
        for task in [&rt_a, &rt_b] {
            assert!(task.configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: 99,
                reset_on_fork: false,
            }));
            scheduler.add_task(task.clone());
        }
        scheduler.add_task(fair);

        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 2);
        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(!scheduler.task_tick(&running));
        }
        scheduler.put_prev_task(running, true);

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            2,
            "same-priority SCHED_FIFO peers and fair tasks wait until the running FIFO task \
             blocks, yields, exits, or is preempted by higher priority RT",
        );
    }

    #[test]
    fn fair_task_is_preempted_while_rt_is_ready() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        assert!(rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 99,
            reset_on_fork: false,
        }));
        scheduler.add_task(rt);

        assert!(
            scheduler.task_tick(&fair),
            "fair task should request reschedule whenever RT work is ready",
        );
        scheduler.put_prev_task(fair, true);

        let next_rt = scheduler.pick_next_task().unwrap();
        assert_eq!(*next_rt.inner(), 2);
    }
}

mod cfs_fork {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn forked_fair_task_does_not_immediately_preempt_parent() {
        let mut scheduler = CFScheduler::<usize>::new();
        let parent = Arc::new(CFSTask::new(1));

        scheduler.add_task(parent.clone());
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(!scheduler.task_tick(&running));
        }

        let child = Arc::new(CFSTask::new(2));
        child.inherit_fair_vruntime_from(&running);
        scheduler.add_task(child);

        assert!(
            !scheduler.task_tick(&running),
            "forked child should inherit the parent's vruntime instead of cutting to the floor",
        );
    }

    #[test]
    fn yielding_parent_lets_forked_child_run() {
        let mut scheduler = CFScheduler::<usize>::new();
        let parent = Arc::new(CFSTask::new(1));

        scheduler.add_task(parent.clone());
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        let child = Arc::new(CFSTask::new(2));
        child.inherit_fair_vruntime_from(&running);
        scheduler.add_task(child);

        scheduler.enqueue_task(running, EnqueueReason::Yield);

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            2,
            "a yielding parent should let its freshly forked child run first",
        );
    }

    #[test]
    fn waking_fair_peer_does_not_immediately_preempt_current() {
        let mut scheduler = CFScheduler::<usize>::new();
        let current = Arc::new(CFSTask::new(1));
        let sleeper = Arc::new(CFSTask::new(2));

        scheduler.add_task(current.clone());
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(!scheduler.task_tick(&running));
        }

        scheduler.enqueue_task(sleeper, EnqueueReason::Wakeup);

        assert!(
            !scheduler.task_tick(&running),
            "a freshly woken fair task should not immediately cut ahead of the current peer",
        );
    }
}

mod cfs_intrusive_membership {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn removal_from_a_different_runqueue_is_safe_and_has_no_effect() {
        let mut owner = CFScheduler::<usize>::new();
        let mut other = CFScheduler::<usize>::new();
        let task = Arc::new(CFSTask::new(1));
        let same_key_peer = Arc::new(CFSTask::new(2));
        owner.add_task(task.clone());
        // Per-runqueue sequence spaces intentionally overlap, so this also
        // exercises the exact-key collision case in the wrong tree.
        other.add_task(same_key_peer.clone());

        assert!(other.remove_task(&task).is_none());
        let removed = owner.remove_task(&task).unwrap();
        assert!(Arc::ptr_eq(&removed, &task));
        assert!(owner.pick_next_task().is_none());
        let untouched = other.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&untouched, &same_key_peer));
    }

    #[test]
    fn linked_key_is_stable_until_remove_and_reinsert() {
        let mut scheduler = CFScheduler::<usize>::new();
        let task = Arc::new(CFSTask::new(1));
        let peer = Arc::new(CFSTask::new(2));
        scheduler.add_task(task.clone());
        scheduler.add_task(peer.clone());

        // This direct low-level mutation is deliberately stronger than the
        // axtask API, which removes a ready task before reconfiguration. It
        // must not mutate a key already embedded in the intrusive tree.
        assert!(task.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            rt_priority: 99,
            ..Default::default()
        }));
        let removed = scheduler.remove_task(&task).unwrap();
        assert!(Arc::ptr_eq(&removed, &task));

        scheduler.enqueue_task(removed, EnqueueReason::Wakeup);
        let first = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&first, &task));
        let second = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&second, &peer));
    }
}
