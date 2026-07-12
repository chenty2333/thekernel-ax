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
                    scheduler.add_task(Arc::new(<$task>::new(i))).unwrap();
                }

                for i in 0..NUM_TASKS * 10 - 1 {
                    let next = scheduler.pick_next_task().unwrap();
                    assert_eq!(*next.inner(), i % NUM_TASKS);
                    // pass a tick to ensure the order of tasks
                    scheduler.task_tick(&next);
                    scheduler.put_prev_task(next, false).unwrap();
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
                    scheduler.add_task(Arc::new(<$task>::new(i))).unwrap();
                }

                let t0 = std::time::Instant::now();
                for _ in 0..COUNT {
                    let next = scheduler.pick_next_task().unwrap();
                    scheduler.put_prev_task(next, false).unwrap();
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
                    scheduler.add_task(t).unwrap();
                }

                let t0 = std::time::Instant::now();
                for i in (0..NUM_TASKS).rev() {
                    let t = scheduler.remove_task(&tasks[i]).unwrap().unwrap();
                    assert_eq!(*t.inner(), i);
                }
                let t1 = std::time::Instant::now();
                println!(
                    "  {}: task remove speed: {:?}/task",
                    stringify!($scheduler),
                    (t1 - t0) / (NUM_TASKS as u32)
                );
            }

            #[test]
            fn foreign_scheduler_removal_is_safe() {
                let mut owner = <$scheduler>::new();
                let mut other = <$scheduler>::new();
                let task = Arc::new(<$task>::new(1));
                let peer = Arc::new(<$task>::new(2));

                owner.add_task(task.clone()).unwrap();
                other.add_task(peer.clone()).unwrap();

                assert!(matches!(
                    other.remove_task(&task),
                    Err(SchedulerError::ForeignQueue)
                ));
                let removed = owner.remove_task(&task).unwrap().unwrap();
                assert!(Arc::ptr_eq(&removed, &task));
                assert!(owner.pick_next_task().is_none());
                let untouched = other.pick_next_task().unwrap();
                assert!(Arc::ptr_eq(&untouched, &peer));
            }
        }
    };
}

def_test_sched!(fifo, FifoScheduler::<usize>, FifoTask::<usize>);
def_test_sched!(rr, RRScheduler::<usize, 5>, RRTask::<usize, 5>);
def_test_sched!(cfs, CFScheduler::<usize>, CFSTask::<usize>);

mod typed_runtime_updates {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn unsupported_and_invalid_priority_updates_are_distinct() {
        let fifo_task = Arc::new(FifoTask::new(1));
        assert_eq!(
            FifoScheduler::new().set_priority(&fifo_task, 0),
            Err(SchedulerError::UnsupportedOperation)
        );

        let rr_task = Arc::new(RRTask::<_, 5>::new(1));
        assert_eq!(
            RRScheduler::<_, 5>::new().set_priority(&rr_task, 0),
            Err(SchedulerError::UnsupportedOperation)
        );

        let task = Arc::new(CFSTask::new(1));
        let mut cfs = CFScheduler::new();
        assert_eq!(
            cfs.set_priority(&task, 20),
            Err(SchedulerError::InvalidParameters)
        );
        cfs.set_priority(&task, 5).unwrap();
        assert_eq!(task.sched_params().nice, 5);
    }

    #[test]
    fn priority_update_preserves_foreign_owner_and_class_errors() {
        let task = Arc::new(CFSTask::new(1));
        let mut owner = CFScheduler::new();
        let mut other = CFScheduler::new();
        owner.add_task(task.clone()).unwrap();
        assert_eq!(
            other.set_priority(&task, 1),
            Err(SchedulerError::ForeignQueue)
        );

        let rt = Arc::new(CFSTask::new(2));
        rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            rt_priority: 1,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            other.set_priority(&rt, 1),
            Err(SchedulerError::IncompatibleClass)
        );
    }

    #[test]
    fn fair_vruntime_inheritance_reports_class_and_queue_ownership() {
        let parent = Arc::new(CFSTask::new(1));
        parent
            .configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                rt_priority: 1,
                ..Default::default()
            })
            .unwrap();
        let child = Arc::new(CFSTask::new(2));
        assert_eq!(
            child.inherit_fair_vruntime_from(&parent),
            Err(SchedulerError::IncompatibleClass)
        );

        let fair_parent = Arc::new(CFSTask::new(3));
        let mut scheduler = CFScheduler::new();
        scheduler.add_task(child.clone()).unwrap();
        assert_eq!(
            child.inherit_fair_vruntime_from(&fair_parent),
            Err(SchedulerError::AlreadyQueued)
        );
    }
}

mod cfs_rt {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn rt_tasks_preempt_fair_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 10,
        })
        .unwrap();
        scheduler.add_task(fair.clone()).unwrap();
        scheduler.add_task(rt.clone()).unwrap();

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 2);
        scheduler.put_prev_task(first, false).unwrap();

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(*second.inner(), 2);
    }

    #[test]
    fn ready_rt_task_preempts_running_fair_task() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 50,
        })
        .unwrap();

        scheduler.add_task(fair.clone()).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);
        scheduler.enqueue_task(rt, EnqueueReason::Wakeup).unwrap();

        assert!(scheduler.task_tick(&running));
    }

    #[test]
    fn higher_rt_priority_runs_first() {
        let mut scheduler = CFScheduler::<usize>::new();
        let low = Arc::new(CFSTask::new(1));
        let high = Arc::new(CFSTask::new(2));
        low.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 10,
        })
        .unwrap();
        high.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 20,
        })
        .unwrap();
        scheduler.add_task(low).unwrap();
        scheduler.add_task(high).unwrap();

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 2);
    }

    #[test]
    fn rr_rotates_between_equal_priority_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let a = Arc::new(CFSTask::new(1));
        let b = Arc::new(CFSTask::new(2));
        for task in [&a, &b] {
            task.configure(CfsTaskParams {
                class: CfsTaskClass::RoundRobin,
                nice: 0,
                rt_priority: 42,
            })
            .unwrap();
            scheduler.add_task(task.clone()).unwrap();
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&first), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(first, false).unwrap();

        let second = scheduler.pick_next_task().unwrap();
        assert_eq!(*second.inner(), 2);
    }

    #[test]
    fn rr_timer_preemption_rotates_between_equal_priority_tasks() {
        let mut scheduler = CFScheduler::<usize>::new();
        let a = Arc::new(CFSTask::new(1));
        let b = Arc::new(CFSTask::new(2));
        for task in [&a, &b] {
            task.configure(CfsTaskParams {
                class: CfsTaskClass::RoundRobin,
                nice: 0,
                rt_priority: 42,
            })
            .unwrap();
            scheduler.add_task(task.clone()).unwrap();
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for tick in 0..RR_TIMESLICE_TICKS {
            assert_eq!(scheduler.task_tick(&first), tick + 1 == RR_TIMESLICE_TICKS);
        }
        scheduler.put_prev_task(first, true).unwrap();

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
            task.configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: 200,
            })
            .unwrap();
            scheduler.add_task(task.clone()).unwrap();
        }

        let first = scheduler.pick_next_task().unwrap();
        assert_eq!(*first.inner(), 1);
        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(
                !scheduler.task_tick(&first),
                "FIFO tasks must not rotate same-priority peers on timer ticks",
            );
        }
        scheduler.put_prev_task(first, true).unwrap();

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            1,
            "a preempted FIFO task keeps precedence over same-priority peers",
        );
    }

    #[test]
    fn fifo_rt_keeps_precedence_over_fair_task() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 200,
        })
        .unwrap();
        scheduler.add_task(fair).unwrap();
        scheduler.add_task(rt).unwrap();

        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 2);
        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(
                !scheduler.task_tick(&running),
                "FIFO tasks must not be time-slice preempted for fair work",
            );
        }
        scheduler.put_prev_task(running, true).unwrap();

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
            task.configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: 200,
            })
            .unwrap();
            scheduler.add_task(task.clone()).unwrap();
        }
        scheduler.add_task(fair).unwrap();

        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 2);
        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(!scheduler.task_tick(&running));
        }
        scheduler.put_prev_task(running, true).unwrap();

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            2,
            "same-priority FIFO peers and fair tasks wait until the running FIFO task \
             blocks, yields, exits, or is preempted by higher priority RT",
        );
    }

    #[test]
    fn fair_task_is_preempted_while_rt_is_ready() {
        let mut scheduler = CFScheduler::<usize>::new();
        let fair = Arc::new(CFSTask::new(1));
        let rt = Arc::new(CFSTask::new(2));
        rt.configure(CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 200,
        })
        .unwrap();
        scheduler.add_task(rt).unwrap();

        assert!(
            scheduler.task_tick(&fair),
            "fair task should request reschedule whenever RT work is ready",
        );
        scheduler.put_prev_task(fair, true).unwrap();

        let next_rt = scheduler.pick_next_task().unwrap();
        assert_eq!(*next_rt.inner(), 2);
    }

    #[test]
    fn full_nonzero_u8_realtime_priority_domain_is_supported() {
        let mut scheduler = CFScheduler::<usize>::new();
        let maximum = Arc::new(CFSTask::new(1));
        let lower = Arc::new(CFSTask::new(2));

        maximum
            .configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: u8::MAX,
            })
            .unwrap();
        lower
            .configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                nice: 0,
                rt_priority: u8::MAX - 1,
            })
            .unwrap();
        scheduler.add_task(lower).unwrap();
        scheduler.add_task(maximum).unwrap();

        assert_eq!(*scheduler.pick_next_task().unwrap().inner(), 1);
    }

    #[test]
    fn rejected_zero_realtime_priority_preserves_ready_task_state() {
        let mut scheduler = CFScheduler::<usize>::new();
        let task = Arc::new(CFSTask::new(1));
        let original = CfsTaskParams {
            class: CfsTaskClass::Fifo,
            nice: 0,
            rt_priority: 42,
        };
        task.configure(original).unwrap();
        scheduler.add_task(task.clone()).unwrap();

        assert_eq!(
            task.configure(CfsTaskParams {
                class: CfsTaskClass::RoundRobin,
                nice: 0,
                rt_priority: 0,
            }),
            Err(SchedulerError::InvalidParameters)
        );
        assert_eq!(task.sched_params(), original);

        let selected = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&selected, &task));
        assert_eq!(selected.sched_params(), original);
    }
}

mod cfs_child_spawn {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn new_fair_child_does_not_immediately_preempt_parent() {
        let mut scheduler = CFScheduler::<usize>::new();
        let parent = Arc::new(CFSTask::new(1));

        scheduler.add_task(parent.clone()).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(!scheduler.task_tick(&running));
        }

        let child = Arc::new(CFSTask::new(2));
        child.inherit_fair_vruntime_from(&running).unwrap();
        scheduler.add_task(child).unwrap();

        assert!(
            !scheduler.task_tick(&running),
            "new child should inherit the parent's vruntime instead of cutting to the floor",
        );
    }

    #[test]
    fn yielding_parent_lets_new_child_run() {
        let mut scheduler = CFScheduler::<usize>::new();
        let parent = Arc::new(CFSTask::new(1));

        scheduler.add_task(parent.clone()).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        let child = Arc::new(CFSTask::new(2));
        child.inherit_fair_vruntime_from(&running).unwrap();
        scheduler.add_task(child).unwrap();

        scheduler
            .enqueue_task(running, EnqueueReason::Yield)
            .unwrap();

        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(
            *next.inner(),
            2,
            "a yielding parent should let its new child run first",
        );
    }

    #[test]
    fn waking_fair_peer_does_not_immediately_preempt_current() {
        let mut scheduler = CFScheduler::<usize>::new();
        let current = Arc::new(CFSTask::new(1));
        let sleeper = Arc::new(CFSTask::new(2));

        scheduler.add_task(current.clone()).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        assert_eq!(*running.inner(), 1);

        for _ in 0..(RR_TIMESLICE_TICKS * 2) {
            assert!(!scheduler.task_tick(&running));
        }

        scheduler
            .enqueue_task(sleeper, EnqueueReason::Wakeup)
            .unwrap();

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
        owner.add_task(task.clone()).unwrap();
        // Per-runqueue sequence spaces intentionally overlap, so this also
        // exercises the exact-key collision case in the wrong tree.
        other.add_task(same_key_peer.clone()).unwrap();

        assert!(matches!(
            other.remove_task(&task),
            Err(SchedulerError::ForeignQueue)
        ));
        let removed = owner.remove_task(&task).unwrap().unwrap();
        assert!(Arc::ptr_eq(&removed, &task));
        assert!(owner.pick_next_task().is_none());
        let untouched = other.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&untouched, &same_key_peer));
    }

    #[test]
    fn queued_configuration_uses_the_scheduler_transaction() {
        let mut scheduler = CFScheduler::<usize>::new();
        let task = Arc::new(CFSTask::new(1));
        let peer = Arc::new(CFSTask::new(2));
        scheduler.add_task(task.clone()).unwrap();
        scheduler.add_task(peer.clone()).unwrap();

        // This direct low-level mutation is deliberately stronger than the
        // axtask API, which removes a ready task before reconfiguration. It
        // must not mutate a key already embedded in the intrusive tree.
        assert_eq!(
            task.configure(CfsTaskParams {
                class: CfsTaskClass::Fifo,
                rt_priority: 200,
                ..Default::default()
            }),
            Err(SchedulerError::AlreadyQueued)
        );
        scheduler
            .set_task_params(
                &task,
                CfsTaskParams {
                    class: CfsTaskClass::Fifo,
                    rt_priority: 200,
                    ..Default::default()
                },
            )
            .unwrap();
        let first = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&first, &task));
        let second = scheduler.pick_next_task().unwrap();
        assert!(Arc::ptr_eq(&second, &peer));
    }
}

mod lifecycle_boundaries {
    use alloc::sync::Arc;

    use crate::*;

    #[test]
    fn zero_round_robin_slice_is_rejected_without_claiming_the_task() {
        let task = Arc::new(RRTask::<usize, 0>::new(1));
        let mut scheduler = RRScheduler::<usize, 0>::new();
        assert!(matches!(
            scheduler.add_task(task.clone()),
            Err(SchedulerError::InvalidTimeSlice)
        ));
        assert!(matches!(
            scheduler.add_task(task),
            Err(SchedulerError::InvalidTimeSlice)
        ));
    }

    #[test]
    fn maximum_round_robin_slice_is_representable_and_does_not_wrap() {
        let task = Arc::new(RRTask::<usize, { usize::MAX }>::new(1));
        let mut scheduler = RRScheduler::<usize, { usize::MAX }>::new();
        scheduler.add_task(task).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        assert!(!scheduler.task_tick(&running));
    }

    #[test]
    fn expired_round_robin_counter_saturates_at_zero() {
        let task = Arc::new(RRTask::<usize, 1>::new(1));
        let mut scheduler = RRScheduler::<usize, 1>::new();
        scheduler.add_task(task).unwrap();
        let running = scheduler.pick_next_task().unwrap();
        for _ in 0..8 {
            assert!(scheduler.task_tick(&running));
        }
    }

    #[test]
    fn dropping_a_scheduler_releases_surviving_task_ownership() {
        let fifo = Arc::new(FifoTask::new(1));
        {
            let mut scheduler = FifoScheduler::new();
            scheduler.add_task(fifo.clone()).unwrap();
        }
        FifoScheduler::new().add_task(fifo).unwrap();

        let rr = Arc::new(RRTask::<_, 2>::new(2));
        {
            let mut scheduler = RRScheduler::<_, 2>::new();
            scheduler.add_task(rr.clone()).unwrap();
        }
        RRScheduler::<_, 2>::new().add_task(rr).unwrap();

        let cfs = Arc::new(CFSTask::new(3));
        {
            let mut scheduler = CFScheduler::new();
            scheduler.add_task(cfs.clone()).unwrap();
        }
        CFScheduler::new().add_task(cfs).unwrap();
    }
}
