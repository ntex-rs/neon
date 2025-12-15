//! A thread pool to perform blocking operations in other threads.
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::{fmt, thread, time::Duration};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

/// An error that may be emitted when all worker threads are busy.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DispatchError;

impl std::error::Error for DispatchError {}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "All threads are busy".fmt(f)
    }
}

type BoxedDispatchable = Box<dyn Dispatchable + Send>;

/// A trait for dispatching a closure. It's implemented for all `FnOnce() + Send
/// + 'static` but may also be implemented for any other types that are `Send`
///   and `'static`.
pub trait Dispatchable: Send + 'static {
    /// Run the dispatchable
    fn run(self: Box<Self>);
}

impl<F> Dispatchable for F
where
    F: FnOnce() + Send + 'static,
{
    fn run(self: Box<Self>) {
        (*self)()
    }
}

struct CounterGuard(Arc<AtomicUsize>);

impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn worker(
    receiver: Receiver<BoxedDispatchable>,
    counter: Arc<AtomicUsize>,
    timeout: Duration,
) -> impl FnOnce() {
    move || {
        counter.fetch_add(1, Ordering::AcqRel);
        let _guard = CounterGuard(counter);
        while let Ok(f) = receiver.recv_timeout(timeout) {
            f.run();
        }
    }
}

/// A thread pool to perform blocking operations in other threads.
#[derive(Debug, Clone)]
pub struct ThreadPool {
    sender: Sender<BoxedDispatchable>,
    receiver: Receiver<BoxedDispatchable>,
    counter: Arc<AtomicUsize>,
    thread_limit: usize,
    recv_timeout: Duration,
}

impl ThreadPool {
    /// Create [`ThreadPool`] with thread number limit and channel receive
    /// timeout.
    pub fn new(thread_limit: usize, recv_timeout: Duration) -> Self {
        let (sender, receiver) = bounded(0);
        Self {
            sender,
            receiver,
            thread_limit,
            recv_timeout,
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Send a dispatchable, usually a closure, to another thread. Usually the
    /// user should not use it. When all threads are busy and thread number
    /// limit has been reached, it will return an error with the original
    /// dispatchable.
    pub fn dispatch(&self, f: BoxedDispatchable) -> Result<(), DispatchError> {
        match self.sender.try_send(f) {
            Ok(_) => Ok(()),
            Err(e) => match e {
                TrySendError::Full(f) => {
                    let cnt = self.counter.load(Ordering::Acquire);
                    if cnt >= self.thread_limit {
                        Err(DispatchError)
                    } else {
                        thread::Builder::new()
                            .name(format!("pool-wrk:{}", cnt))
                            .spawn(worker(
                                self.receiver.clone(),
                                self.counter.clone(),
                                self.recv_timeout,
                            ))
                            .expect("Cannot construct new thread");
                        self.sender.send(f).expect("the channel should not be full");
                        Ok(())
                    }
                }
                TrySendError::Disconnected(_) => {
                    unreachable!("receiver should not all disconnected")
                }
            },
        }
    }
}
