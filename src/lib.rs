//! The async runtime for ntex.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
// #![warn(missing_docs)]

pub mod driver;
pub mod pool;
mod rt;

pub use async_task::Task;
pub use rt::{spawn, spawn_blocking, Handle, JoinHandle, Runtime, RuntimeBuilder};
