use std::cell::{Cell, UnsafeCell};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::{collections::VecDeque, num::NonZeroUsize, time::Duration};
use std::{io, rc::Rc, sync::Arc};

pub use ntex_polling::{Event, PollMode};
use ntex_polling::{Events, Poller};

pub trait Handler {
    /// Submitted interest
    fn event(&mut self, id: usize, event: Event);

    /// Operation submission has failed
    fn error(&mut self, id: usize, err: io::Error);
}

pub(crate) fn spawn_blocking(
    _: &crate::Runtime,
    drv: &Driver,
    f: Box<dyn crate::pool::Dispatchable + Send>,
) {
    unsafe { (*drv.changes.get()).push_back(Change::Blocking(f)) }
}

enum Change {
    Error {
        batch: usize,
        user_data: u32,
        error: io::Error,
    },
    Blocking(Box<dyn crate::pool::Dispatchable + Send>),
}

pub struct DriverApi {
    id: usize,
    batch: u64,
    poll: Arc<Poller>,
    changes: Rc<UnsafeCell<VecDeque<Change>>>,
}

impl DriverApi {
    /// Attach an fd to the driver.
    ///
    /// `fd` must be attached to the driver before using register/unregister
    /// methods.
    pub fn attach(&self, fd: RawFd, id: u32, mut event: Event, mode: PollMode) {
        event.key = (id as u64 | self.batch) as usize;
        if let Err(err) = unsafe { self.poll.add_with_mode(fd, event, mode) } {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }

    /// Detach an fd from the driver.
    pub fn detach(&self, fd: RawFd, id: u32) {
        if let Err(err) = self.poll.delete(unsafe { BorrowedFd::borrow_raw(fd) }) {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }

    /// Register interest for specified file descriptor.
    pub fn modify(&self, fd: RawFd, id: u32, mut event: Event, mode: PollMode) {
        event.key = (id as u64 | self.batch) as usize;

        let result =
            self.poll
                .modify_with_mode(unsafe { BorrowedFd::borrow_raw(fd) }, event, mode);
        if let Err(err) = result {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }

    fn change(&self, ev: Change) {
        unsafe { (*self.changes.get()).push_back(ev) };
    }
}

/// Low-level driver of polling.
pub(crate) struct Driver {
    poll: Arc<Poller>,
    capacity: usize,
    changes: Rc<UnsafeCell<VecDeque<Change>>>,
    hid: Cell<u64>,
    handlers: Cell<Option<Box<Vec<Box<dyn Handler>>>>>,
}

impl Driver {
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    pub(crate) fn new(capacity: u32) -> io::Result<Self> {
        log::trace!("New poll driver");

        Ok(Self {
            hid: Cell::new(0),
            poll: Arc::new(Poller::new()?),
            capacity: capacity as usize,
            changes: Rc::new(UnsafeCell::new(VecDeque::with_capacity(32))),
            handlers: Cell::new(Some(Box::new(Vec::default()))),
        })
    }

    /// Driver type
    pub(crate) const fn tp(&self) -> crate::driver::DriverType {
        crate::driver::DriverType::Poll
    }

    /// Register updates handler
    pub(crate) fn register<F>(&self, f: F)
    where
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self.handlers.take().unwrap_or_default();

        let api = DriverApi {
            id: id as usize,
            batch: id << Self::BATCH,
            poll: self.poll.clone(),
            changes: self.changes.clone(),
        };
        handlers.push(f(api));
        self.hid.set(id + 1);
        self.handlers.set(Some(handlers));
    }

    /// Poll the driver and handle completed entries.
    pub(crate) fn poll<T, F>(&self, mut f: F) -> io::Result<T>
    where
        F: FnMut() -> super::PollResult<T>,
    {
        let mut events = if self.capacity == 0 {
            Events::new()
        } else {
            Events::with_capacity(NonZeroUsize::new(self.capacity).unwrap())
        };

        loop {
            let timeout = match f() {
                super::PollResult::Pending => None,
                super::PollResult::HasTasks => Some(Duration::ZERO),
                super::PollResult::Ready(val) => return Ok(val),
            };

            let has_changes = !unsafe { (*self.changes.get()).is_empty() };
            if has_changes {
                let mut handlers = self.handlers.take().unwrap();
                self.apply_changes(&mut handlers);
                self.handlers.set(Some(handlers));
            }

            self.poll.wait(&mut events, timeout)?;

            let mut handlers = self.handlers.take().unwrap();
            for event in events.iter() {
                let key = event.key as u64;
                let batch = ((key & Self::BATCH_MASK) >> Self::BATCH) as usize;
                handlers[batch].event((key & Self::DATA_MASK) as usize, event)
            }
            self.apply_changes(&mut handlers);
            self.handlers.set(Some(handlers));
        }
    }

    fn apply_changes(&self, handlers: &mut [Box<dyn Handler>]) {
        while let Some(op) = unsafe { (*self.changes.get()).pop_front() } {
            match op {
                Change::Error {
                    batch,
                    user_data,
                    error,
                } => handlers[batch].error(user_data as usize, error),
                Change::Blocking(f) => {
                    let _ = crate::Runtime::with_current(|rt| rt.pool.dispatch(f));
                }
            }
        }
    }

    /// Get notification handle
    pub(crate) fn handle(&self) -> NotifyHandle {
        NotifyHandle::new(self.poll.clone())
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.poll.as_raw_fd()
    }
}

#[derive(Clone, Debug)]
/// A notify handle to the inner driver.
pub(crate) struct NotifyHandle {
    poll: Arc<Poller>,
}

impl NotifyHandle {
    fn new(poll: Arc<Poller>) -> Self {
        Self { poll }
    }

    /// Notify the driver
    pub(crate) fn notify(&self) -> io::Result<()> {
        self.poll.notify()
    }
}
