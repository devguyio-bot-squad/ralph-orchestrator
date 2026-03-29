//! GitHub-backed task source.
//!
//! Maps GitHub Issues (with optional Projects v2 integration) to the
//! [`TaskSource`](crate::TaskSource) trait. Two modes are supported:
//!
//! - **Simple** — labels encode status/priority, issue body carries metadata.
//! - **Projects v2** — (future) uses project fields for status tracking.

pub mod api;
pub mod config;
pub mod metadata;
