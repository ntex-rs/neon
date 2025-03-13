pub use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use std::{cell::Cell, cell::RefCell, collections::VecDeque, io, rc::Rc, time::Duration};

use io_uring::cqueue::{more, Entry as CEntry};
use io_uring::opcode::{AsyncCancel, PollAdd};
use io_uring::squeue::Entry as SEntry;
use io_uring::types::{Fd, SubmitArgs, Timespec};
use io_uring::IoUring;

pub mod op;

mod notify;
use self::notify::Notifier;
pub use self::notify::NotifyHandle;

#[derive(Debug)]
enum Change {
    Submit { entry: SEntry },
    Cancel { op_id: u64 },
}

pub struct DriverApi {
    batch: u64,
    changes: Rc<RefCell<VecDeque<Change>>>,
}

impl DriverApi {
    pub fn submit(&self, user_data: u32, entry: SEntry) {
        log::debug!(
            "Submit operation batch: {:?} user-data: {:?} entry: {:?}",
            self.batch >> Driver::BATCH,
            user_data,
            entry,
        );
        self.changes.borrow_mut().push_back(Change::Submit {
            entry: entry.user_data(user_data as u64 | self.batch),
        });
    }

    pub fn cancel(&self, op_id: u32) {
        log::debug!(
            "Cancel operation batch: {:?} user-data: {:?}",
            self.batch >> Driver::BATCH,
            op_id
        );
        self.changes.borrow_mut().push_back(Change::Cancel {
            op_id: op_id as u64 | self.batch,
        });
    }
}

/// Low-level driver of io-uring.
pub struct Driver {
    ring: RefCell<IoUring<SEntry, CEntry>>,
    notifier: Notifier,

    hid: Cell<u64>,
    changes: Rc<RefCell<VecDeque<Change>>>,
    handlers: Cell<Option<Box<Vec<Box<dyn op::Handler>>>>>,
}

impl Driver {
    const NOTIFY: u64 = u64::MAX;
    const CANCEL: u64 = u64::MAX - 1;
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    pub fn new(capacity: u32) -> io::Result<Self> {
        log::trace!("New io-uring driver");

        let mut ring = IoUring::builder()
            .setup_coop_taskrun()
            .setup_single_issuer()
            .build(capacity)?;

        let notifier = Notifier::new()?;

        #[allow(clippy::useless_conversion)]
        unsafe {
            ring.submission()
                .push(
                    &PollAdd::new(Fd(notifier.as_raw_fd()), libc::POLLIN as _)
                        .multi(true)
                        .build()
                        .user_data(Self::NOTIFY)
                        .into(),
                )
                .expect("the squeue sould not be full");
        }
        Ok(Self {
            notifier,
            ring: RefCell::new(ring),
            hid: Cell::new(0),
            changes: Rc::new(RefCell::new(VecDeque::new())),
            handlers: Cell::new(Some(Box::new(Vec::new()))),
        })
    }

    /// Driver type
    pub const fn tp(&self) -> crate::driver::DriverType {
        crate::driver::DriverType::IoUring
    }

    /// Register updates handler
    pub fn register<F>(&self, f: F)
    where
        F: FnOnce(DriverApi) -> Box<dyn self::op::Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self.handlers.take().unwrap_or_default();

        let api = DriverApi {
            batch: id << 48,
            changes: self.changes.clone(),
        };
        handlers.push(f(api));
        self.hid.set(id + 1);
        self.handlers.set(Some(handlers));
    }

    // Auto means that it choose to wait or not automatically.
    fn submit_auto(&self, timeout: Option<Duration>) -> io::Result<()> {
        let mut ring = self.ring.borrow_mut();
        let res = {
            // Last part of submission queue, wait till timeout.
            if let Some(duration) = timeout {
                let timespec = Timespec::new()
                    .sec(duration.as_secs())
                    .nsec(duration.subsec_nanos());
                let args = SubmitArgs::new().timespec(&timespec);
                ring.submitter().submit_with_args(1, &args)
            } else {
                ring.submit_and_wait(1)
            }
        };
        match res {
            Ok(_) => {
                // log::debug!("Submit result: {res:?} {:?}", timeout);
                if ring.completion().is_empty() {
                    Err(io::ErrorKind::TimedOut.into())
                } else {
                    Ok(())
                }
            }
            Err(e) => match e.raw_os_error() {
                Some(libc::ETIME) => {
                    if timeout.is_some() && timeout != Some(Duration::ZERO) {
                        Err(io::ErrorKind::TimedOut.into())
                    } else {
                        Ok(())
                    }
                }
                Some(libc::EBUSY) | Some(libc::EAGAIN) => {
                    Err(io::ErrorKind::Interrupted.into())
                }
                _ => Err(e),
            },
        }
    }

    fn apply_changes(&self) -> bool {
        let mut changes = self.changes.borrow_mut();
        if changes.is_empty() {
            return false;
        }
        log::debug!("Apply changes, {:?}", changes.len());

        let mut ring = self.ring.borrow_mut();
        let mut squeue = ring.submission();

        while let Some(change) = changes.pop_front() {
            match change {
                Change::Submit { entry } => {
                    if unsafe { squeue.push(&entry) }.is_err() {
                        changes.push_front(Change::Submit { entry });
                        break;
                    }
                }
                Change::Cancel { op_id } => {
                    let entry = AsyncCancel::new(op_id).build().user_data(Self::CANCEL);
                    if unsafe { squeue.push(&entry) }.is_err() {
                        changes.push_front(Change::Cancel { op_id });
                        break;
                    }
                }
            }
        }
        squeue.sync();

        !changes.is_empty()
    }

    /// Poll the driver and handle completed operations.
    pub fn poll<F: FnOnce()>(&self, timeout: Option<Duration>, f: F) -> io::Result<()> {
        let has_more = self.apply_changes();
        let poll_result = self.poll_completions();

        if !poll_result || has_more {
            if has_more {
                self.submit_auto(Some(Duration::ZERO))?;
            } else {
                self.submit_auto(timeout)?;
            }
            self.poll_completions();
        }

        f();

        Ok(())
    }

    fn poll_completions(&self) -> bool {
        let mut ring = self.ring.borrow_mut();
        let mut cqueue = ring.completion();
        cqueue.sync();
        let has_entry = !cqueue.is_empty();
        if !has_entry {
            return false;
        }
        let mut handlers = self.handlers.take().unwrap();
        for entry in cqueue {
            let user_data = entry.user_data();
            match user_data {
                Self::CANCEL => {}
                Self::NOTIFY => {
                    let flags = entry.flags();
                    debug_assert!(more(flags));
                    self.notifier.clear().expect("cannot clear notifier");
                }
                _ => {
                    let batch = ((user_data & Self::BATCH_MASK) >> Self::BATCH) as usize;
                    let user_data = (user_data & Self::DATA_MASK) as usize;

                    let result = entry.result();

                    if result == -libc::ECANCELED {
                        handlers[batch].canceled(user_data);
                    } else {
                        let result = if result < 0 {
                            Err(io::Error::from_raw_os_error(result))
                        } else {
                            Ok(result as _)
                        };
                        handlers[batch].completed(user_data, entry.flags(), result);
                    }
                }
            }
        }
        self.handlers.set(Some(handlers));
        true
    }

    pub fn handle(&self) -> NotifyHandle {
        self.notifier.handle()
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.ring.borrow().as_raw_fd()
    }
}
