//#![allow(clippy::type_complexity)]

use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::{cell::Cell, cell::RefCell, io, rc::Rc, sync::Arc};
use std::{num::NonZeroUsize, time::Duration};

pub use polling::Event;
use polling::{Events, Poller};

use crate::pool::Dispatchable;

pub trait Handler {
    /// Submitted interest
    fn event(&mut self, id: usize, event: Event);

    /// Operation submission has failed
    fn error(&mut self, id: usize, err: io::Error);

    /// All events are processed, process all updates
    fn commit(&mut self);
}

enum Change {
    Attach {
        fd: RawFd,
        event: Event,
        user_data: u64,
    },
    Detach {
        fd: RawFd,
        batch: u64,
        user_data: u32,
    },
    Modify {
        fd: RawFd,
        user_data: u64,
        event: Event,
    },
    Blocking(Box<dyn Dispatchable + Send>),
}

pub struct DriverApi {
    batch: u64,
    changes: Rc<RefCell<Vec<Change>>>,
}

impl DriverApi {
    /// Attach an fd to the driver.
    ///
    /// `fd` must be attached to the driver before using register/unregister
    /// methods.
    pub fn attach(&self, fd: RawFd, user_data: u32, event: Option<Event>) {
        let user_data = user_data as u64 | self.batch;

        let event = event
            .map(|mut ev| {
                ev.key = user_data as usize;
                ev
            })
            .unwrap_or_else(|| Event::new(user_data as usize, false, false));

        self.changes.borrow_mut().push(Change::Attach {
            fd,
            event,
            user_data,
        });
    }

    /// Detach an fd from the driver.
    pub fn detach(&self, fd: RawFd, user_data: u32) {
        self.changes.borrow_mut().push(Change::Detach {
            fd,
            user_data,
            batch: self.batch,
        });
    }

    /// Register interest for specified file descriptor.
    pub fn modify(&self, fd: RawFd, user_data: u32, mut event: Event) {
        log::debug!(
            "Register event {:?} for {:?} user-data: {:?}",
            event,
            fd,
            user_data
        );
        let user_data = user_data as u64 | self.batch;
        event.key = user_data as usize;
        self.changes.borrow_mut().push(Change::Modify {
            fd,
            event,
            user_data,
        });
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
            batch: id << Self::BATCH,
            changes: self.changes.clone(),
        };
        handlers.push(f(api));
        self.hid.set(id + 1);
        self.handlers.set(Some(handlers));
    }

    /// Poll the driver and handle completed entries.
    pub fn poll<F: FnOnce()>(&self, timeout: Option<Duration>, f: F) -> io::Result<()> {
        let mut events = self.events.borrow_mut();
        self.poll.wait(&mut events, timeout)?;

        if events.is_empty() {
            if timeout.is_some() && timeout != Some(Duration::ZERO) {
                return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
            }
        } else {
            let mut handlers = self.handlers.take().unwrap();
            for event in events.iter() {
                let key = event.key as u64;
                let batch = ((key & Self::BATCH_MASK) >> Self::BATCH) as usize;
                let user_data = (key & Self::DATA_MASK) as usize;

                log::debug!("Event {:?} for {}:{}", event.key as RawFd, batch, user_data);

                handlers[batch].event(user_data, event)
            }
            self.handlers.set(Some(handlers));
        }

        // apply changes
        self.apply_changes();

        // complete batch handling
        let mut handlers = self.handlers.take().unwrap();
        for handler in handlers.iter_mut() {
            handler.commit();
        }
        self.handlers.set(Some(handlers));
        self.apply_changes();

        // run user function
        f();

        // check if we have more changes from "run"
        self.apply_changes();

        Ok(())
    }

    /// Re-calc driver changes
    fn apply_changes(&self) {
        let mut changes = self.changes.borrow_mut();
        if changes.is_empty() {
            return;
        }
        log::debug!("Apply driver changes, {:?}", changes.len());

        let mut handlers = self.handlers.take().unwrap();

        for change in changes.drain(..) {
            match change {
                Change::Attach {
                    fd,
                    event,
                    user_data,
                } => {
                    if let Err(err) = unsafe { self.poll.add(fd, event) } {
                        let batch =
                            ((user_data & Self::BATCH_MASK) >> Self::BATCH) as usize;
                        let user_data = (user_data & Self::DATA_MASK) as usize;
                        handlers[batch].error(user_data, err);
                    }
                }
                Change::Detach {
                    fd,
                    batch,
                    user_data,
                } => {
                    if let Err(err) =
                        self.poll.delete(unsafe { BorrowedFd::borrow_raw(fd) })
                    {
                        handlers[(batch >> Self::BATCH) as usize]
                            .error(user_data as usize, err);
                    }
                }
                Change::Modify {
                    fd,
                    event,
                    user_data,
                } => {
                    let result = self
                        .poll
                        .modify(unsafe { BorrowedFd::borrow_raw(fd) }, event);
                    if let Err(err) = result {
                        let batch =
                            ((user_data & Self::BATCH_MASK) >> Self::BATCH) as usize;
                        let user_data = (user_data & Self::DATA_MASK) as usize;
                        handlers[batch].error(user_data, err);
                    }
                }
                Change::Blocking(f) => {
                    crate::Runtime::with_current(|rt| rt.schedule_blocking(f));
                }
            }
        }

        self.handlers.set(Some(handlers));
    }

    pub(crate) fn push_blocking(
        &self,
        _: &crate::Runtime,
        f: Box<dyn Dispatchable + Send>,
    ) {
        self.changes.borrow_mut().push(Change::Blocking(f));
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
