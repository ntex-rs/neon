use std::cell::{Cell, RefCell};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{collections::VecDeque, io, mem, rc::Rc, sync::Arc, time::Duration};

use io_uring::cqueue::{more, Entry as CEntry};
use io_uring::opcode::{AsyncCancel, PollAdd};
use io_uring::squeue::Entry as SEntry;
use io_uring::types::{Fd, SubmitArgs, Timespec};
use io_uring::{IoUring, Probe};

pub trait Handler {
    /// Operation is completed
    fn completed(&mut self, id: usize, flags: u32, result: io::Result<i32>);

    fn canceled(&mut self, id: usize);
}

#[derive(Debug)]
enum Change {
    Submit { entry: SEntry },
    Cancel { op_id: u64 },
}

pub struct DriverApi {
    batch: u64,
    probe: Rc<Probe>,
    changes: Rc<RefCell<VecDeque<Change>>>,
}

impl DriverApi {
    /// Submit request to the driver.
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

    /// Attempt to cancel an already issued request.
    pub fn cancel(&self, user_data: u32) {
        log::debug!(
            "Cancel operation batch: {:?} user-data: {:?}",
            self.batch >> Driver::BATCH,
            user_data
        );
        self.changes.borrow_mut().push_back(Change::Cancel {
            op_id: user_data as u64 | self.batch,
        });
    }

    /// Get whether a specific io-uring opcode is supported.
    pub fn is_supported(&self, opcode: u8) -> bool {
        self.probe.is_supported(opcode)
    }
}

/// Low-level driver of io-uring.
pub struct Driver {
    ring: RefCell<IoUring<SEntry, CEntry>>,
    notifier: Notifier,

    hid: Cell<u64>,
    probe: Rc<Probe>,
    changes: Rc<RefCell<VecDeque<Change>>>,
    handlers: Cell<Option<Box<Vec<Box<dyn Handler>>>>>,
}

impl Driver {
    const NOTIFY: u64 = u64::MAX;
    const CANCEL: u64 = u64::MAX - 1;
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    /// Create io-uring driver
    pub fn new(capacity: u32) -> io::Result<Self> {
        log::trace!("New io-uring driver");

        #[allow(unused_mut)]
        let mut builder = IoUring::builder();
        #[cfg(not(feature = "io-uring-compat"))]
        let builder = builder.setup_coop_taskrun().setup_single_issuer();

        let mut ring = builder.build(capacity)?;
        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe)?;

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
            probe: Rc::new(probe),
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
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self.handlers.take().unwrap_or_default();

        let api = DriverApi {
            batch: id << Self::BATCH,
            probe: self.probe.clone(),
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
    pub fn poll(&self, wait_events: bool) -> io::Result<()> {
        let has_more = self.apply_changes();
        let poll_result = self.poll_completions();

        if !poll_result || has_more {
            if has_more {
                self.submit_auto(Some(Duration::ZERO))?;
            } else {
                let timeout = if wait_events {
                    None
                } else {
                    Some(Duration::ZERO)
                };
                self.submit_auto(timeout)?;
            }
            self.poll_completions();
        }

        Ok(())
    }

    fn poll_completions(&self) -> bool {
        let mut ring = self.ring.borrow_mut();
        let mut cqueue = ring.completion();
        cqueue.sync();
        if cqueue.is_empty() {
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
                            Err(io::Error::from_raw_os_error(-result))
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

    /// Get notification handle for this driver
    pub fn handle(&self) -> NotifyHandle {
        self.notifier.handle()
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.ring.borrow().as_raw_fd()
    }
}

#[derive(Debug)]
pub(crate) struct Notifier {
    fd: Arc<OwnedFd>,
}

impl Notifier {
    /// Create a new notifier.
    pub(crate) fn new() -> io::Result<Self> {
        let fd = crate::syscall!(libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { fd: Arc::new(fd) })
    }

    pub(crate) fn clear(&self) -> io::Result<()> {
        loop {
            let mut buffer = [0u64];
            let res = crate::syscall!(libc::read(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                mem::size_of::<u64>()
            ));
            match res {
                Ok(len) => {
                    debug_assert_eq!(len, mem::size_of::<u64>() as _);
                    break Ok(());
                }
                // Clear the next time:)
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Ok(()),
                // Just like read_exact
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break Err(e),
            }
        }
    }

    pub(crate) fn handle(&self) -> NotifyHandle {
        NotifyHandle::new(self.fd.clone())
    }
}

impl AsRawFd for Notifier {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[derive(Clone)]
/// A notify handle to the driver.
pub struct NotifyHandle {
    fd: Arc<OwnedFd>,
}

impl NotifyHandle {
    pub(crate) fn new(fd: Arc<OwnedFd>) -> Self {
        Self { fd }
    }

    /// Notify the driver.
    pub fn notify(&self) -> io::Result<()> {
        let data = 1u64;
        crate::syscall!(libc::write(
            self.fd.as_raw_fd(),
            &data as *const _ as *const _,
            std::mem::size_of::<u64>(),
        ))?;
        Ok(())
    }
}
