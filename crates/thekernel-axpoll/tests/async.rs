use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use axpoll::{PollSet, RegisterError, RegistrationToken, UpdateError};
use tokio::task::yield_now;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum WaitError {
    Register(RegisterError),
    Update(UpdateError),
}

struct WaitFuture<const CAPACITY: usize> {
    poll_set: Arc<PollSet<CAPACITY>>,
    ready: Arc<AtomicBool>,
    registration: Option<RegistrationToken>,
}

impl<const CAPACITY: usize> Future for WaitFuture<CAPACITY> {
    type Output = Result<(), WaitError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.ready.load(Ordering::SeqCst) {
            if let Some(registration) = this.registration.take() {
                this.poll_set.cancel(registration);
            }
            return Poll::Ready(Ok(()));
        }

        if let Some(registration) = this.registration {
            match this.poll_set.update(registration, context.waker()) {
                Ok(()) => return Poll::Pending,
                Err(UpdateError::InvalidToken) => {
                    this.registration = None;
                }
                Err(error) => return Poll::Ready(Err(WaitError::Update(error))),
            }
        }

        match this.poll_set.register(context.waker()) {
            Ok(registration) => {
                this.registration = Some(registration);
                Poll::Pending
            }
            Err(error) => Poll::Ready(Err(WaitError::Register(error))),
        }
    }
}

impl<const CAPACITY: usize> Drop for WaitFuture<CAPACITY> {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            self.poll_set.cancel(registration);
        }
    }
}

impl<const CAPACITY: usize> WaitFuture<CAPACITY> {
    fn new(poll_set: Arc<PollSet<CAPACITY>>, ready: Arc<AtomicBool>) -> Self {
        Self {
            poll_set,
            ready,
            registration: None,
        }
    }
}

async fn wait_for_len<const CAPACITY: usize>(poll_set: &PollSet<CAPACITY>, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while poll_set.len() != expected {
            yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn async_wake_single() {
    let poll_set: Arc<PollSet> = Arc::new(PollSet::new());
    let ready = Arc::new(AtomicBool::new(false));
    let future = WaitFuture::new(poll_set.clone(), ready.clone());

    let worker_set = poll_set.clone();
    let worker = tokio::spawn(async move {
        yield_now().await;
        ready.store(true, Ordering::SeqCst);
        worker_set.wake();
    });

    future.await.unwrap();
    worker.await.unwrap();
}

#[tokio::test]
async fn async_wake_many_at_exact_capacity() {
    const WAITERS: usize = 64;

    let poll_set = Arc::new(PollSet::<WAITERS>::new());
    let mut flags = Vec::new();
    let mut handles = Vec::new();
    for _ in 0..WAITERS {
        let flag = Arc::new(AtomicBool::new(false));
        let future = WaitFuture::new(poll_set.clone(), flag.clone());
        handles.push(tokio::spawn(future));
        flags.push(flag);
    }
    wait_for_len(&poll_set, WAITERS).await;

    let mut ready = Vec::new();
    let mut pending = Vec::new();
    for (index, handle) in handles.into_iter().enumerate() {
        if index % 2 == 0 {
            flags[index].store(true, Ordering::SeqCst);
            ready.push(handle);
        } else {
            pending.push(handle);
        }
    }
    assert_eq!(poll_set.wake(), WAITERS);
    for handle in ready {
        handle.await.unwrap().unwrap();
    }

    wait_for_len(&poll_set, WAITERS / 2).await;
    for (index, flag) in flags.iter().enumerate() {
        if index % 2 != 0 {
            flag.store(true, Ordering::SeqCst);
        }
    }
    assert_eq!(poll_set.wake(), WAITERS / 2);
    for handle in pending {
        handle.await.unwrap().unwrap();
    }
    assert!(poll_set.is_empty());
}

#[test]
fn future_updates_waker_and_drop_cancels_registration() {
    struct Counter(AtomicUsize);

    impl std::task::Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let poll_set = Arc::new(PollSet::<1>::new());
    let ready = Arc::new(AtomicBool::new(false));
    let mut future = Box::pin(WaitFuture::new(poll_set.clone(), ready));
    let first = Arc::new(Counter(AtomicUsize::new(0)));
    let second = Arc::new(Counter(AtomicUsize::new(0)));

    let first_waker = Waker::from(first.clone());
    let mut first_context = Context::from_waker(&first_waker);
    assert_eq!(future.as_mut().poll(&mut first_context), Poll::Pending);
    assert_eq!(poll_set.len(), 1);

    let second_waker = Waker::from(second.clone());
    let mut second_context = Context::from_waker(&second_waker);
    assert_eq!(future.as_mut().poll(&mut second_context), Poll::Pending);
    assert_eq!(poll_set.len(), 1);

    assert_eq!(poll_set.wake(), 1);
    assert_eq!(first.0.load(Ordering::SeqCst), 0);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);

    // A wake consumed the token. Dropping the future attempts a stale cancel,
    // which must not affect a later registration.
    drop(future);
    assert!(poll_set.is_empty());
}

#[test]
fn dropping_pending_future_cancels_registration() {
    let poll_set = Arc::new(PollSet::<1>::new());
    let ready = Arc::new(AtomicBool::new(false));
    let mut future = Box::pin(WaitFuture::new(poll_set.clone(), ready));
    let waker = futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);

    assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);
    assert_eq!(poll_set.len(), 1);
    drop(future);
    assert!(poll_set.is_empty());
}
