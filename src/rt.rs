use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::{HashMap, VecDeque};
use std::{future::Future, io, os::fd, sync::Arc, thread, time::Duration};

use async_task::{Runnable, Task};
use crossbeam_queue::SegQueue;
use swap_buffer_queue::{buffer::ArrayBuffer, error::TryEnqueueError, Queue};

use crate::driver::{Driver, DriverApi, DriverType, Handler, NotifyHandle, PollResult};
use crate::pool::ThreadPool;

scoped_tls::scoped_thread_local!(static CURRENT_RUNTIME: Runtime);

/// Type alias for `oneshot::Receiver<Result<T, Box<dyn Any + Send>>>`, which resolves to an
/// `Err` when the spawned task panicked.
pub type JoinHandle<T> = oneshot::Receiver<Result<T, Box<dyn Any + Send>>>;

/// The async runtime for ntex.
///
/// It is a thread local runtime, and cannot be sent to other threads.
pub struct Runtime {
    driver: Driver,
    queue: Arc<RunnableQueue>,
    pub(crate) pool: ThreadPool,
    values: RefCell<HashMap<TypeId, Box<dyn Any>, fxhash::FxBuildHasher>>,
}

impl Runtime {
    /// Create [`Runtime`] with default config.
    pub fn new() -> io::Result<Self> {
        Self::builder().build()
    }

    /// Create a builder for [`Runtime`].
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    #[allow(clippy::arc_with_non_send_sync)]
    fn with_builder(builder: &RuntimeBuilder) -> io::Result<Self> {
        let driver = builder.build_driver()?;
        let queue = Arc::new(RunnableQueue::new(builder.event_interval, driver.handle()));
        Ok(Self {
            queue,
            driver,
            pool: ThreadPool::new(builder.pool_limit, builder.pool_recv_timeout),
            values: RefCell::new(HashMap::default()),
        })
    }

    /// Perform a function on the current runtime.
    ///
    /// ## Panics
    ///
    /// This method will panic if there are no running [`Runtime`].
    pub fn with_current<T, F: FnOnce(&Self) -> T>(f: F) -> T {
        #[cold]
        fn not_in_neon_runtime() -> ! {
            panic!("not in a neon runtime")
        }

        if CURRENT_RUNTIME.is_set() {
            CURRENT_RUNTIME.with(f)
        } else {
            not_in_neon_runtime()
        }
    }

    #[inline]
    /// Get handle for current runtime
    pub fn handle(&self) -> Handle {
        Handle {
            queue: self.queue.clone(),
        }
    }

    #[inline]
    /// Get current driver type
    pub fn driver_type(&self) -> DriverType {
        self.driver.tp()
    }

    #[inline]
    /// Register io handler
    pub fn register_handler<F>(&self, f: F)
    where
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        self.driver.register(f)
    }

    /// Block on the future till it completes.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        CURRENT_RUNTIME.set(self, || {
            let mut result = None;
            let mut delayed = false;
            unsafe { self.spawn_unchecked(async { result = Some(future.await) }) }.detach();

            self.driver
                .poll(|| {
                    delayed |= delayed;
                    if let Some(result) = result.take() {
                        PollResult::Ready(result)
                    } else if self.queue.run(delayed) {
                        PollResult::HasTasks
                    } else {
                        PollResult::Pending
                    }
                })
                .expect("Failed to poll driver")
        })
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> Task<F::Output> {
        unsafe { self.spawn_unchecked(future) }
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    pub unsafe fn spawn_unchecked<F: Future>(&self, future: F) -> Task<F::Output> {
        let queue = self.queue.clone();
        let schedule = move |runnable| {
            queue.schedule(runnable);
        };
        let (runnable, task) = async_task::spawn_unchecked(future, schedule);
        runnable.schedule();
        task
    }

    /// Spawns a blocking task in a new thread, and wait for it.
    ///
    /// The task will not be cancelled even if the future is dropped.
    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        crate::driver::spawn_blocking(
            self,
            &self.driver,
            Box::new(move || {
                if !tx.is_closed() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
                    let _ = tx.send(result);
                }
            }),
        );

        rx
    }

    /// Get a type previously inserted to this runtime or create new one.
    pub fn value<T, F>(f: F) -> T
    where
        T: Clone + 'static,
        F: FnOnce(&Runtime) -> T,
    {
        Runtime::with_current(|rt| {
            let val = rt
                .values
                .borrow()
                .get(&TypeId::of::<T>())
                .and_then(|boxed| boxed.downcast_ref().cloned());
            if let Some(val) = val {
                val
            } else {
                let val = f(rt);
                rt.values
                    .borrow_mut()
                    .insert(TypeId::of::<T>(), Box::new(val.clone()));
                val
            }
        })
    }
}

impl fd::AsRawFd for Runtime {
    fn as_raw_fd(&self) -> fd::RawFd {
        self.driver.as_raw_fd()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        CURRENT_RUNTIME.set(self, || self.queue.clear())
    }
}

/// Handle for current runtime
pub struct Handle {
    queue: Arc<RunnableQueue>,
}

impl Handle {
    /// Wake up runtime
    pub fn notify(&self) -> io::Result<()> {
        self.queue.driver.notify()
    }

    /// Spawns a new asynchronous task, returning a [`Task`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + Send + 'static>(&self, future: F) -> Task<F::Output> {
        let queue = self.queue.clone();
        let schedule = move |runnable| {
            queue.schedule(runnable);
        };
        let (runnable, task) = unsafe { async_task::spawn_unchecked(future, schedule) };
        runnable.schedule();
        task
    }
}

struct RunnableQueue {
    id: thread::ThreadId,
    idle: Cell<bool>,
    driver: NotifyHandle,
    event_interval: usize,
    local_queue: UnsafeCell<VecDeque<Runnable>>,
    sync_fixed_queue: Queue<ArrayBuffer<Runnable, 128>>,
    sync_queue: SegQueue<Runnable>,
}

impl RunnableQueue {
    fn new(event_interval: usize, driver: NotifyHandle) -> Self {
        Self {
            driver,
            event_interval,
            id: thread::current().id(),
            idle: Cell::new(true),
            local_queue: UnsafeCell::new(VecDeque::new()),
            sync_fixed_queue: Queue::default(),
            sync_queue: SegQueue::new(),
        }
    }

    fn schedule(&self, runnable: Runnable) {
        if self.id == thread::current().id() {
            unsafe { (*self.local_queue.get()).push_back(runnable) };
            if self.idle.get() {
                self.idle.set(false);
                self.driver.notify().ok();
            }
        } else {
            //if let Err(TryEnqueueError::InsufficientCapacity([runnable])) =
            //    self.sync_fixed_queue.try_enqueue([runnable])
            //{
            self.sync_queue.push(runnable);
            //}
            self.driver.notify().ok();
        }
    }

    fn run(&self, _delayed: bool) -> bool {
        self.idle.set(false);

        for _ in 0..self.event_interval {
            let task = unsafe { (*self.local_queue.get()).pop_front() };
            if let Some(task) = task {
                task.run();
            } else {
                break;
            }
        }

        // if let Ok(buf) = self.sync_fixed_queue.try_dequeue() {
        //     for task in buf {
        //         task.run();
        //     }
        // }

        // if delayed {
        for _ in 0..self.event_interval {
            if !self.sync_queue.is_empty() {
                if let Some(task) = self.sync_queue.pop() {
                    task.run();
                    continue;
                }
            }
            break;
        }
        //}
        self.idle.set(true);

        !unsafe { (*self.local_queue.get()).is_empty() }
            || !self.sync_fixed_queue.is_empty()
            || !self.sync_queue.is_empty()
    }

    fn clear(&self) {
        while self.sync_queue.pop().is_some() {}
        unsafe { (*self.local_queue.get()).clear() };
    }
}

/// Builder for [`Runtime`].
#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    event_interval: usize,
    pool_limit: usize,
    pool_recv_timeout: Duration,
    io_queue_capacity: u32,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuilder {
    /// Create the builder with default config.
    pub fn new() -> Self {
        Self {
            event_interval: 61,
            pool_limit: 256,
            pool_recv_timeout: Duration::from_secs(60),
            io_queue_capacity: 1024,
        }
    }

    /// Sets the number of scheduler ticks after which the scheduler will poll
    /// for external events (timers, I/O, and so on).
    ///
    /// A scheduler “tick” roughly corresponds to one poll invocation on a task.
    pub fn event_interval(&mut self, val: usize) -> &mut Self {
        self.event_interval = val;
        self
    }

    /// Set the capacity of the inner event queue or submission queue, if
    /// exists. The default value is 1024.
    pub fn io_queue_capacity(&mut self, capacity: u32) -> &mut Self {
        self.io_queue_capacity = capacity;
        self
    }

    /// Set the thread number limit of the inner thread pool, if exists. The
    /// default value is 256.
    pub fn thread_pool_limit(&mut self, value: usize) -> &mut Self {
        self.pool_limit = value;
        self
    }

    /// Set the waiting timeout of the inner thread, if exists. The default is
    /// 60 seconds.
    pub fn thread_pool_recv_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.pool_recv_timeout = timeout;
        self
    }

    /// Build [`Runtime`].
    pub fn build(&self) -> io::Result<Runtime> {
        Runtime::with_builder(self)
    }

    fn build_driver(&self) -> io::Result<Driver> {
        Driver::new(self.io_queue_capacity)
    }
}

/// Spawns a new asynchronous task, returning a [`Task`] for it.
///
/// Spawning a task enables the task to execute concurrently to other tasks.
/// There is no guarantee that a spawned task will execute to completion.
///
/// ## Panics
///
/// This method doesn't create runtime. It tries to obtain the current runtime
/// by [`Runtime::with_current`].
pub fn spawn<F: Future + 'static>(future: F) -> Task<F::Output> {
    Runtime::with_current(|r| r.spawn(future))
}

/// Spawns a blocking task in a new thread, and wait for it.
///
/// The task will not be cancelled even if the future is dropped.
///
/// ## Panics
///
/// This method doesn't create runtime. It tries to obtain the current runtime
/// by [`Runtime::with_current`].
pub fn spawn_blocking<T: Send + 'static>(
    f: impl (FnOnce() -> T) + Send + 'static,
) -> JoinHandle<T> {
    Runtime::with_current(|r| r.spawn_blocking(f))
}
