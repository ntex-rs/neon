use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{cell::Cell, cell::RefCell, collections::VecDeque, io, mem, rc::Rc, sync::Arc};

use io_uring::cqueue::{more, Entry as CEntry};
use io_uring::opcode::{AsyncCancel, PollAdd};
use io_uring::{squeue::Entry as SEntry, types::Fd, IoUring, Probe};

use crate::pool::Dispatchable;

pub trait Handler {
    /// Operation is completed
    fn completed(&mut self, id: usize, flags: u32, result: io::Result<i32>);

    /// Operation is canceled
    fn canceled(&mut self, id: usize);
}

#[derive(Debug)]
enum Change {
    Submit { entry: SEntry },
    Cancel { op_id: u64 },
}

#[inline(always)]
pub(crate) fn spawn_blocking(rt: &crate::Runtime, _: &Driver, f: Box<dyn Dispatchable + Send>) {
    let _ = rt.pool.dispatch(f);
}

pub struct DriverApi {
    id: usize,
    batch: u64,
    probe: Rc<Probe>,
    changes: Rc<RefCell<VecDeque<Change>>>,
}

impl DriverApi {
    #[inline]
    /// Submit request to the driver.
    pub fn submit(&self, id: u32, entry: SEntry) {
        log::debug!(
            "Submit batch({:?}) id({:?}) entry: {:?}",
            self.id,
            id,
            entry
        );
        self.changes.borrow_mut().push_back(Change::Submit {
            entry: entry.user_data(id as u64 | self.batch),
        });
    }

    #[inline]
    /// Attempt to cancel an already issued request.
    pub fn cancel(&self, id: u32) {
        log::debug!("Cancel op batch({:?}) id: {:?}", self.id, id);
        self.changes.borrow_mut().push_back(Change::Cancel {
            op_id: id as u64 | self.batch,
        });
    }

    /// Get whether a specific io-uring opcode is supported.
    pub fn is_supported(&self, opcode: u8) -> bool {
        self.probe.is_supported(opcode)
    }
}

/// Low-level driver of io-uring.
pub struct Driver {
    fd: RawFd,
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
    pub(crate) fn new(capacity: u32) -> io::Result<Self> {
        log::trace!("New io-uring driver");

        #[allow(unused_mut)]
        let mut builder = IoUring::builder();
        #[cfg(not(feature = "io-uring-compat"))]
        let builder = builder.setup_coop_taskrun().setup_single_issuer();

        let mut ring = builder.build(capacity)?;
        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe)?;

        let notifier = Notifier::new()?;
        unsafe {
            ring.submission()
                .push(
                    &PollAdd::new(Fd(notifier.as_raw_fd()), libc::POLLIN as _)
                        .multi(true)
                        .build()
                        .user_data(Self::NOTIFY),
                )
                .expect("the squeue sould not be full");
        }
        Ok(Self {
            notifier,
            fd: ring.as_raw_fd(),
            ring: RefCell::new(ring),
            probe: Rc::new(probe),
            hid: Cell::new(0),
            changes: Rc::new(RefCell::new(VecDeque::new())),
            handlers: Cell::new(Some(Box::new(Vec::new()))),
        })
    }

    /// Driver type
    pub(crate) const fn tp(&self) -> crate::driver::DriverType {
        crate::driver::DriverType::IoUring
    }

    /// Register updates handler
    pub fn register<F>(&self, f: F)
    where
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self.handlers.take().unwrap_or_default();
        handlers.push(f(DriverApi {
            id: id as usize,
            batch: id << Self::BATCH,
            probe: self.probe.clone(),
            changes: self.changes.clone(),
        }));
        self.handlers.set(Some(handlers));
        self.hid.set(id + 1);
    }

    fn apply_changes(&self, ring: &mut IoUring<SEntry, CEntry>) -> bool {
        let mut changes = self.changes.borrow_mut();
        if changes.is_empty() {
            return false;
        }
        log::debug!("Apply changes, {:?}", changes.len());

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
    pub(crate) fn poll<T, F>(&self, mut f: F) -> io::Result<T>
    where
        F: FnMut() -> super::PollResult<T>,
    {
        let mut ring = self.ring.borrow_mut();

        loop {
            let wait_events = match f() {
                super::PollResult::Pending => true,
                super::PollResult::HasTasks => false,
                super::PollResult::Ready(val) => return Ok(val),
            };

            let has_more = self.apply_changes(&mut ring);
            let poll_result = self.poll_completions(&mut ring);

            let result = if has_more {
                ring.submit()
            } else if poll_result {
                continue;
            } else if wait_events {
                ring.submit_and_wait(1)
            } else {
                ring.submit()
            };
            if let Err(e) = result {
                match e.raw_os_error() {
                    Some(libc::ETIME) | Some(libc::EBUSY) | Some(libc::EAGAIN) => {}
                    _ => return Err(e),
                }
            }
        }
    }

    fn poll_completions(&self, ring: &mut IoUring<SEntry, CEntry>) -> bool {
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
    pub(crate) fn handle(&self) -> NotifyHandle {
        self.notifier.handle()
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
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
                // Clear the next time
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
pub(crate) struct NotifyHandle {
    fd: Arc<OwnedFd>,
}

impl NotifyHandle {
    pub(crate) fn new(fd: Arc<OwnedFd>) -> Self {
        Self { fd }
    }

    /// Notify the driver.
    pub(crate) fn notify(&self) -> io::Result<()> {
        let data = 1u64;
        crate::syscall!(libc::write(
            self.fd.as_raw_fd(),
            &data as *const _ as *const _,
            std::mem::size_of::<u64>(),
        ))?;
        Ok(())
    }
}
