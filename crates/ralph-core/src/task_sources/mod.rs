//! Task source implementations.
//!
//! Each module provides a [`TaskSource`](crate::TaskSource) implementation
//! backed by a different storage system.

mod jsonl;

pub use jsonl::JsonlTaskSource;
