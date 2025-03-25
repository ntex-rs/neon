use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::{cell::Cell, cell::RefCell, io, rc::Rc, sync::Arc};
use std::{num::NonZeroUsize, time::Duration};

pub use polling::Event;
use polling::{Events, Poller};

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
    drv.changes.borrow_mut().push(Change::Blocking(f));
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
    changes: Rc<RefCell<Vec<Change>>>,
}

impl DriverApi {
    /// Attach an fd to the driver.
    ///
    /// `fd` must be attached to the driver before using register/unregister
    /// methods.
    pub fn attach(&self, fd: RawFd, id: u32, event: Option<Event>) {
        let user_data = id as u64 | self.batch;

        let event = event
            .map(|mut ev| {
                ev.key = user_data as usize;
                ev
            })
            .unwrap_or_else(|| Event::new(user_data as usize, false, false));

        if let Err(err) = unsafe { self.poll.add(fd, event) } {
            self.changes.borrow_mut().push(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }

    /// Detach an fd from the driver.
    pub fn detach(&self, fd: RawFd, id: u32) {
        if let Err(err) = self.poll.delete(unsafe { BorrowedFd::borrow_raw(fd) }) {
            self.changes.borrow_mut().push(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }

    /// Register interest for specified file descriptor.
    pub fn modify(&self, fd: RawFd, id: u32, mut event: Event) {
        log::debug!("Register event {:?} for {:?} id: {:?}", event, fd, id);

        let user_data = id as u64 | self.batch;
        event.key = user_data as usize;

        let result = self
            .poll
            .modify(unsafe { BorrowedFd::borrow_raw(fd) }, event);
        if let Err(err) = result {
            self.changes.borrow_mut().push(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }
}

/// Low-level driver of polling.
pub struct Driver {
    poll: Arc<Poller>,
    events: RefCell<Events>,
    hid: Cell<u64>,
    changes: Rc<RefCell<Vec<Change>>>,
    handlers: Cell<Option<Box<Vec<Box<dyn Handler>>>>>,
}

impl Driver {
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    pub fn new(capacity: u32) -> io::Result<Self> {
        log::trace!("New poll driver");

        let events = if capacity == 0 {
            Events::new()
        } else {
            Events::with_capacity(NonZeroUsize::new(capacity as usize).unwrap())
        };

        Ok(Self {
            poll: Arc::new(Poller::new()?),
            events: RefCell::new(events),
            hid: Cell::new(0),
            changes: Rc::new(RefCell::new(Vec::with_capacity(16))),
            handlers: Cell::new(Some(Box::new(Vec::default()))),
        })
    }

    /// Driver type
    pub const fn tp(&self) -> crate::driver::DriverType {
        crate::driver::DriverType::Poll
    }

    /// Register updates handler
    pub fn register<F>(&self, f: F)
    where
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self.handlers.take().unwrap_or_default();

        let api = DriverApi {
            id: id as usize,
            batch: id << Self::BATCH,
            changes: self.changes.clone(),
            poll: self.poll.clone(),
        };
        handlers.push(f(api));
        self.hid.set(id + 1);
        self.handlers.set(Some(handlers));
    }

    /// Poll the driver and handle completed entries.
    pub fn poll(&self, wait_events: bool) -> io::Result<()> {
        let mut events = self.events.borrow_mut();
        let timeout = if wait_events {
            None
        } else {
            Some(Duration::ZERO)
        };
        self.poll.wait(&mut events, timeout)?;

        if !events.is_empty() {
            // route events to handlers
            let mut handlers = self.handlers.take().unwrap();
            for event in events.iter() {
                let key = event.key as u64;
                let batch = ((key & Self::BATCH_MASK) >> Self::BATCH) as usize;
                let user_data = (key & Self::DATA_MASK) as usize;

                log::debug!("Event {:?} for {}:{}", event.key as RawFd, batch, user_data);
                handlers[batch].event(user_data, event)
            }

            // apply errors
            let mut changes = self.changes.borrow_mut();
            if !changes.is_empty() {
                log::debug!("Apply driver errors, {:?}", changes.len());
                for op in changes.drain(..) {
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

            self.handlers.set(Some(handlers));
        }

        Ok(())
    }

    /// Get notification handle
    pub fn handle(&self) -> NotifyHandle {
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
pub struct NotifyHandle {
    poll: Arc<Poller>,
}

impl NotifyHandle {
    fn new(poll: Arc<Poller>) -> Self {
        Self { poll }
    }

    /// Notify the driver
    pub fn notify(&self) -> io::Result<()> {
        self.poll.notify()
    }
}
