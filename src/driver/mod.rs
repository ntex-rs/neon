//! The platform-specified driver.
//!
//! Some types differ by compilation target.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![allow(clippy::type_complexity)]

cfg_if::cfg_if! {
    //if #[cfg(windows)] {
    //    #[path = "iocp/mod.rs"]
    //    mod sys;
    //} else
    if #[cfg(all(target_os = "linux", feature = "io-uring"))] {
        #[path = "iour.rs"]
        mod sys;
    } else if #[cfg(unix)] {
        #[path = "poll.rs"]
        mod sys;
    }
}

pub use sys::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DriverType {
    Poll,
    IoUring,
}

impl DriverType {
    pub const fn name(&self) -> &'static str {
        match self {
            DriverType::Poll => "polling",
            DriverType::IoUring => "io-uring",
        }
    }

    pub const fn is_polling(&self) -> bool {
        matches!(self, &DriverType::Poll)
    }
}

pub(crate) enum PollResult<T> {
    Ready(T),
    Pending,
    HasTasks,
}

#[cfg(windows)]
#[macro_export]
#[doc(hidden)]
macro_rules! syscall {
    (BOOL, $e:expr) => {
        $crate::syscall!($e, == 0)
    };
    (SOCKET, $e:expr) => {
        $crate::syscall!($e, != 0)
    };
    (HANDLE, $e:expr) => {
        $crate::syscall!($e, == ::windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE)
    };
    ($e:expr, $op: tt $rhs: expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res $op $rhs {
            Err(::std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

/// Helper macro to execute a system call
#[cfg(unix)]
#[macro_export]
#[doc(hidden)]
macro_rules! syscall {
    (break $e:expr) => {
        loop {
            match $crate::syscall!($e) {
                Ok(fd) => break ::std::task::Poll::Ready(Ok(fd as usize)),
                Err(e) if e.kind() == ::std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(::libc::EINPROGRESS)
                    => break ::std::task::Poll::Pending,
                Err(e) if e.kind() == ::std::io::ErrorKind::Interrupted => {},
                Err(e) => break ::std::task::Poll::Ready(Err(e)),
            }
        }
    };
    ($e:expr, $f:ident($fd:expr)) => {
        match $crate::syscall!(break $e) {
            ::std::task::Poll::Pending => Ok($crate::sys::Decision::$f($fd)),
            ::std::task::Poll::Ready(Ok(res)) => Ok($crate::sys::Decision::Completed(res)),
            ::std::task::Poll::Ready(Err(e)) => Err(e),
        }
    };
    ($e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res == -1 {
            Err(::std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}
