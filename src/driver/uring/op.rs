use std::io;

use crate::syscall;

pub trait Handler {
    /// Operation is completed
    fn completed(&mut self, user_data: usize, flags: u32, result: io::Result<i32>);

    fn canceled(&mut self, user_data: usize);
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
