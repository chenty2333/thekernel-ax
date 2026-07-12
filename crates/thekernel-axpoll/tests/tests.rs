use std::{
    sync::{
        Arc, Barrier, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    task::Waker,
    thread,
    time::Duration,
};

use axpoll::{PollSet, RegisterError, UpdateError};

struct Counter(AtomicUsize);

impl Counter {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicUsize::new(0)))
    }

    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    fn add(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl std::task::Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.add();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.add();
    }
}

#[test]
fn register_and_wake() {
    let poll_set: PollSet = PollSet::new();
    let counter = Counter::new();
    let waker = Waker::from(counter.clone());

    poll_set.register(&waker).unwrap();
    assert_eq!(poll_set.len(), 1);
    assert_eq!(poll_set.wake(), 1);
    assert_eq!(poll_set.len(), 0);
    assert_eq!(counter.count(), 1);
}

#[test]
fn wake_is_one_shot_but_does_not_close() {
    let poll_set = PollSet::<1>::new();
    let first = Counter::new();
    let first_waker = Waker::from(first.clone());
    poll_set.register(&first_waker).unwrap();

    assert_eq!(poll_set.wake(), 1);
    assert_eq!(poll_set.wake(), 0);
    assert!(!poll_set.is_closed());

    let second = Counter::new();
    let second_waker = Waker::from(second.clone());
    poll_set.register(&second_waker).unwrap();
    assert_eq!(poll_set.wake(), 1);
    assert_eq!(first.count(), 1);
    assert_eq!(second.count(), 1);
}

#[test]
fn equivalent_wakers_are_independent_registrations() {
    let poll_set = PollSet::<2>::new();
    let counter = Counter::new();
    let waker = Waker::from(counter.clone());

    let first = poll_set.register(&waker).unwrap();
    let second = poll_set.register(&waker).unwrap();
    assert_ne!(first, second);
    assert_eq!(poll_set.len(), 2);

    assert!(poll_set.cancel(first));
    assert_eq!(poll_set.len(), 1);
    assert_eq!(poll_set.wake(), 1);
    assert_eq!(counter.count(), 1);
}

#[test]
fn equivalent_waker_does_not_bypass_full_capacity() {
    let poll_set = PollSet::<1>::new();
    let counter = Counter::new();
    let waker = Waker::from(counter.clone());
    poll_set.register(&waker).unwrap();

    assert_eq!(poll_set.register(&waker), Err(RegisterError::Full));
    assert_eq!(poll_set.wake(), 1);
    assert_eq!(counter.count(), 1);
}

#[test]
fn full_is_explicit_and_does_not_overwrite() {
    let poll_set = PollSet::<2>::new();
    let first = Counter::new();
    let second = Counter::new();
    let rejected = Counter::new();

    poll_set.register(&Waker::from(first.clone())).unwrap();
    poll_set.register(&Waker::from(second.clone())).unwrap();
    assert_eq!(
        poll_set.register(&Waker::from(rejected.clone())),
        Err(RegisterError::Full)
    );
    assert_eq!(poll_set.len(), 2);

    assert_eq!(poll_set.wake(), 2);
    assert_eq!(first.count(), 1);
    assert_eq!(second.count(), 1);
    assert_eq!(rejected.count(), 0);
}

#[test]
fn zero_capacity_is_always_full() {
    let poll_set = PollSet::<0>::new();
    let counter = Counter::new();
    assert_eq!(poll_set.capacity(), 0);
    assert_eq!(
        poll_set.register(&Waker::from(counter)),
        Err(RegisterError::Full)
    );
}

#[test]
fn cancel_removes_registration() {
    let poll_set: PollSet = PollSet::new();
    let counter = Counter::new();
    let token = poll_set.register(&Waker::from(counter.clone())).unwrap();

    assert!(poll_set.cancel(token));
    assert!(!poll_set.cancel(token));
    assert_eq!(poll_set.wake(), 0);
    assert_eq!(counter.count(), 0);
}

#[test]
fn reused_slot_changes_generation_and_rejects_stale_token() {
    let poll_set = PollSet::<1>::new();
    let first = Counter::new();
    let first_token = poll_set.register(&Waker::from(first)).unwrap();
    assert!(poll_set.cancel(first_token));

    let second = Counter::new();
    let second_token = poll_set.register(&Waker::from(second.clone())).unwrap();
    assert_ne!(first_token, second_token);
    assert!(!poll_set.cancel(first_token));
    assert_eq!(
        poll_set.update(first_token, &Waker::from(Counter::new())),
        Err(UpdateError::InvalidToken)
    );
    assert!(poll_set.cancel(second_token));
    assert_eq!(second.count(), 0);
}

#[test]
fn foreign_registry_token_is_invalid() {
    let first = PollSet::<1>::new();
    let second = PollSet::<1>::new();
    let token = first.register(&Waker::from(Counter::new())).unwrap();

    assert!(!second.cancel(token));
    assert_eq!(
        second.update(token, &Waker::from(Counter::new())),
        Err(UpdateError::InvalidToken)
    );
    assert_eq!(first.len(), 1);
}

#[test]
fn update_replaces_waker_without_changing_token() {
    let poll_set = PollSet::<1>::new();
    let first = Counter::new();
    let second = Counter::new();
    let token = poll_set.register(&Waker::from(first.clone())).unwrap();

    poll_set
        .update(token, &Waker::from(second.clone()))
        .unwrap();
    assert_eq!(poll_set.len(), 1);
    assert_eq!(poll_set.wake(), 1);
    assert_eq!(first.count(), 0);
    assert_eq!(second.count(), 1);
}

#[test]
fn close_wakes_is_idempotent_and_rejects_registration() {
    let poll_set = PollSet::<2>::new();
    let first = Counter::new();
    let second = Counter::new();
    let token = poll_set.register(&Waker::from(first.clone())).unwrap();
    poll_set.register(&Waker::from(second.clone())).unwrap();

    assert_eq!(poll_set.close(), 2);
    assert!(poll_set.is_closed());
    assert!(poll_set.is_empty());
    assert_eq!(first.count(), 1);
    assert_eq!(second.count(), 1);
    assert_eq!(poll_set.close(), 0);
    assert_eq!(poll_set.wake(), 0);
    assert_eq!(
        poll_set.register(&Waker::from(Counter::new())),
        Err(RegisterError::Closed)
    );
    assert_eq!(
        poll_set.update(token, &Waker::from(Counter::new())),
        Err(UpdateError::Closed)
    );
    assert!(!poll_set.cancel(token));
}

#[test]
fn drop_closes_and_wakes() {
    let counters = (0..4).map(|_| Counter::new()).collect::<Vec<_>>();
    {
        let poll_set = PollSet::<4>::new();
        for counter in &counters {
            poll_set.register(&Waker::from(counter.clone())).unwrap();
        }
    }
    assert!(counters.iter().all(|counter| counter.count() == 1));
}

#[test]
fn wake_and_cancel_race_has_one_winner() {
    for _ in 0..128 {
        let poll_set = Arc::new(PollSet::<1>::new());
        let counter = Counter::new();
        let token = poll_set.register(&Waker::from(counter.clone())).unwrap();
        let barrier = Arc::new(Barrier::new(3));

        let wake_set = poll_set.clone();
        let wake_barrier = barrier.clone();
        let wake = thread::spawn(move || {
            wake_barrier.wait();
            wake_set.wake()
        });

        let cancel_set = poll_set.clone();
        let cancel_barrier = barrier.clone();
        let cancel = thread::spawn(move || {
            cancel_barrier.wait();
            cancel_set.cancel(token)
        });

        barrier.wait();
        let woken = wake.join().unwrap();
        let cancelled = cancel.join().unwrap();
        let wake_count = counter.count();

        assert!(
            (woken, cancelled, wake_count) == (1, false, 1)
                || (woken, cancelled, wake_count) == (0, true, 0),
            "unexpected wake/cancel outcome: {woken:?}, {cancelled:?}, {wake_count:?}",
        );
        assert!(poll_set.is_empty());
    }
}

struct LenOnWake<const CAPACITY: usize> {
    poll_set: Weak<PollSet<CAPACITY>>,
    called: AtomicBool,
}

impl<const CAPACITY: usize> std::task::Wake for LenOnWake<CAPACITY> {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let poll_set = self.poll_set.upgrade().unwrap();
        assert_eq!(poll_set.len(), 0);
        self.called.store(true, Ordering::SeqCst);
    }
}

#[test]
fn wake_callback_can_reenter_registry() {
    let poll_set = Arc::new(PollSet::<1>::new());
    let callback = Arc::new(LenOnWake {
        poll_set: Arc::downgrade(&poll_set),
        called: AtomicBool::new(false),
    });
    poll_set.register(&Waker::from(callback.clone())).unwrap();

    let worker_set = poll_set.clone();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || sender.send(worker_set.wake()).unwrap());

    assert_eq!(receiver.recv_timeout(Duration::from_secs(2)), Ok(1));
    worker.join().unwrap();
    assert!(callback.called.load(Ordering::SeqCst));
}

struct LenOnDrop<const CAPACITY: usize> {
    poll_set: Weak<PollSet<CAPACITY>>,
    dropped: Arc<AtomicBool>,
}

impl<const CAPACITY: usize> std::task::Wake for LenOnDrop<CAPACITY> {
    fn wake(self: Arc<Self>) {}
}

impl<const CAPACITY: usize> Drop for LenOnDrop<CAPACITY> {
    fn drop(&mut self) {
        if let Some(poll_set) = self.poll_set.upgrade() {
            assert_eq!(poll_set.len(), 0);
        }
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[test]
fn final_waker_drop_can_reenter_registry() {
    let poll_set = Arc::new(PollSet::<1>::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let callback = Arc::new(LenOnDrop {
        poll_set: Arc::downgrade(&poll_set),
        dropped: dropped.clone(),
    });
    let waker = Waker::from(callback);
    let token = poll_set.register(&waker).unwrap();
    drop(waker);

    let worker_set = poll_set.clone();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || sender.send(worker_set.cancel(token)).unwrap());

    assert_eq!(receiver.recv_timeout(Duration::from_secs(2)), Ok(true));
    worker.join().unwrap();
    assert!(dropped.load(Ordering::SeqCst));
}
