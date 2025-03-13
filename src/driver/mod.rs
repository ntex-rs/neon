//! The platform-specified driver.
//!
//! Some types differ by compilation target.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![allow(clippy::type_complexity)]

mod driver_type;
pub use driver_type::*;

cfg_if::cfg_if! {
    //if #[cfg(windows)] {
    //    #[path = "iocp/mod.rs"]
    //    mod sys;
    //} else
    if #[cfg(all(target_os = "linux", feature = "io-uring"))] {
        #[path = "uring/mod.rs"]
        mod sys;
    } else if #[cfg(unix)] {
        #[path = "poll/mod.rs"]
        mod sys;
    }
}

pub use sys::*;

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

#[macro_export]
#[doc(hidden)]
macro_rules! impl_raw_fd {
    ($t:ty, $it:ty, $inner:ident) => {
        impl $crate::driver::AsRawFd for $t {
            fn as_raw_fd(&self) -> $crate::driver::RawFd {
                self.$inner.as_raw_fd()
            }
        }
        #[cfg(unix)]
        impl std::os::fd::FromRawFd for $t {
            unsafe fn from_raw_fd(fd: $crate::driver::RawFd) -> Self {
                Self {
                    $inner: std::os::fd::FromRawFd::from_raw_fd(fd),
                }
            }
        }
    };
    ($t:ty, $it:ty, $inner:ident,file) => {
        $crate::impl_raw_fd!($t, $it, $inner);
        #[cfg(windows)]
        impl std::os::windows::io::FromRawHandle for $t {
            unsafe fn from_raw_handle(handle: std::os::windows::io::RawHandle) -> Self {
                Self {
                    $inner: std::os::windows::io::FromRawHandle::from_raw_handle(handle),
                }
            }
        }
        #[cfg(windows)]
        impl std::os::windows::io::AsRawHandle for $t {
            fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
                self.$inner.as_raw_handle()
            }
        }
    };
    ($t:ty, $it:ty, $inner:ident,socket) => {
        $crate::impl_raw_fd!($t, $it, $inner);
        #[cfg(windows)]
        impl std::os::windows::io::FromRawSocket for $t {
            unsafe fn from_raw_socket(sock: std::os::windows::io::RawSocket) -> Self {
                Self {
                    $inner: std::os::windows::io::FromRawSocket::from_raw_socket(sock),
                }
            }
        }
        #[cfg(windows)]
        impl std::os::windows::io::AsRawSocket for $t {
            fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
                self.$inner.as_raw_socket()
            }
        }
    };
}
