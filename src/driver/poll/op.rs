use std::{io, os::fd::RawFd};

use crate::syscall;

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

/// Create socket.
pub async fn create_socket(
    domain: i32,
    socket_type: i32,
    protocol: i32,
) -> io::Result<i32> {
    crate::spawn_blocking(move || syscall!(libc::socket(domain, socket_type, protocol)))
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
        .and_then(|result| result.unwrap())
}

/// Close socket.
pub async fn close_socket(fd: RawFd) -> io::Result<i32> {
    crate::spawn_blocking(move || syscall!(libc::close(fd)))
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
        .and_then(|result| result.unwrap())
}
