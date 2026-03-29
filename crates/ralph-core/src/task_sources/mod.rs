//! Task source implementations.
//!
//! Each module provides a [`TaskSource`](crate::TaskSource) implementation
//! backed by a different storage system.

pub mod github;
mod jsonl;

pub use github::GithubTaskSource;
pub use jsonl::JsonlTaskSource;
