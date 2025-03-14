#![allow(clippy::type_complexity)]

use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::{cell::Cell, cell::RefCell, io, rc::Rc, sync::Arc};
use std::{num::NonZeroUsize, time::Duration};

use nohash_hasher::IntMap;
use polling::{Event, Events, Poller};

use crate::pool::Dispatchable;

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const NEW     = 0b0000_0001;
        const CHANGED = 0b0000_0010;
    }
}

/// The interest to poll a file descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    /// Represents a read operation.
    Readable,
    /// Represents a write operation.
    Writable,
}

pub trait Handler {
    /// Submitted interest
    fn readable(&mut self, id: usize);

    /// Submitted interest
    fn writable(&mut self, id: usize);

    /// Operation submission has failed
    fn error(&mut self, id: usize, err: io::Error);

    /// All events are processed, process all updates
    fn commit(&mut self);
}

#[derive(Debug)]
struct FdItem {
    flags: Flags,
    batch: usize,
    read: Option<usize>,
    write: Option<usize>,
}

impl FdItem {
    fn new(batch: usize) -> Self {
        Self {
            batch,
            read: None,
            write: None,
            flags: Flags::NEW,
        }
    }

    fn register(&mut self, user_data: usize, interest: Interest) {
        self.flags.insert(Flags::CHANGED);
        match interest {
            Interest::Readable => {
                self.read = Some(user_data);
            }
            Interest::Writable => {
                self.write = Some(user_data);
            }
        }
    }

    fn unregister(&mut self, int: Interest) {
        let res = match int {
            Interest::Readable => self.read.take(),
            Interest::Writable => self.write.take(),
        };
        if res.is_some() {
            self.flags.insert(Flags::CHANGED);
        }
    }

    fn unregister_all(&mut self) {
        if self.read.is_some() || self.write.is_some() {
            self.flags.insert(Flags::CHANGED);
        }

        let _ = self.read.take();
        let _ = self.write.take();
    }

    fn user_data(&mut self, interest: Interest) -> Option<usize> {
        match interest {
            Interest::Readable => self.read,
            Interest::Writable => self.write,
        }
    }

    fn event(&self, key: usize) -> Event {
        let mut event = Event::none(key);
        if self.read.is_some() {
            event.readable = true;
        }
        if self.write.is_some() {
            event.writable = true;
        }
        event
    }
}

enum Change {
    Register {
        fd: RawFd,
        batch: usize,
        user_data: usize,
        int: Interest,
    },
    Unregister {
        fd: RawFd,
        batch: usize,
        int: Interest,
    },
    UnregisterAll {
        fd: RawFd,
        batch: usize,
    },
    Blocking(Box<dyn Dispatchable + Send>),
}

pub struct DriverApi {
    batch: usize,
    changes: Rc<RefCell<Vec<Change>>>,
}

impl DriverApi {
    /// Register interest for specified file descriptor.
    pub fn register(&self, fd: RawFd, user_data: usize, int: Interest) {
        log::debug!(
            "Register interest {:?} for {:?} user-data: {:?}",
            int,
            fd,
            user_data
        );
        self.change(Change::Register {
            fd,
            batch: self.batch,
            user_data,
            int,
        });
    }

    /// Unregister interest for specified file descriptor.
    pub fn unregister(&self, fd: RawFd, int: Interest) {
        log::debug!(
            "Unregister interest {:?} for {:?} batch: {:?}",
            int,
            fd,
            self.batch
        );
        self.change(Change::Unregister {
            fd,
            batch: self.batch,
            int,
        });
    }

    /// Unregister all interests.
    pub fn unregister_all(&self, fd: RawFd) {
        self.change(Change::UnregisterAll {
            fd,
            batch: self.batch,
        });
    }

    fn change(&self, change: Change) {
        self.changes.borrow_mut().push(change);
    }
}

/// Low-level driver of polling.
pub struct Driver {
    poll: Arc<Poller>,
    events: RefCell<Events>,
    registry: RefCell<IntMap<RawFd, FdItem>>,
    hid: Cell<usize>,
    changes: Rc<RefCell<Vec<Change>>>,
    handlers: Cell<Option<Box<Vec<Box<dyn Handler>>>>>,
}

impl Driver {
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
            registry: RefCell::new(Default::default()),
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
            batch: id,
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
            let mut registry = self.registry.borrow_mut();
            let mut handlers = self.handlers.take().unwrap();
            for event in events.iter() {
                let fd = event.key as RawFd;
                log::debug!("Event {:?} for {:?}", event, registry.get(&fd));

                if let Some(item) = registry.get_mut(&fd) {
                    if event.readable {
                        if let Some(user_data) = item.user_data(Interest::Readable) {
                            handlers[item.batch].readable(user_data)
                        }
                    }
                    if event.writable {
                        if let Some(user_data) = item.user_data(Interest::Writable) {
                            handlers[item.batch].writable(user_data)
                        }
                    }
                }
            }
            self.handlers.set(Some(handlers));
        }

        // apply changes
        self.apply_changes()?;

        // complete batch handling
        let mut handlers = self.handlers.take().unwrap();
        for handler in handlers.iter_mut() {
            handler.commit();
        }
        self.handlers.set(Some(handlers));
        self.apply_changes()?;

        // run user function
        f();

        // check if we have more changes from "run"
        self.apply_changes()?;

        Ok(())
    }

    /// Re-calc driver changes
    fn apply_changes(&self) -> io::Result<()> {
        let mut changes = self.changes.borrow_mut();
        if changes.is_empty() {
            return Ok(());
        }
        log::debug!("Apply driver changes, {:?}", changes.len());

        let mut registry = self.registry.borrow_mut();

        for change in &mut *changes {
            match change {
                Change::Register {
                    fd,
                    batch,
                    user_data,
                    int,
                } => {
                    let item = registry.entry(*fd).or_insert_with(|| FdItem::new(*batch));
                    item.register(*user_data, *int);
                }
                Change::Unregister { fd, batch, int } => {
                    let item = registry.entry(*fd).or_insert_with(|| FdItem::new(*batch));
                    item.unregister(*int);
                }
                Change::UnregisterAll { fd, batch } => {
                    let item = registry.entry(*fd).or_insert_with(|| FdItem::new(*batch));
                    item.unregister_all();
                }
                _ => {}
            }
        }

        for change in changes.drain(..) {
            let fd = match change {
                Change::Register { fd, .. } => Some(fd),
                Change::Unregister { fd, .. } => Some(fd),
                Change::UnregisterAll { fd, .. } => Some(fd),
                Change::Blocking(f) => {
                    crate::Runtime::with_current(|rt| rt.schedule_blocking(f));
                    None
                }
            };

            if let Some(fd) = fd {
                if let Some(item) = registry.get_mut(&fd) {
                    if item.flags.contains(Flags::CHANGED) {
                        item.flags.remove(Flags::CHANGED);

                        let new = item.flags.contains(Flags::NEW);
                        let renew_event = item.event(fd as usize);

                        if !renew_event.readable && !renew_event.writable {
                            registry.remove(&fd);
                            if !new {
                                self.poll.delete(unsafe { BorrowedFd::borrow_raw(fd) })?;
                            }
                        } else if new {
                            item.flags.remove(Flags::NEW);
                            unsafe { self.poll.add(fd, renew_event)? };
                        } else {
                            self.poll.modify(
                                unsafe { BorrowedFd::borrow_raw(fd) },
                                renew_event,
                            )?;
                        }
                    }
                }
            }
        }

        Ok(())
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

impl Drop for Driver {
    fn drop(&mut self) {
        for fd in self.registry.borrow().keys() {
            unsafe {
                let fd = BorrowedFd::borrow_raw(*fd);
                self.poll.delete(fd).ok();
            }
        }
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
