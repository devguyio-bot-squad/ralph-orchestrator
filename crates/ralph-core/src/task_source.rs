//! Pluggable task source abstraction.
//!
//! Defines the [`TaskSource`] trait that decouples task lifecycle operations
//! from their backing store. Implementations include JSONL files (default)
//! and external systems like GitHub Issues/Projects.

use std::time::Duration;

use crate::task::Task;

/// Errors from task source operations.
///
/// Variants let callers implement intelligent retry and error reporting.
#[derive(Debug, thiserror::Error)]
pub enum TaskSourceError {
    /// Retryable after delay (rate limit, transient network failure).
    #[error("retryable error: {source}")]
    Retryable {
        source: Box<dyn std::error::Error + Send>,
        retry_after: Option<Duration>,
    },

    /// Auth failure — abort, don't retry.
    #[error("auth error: {0}")]
    Auth(String),

    /// Resource not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),

    /// Generic error.
    #[error("task source error: {0}")]
    Other(Box<dyn std::error::Error + Send>),
}

impl From<std::io::Error> for TaskSourceError {
    fn from(err: std::io::Error) -> Self {
        TaskSourceError::Other(Box::new(err))
    }
}

/// Convenience alias for task source operations.
pub type TaskSourceResult<T> = Result<T, TaskSourceError>;

/// Pluggable interface for task lifecycle operations.
///
/// Implementations back task storage to different systems (JSONL files,
/// GitHub Issues, GitHub Projects-v2, etc.). The trait is object-safe
/// (`Send` bound, owned returns, no generics) so it can be used as
/// `Box<dyn TaskSource>`.
pub trait TaskSource: Send {
    // -- Lifecycle --

    /// Ensure the external system is ready: create labels, verify project
    /// access, etc. Called by the factory on construction. Idempotent.
    fn setup(&mut self) -> TaskSourceResult<()>;

    /// Re-fetch data from the external source. Called once per event loop
    /// iteration before queries. JSONL: re-read file. GitHub: re-query API
    /// (respecting cache TTL if configured).
    fn refresh(&mut self) -> TaskSourceResult<()>;

    /// Scope subsequent queries to a specific loop. `None` = all loops.
    /// GitHub: adds `loop/{id}` to label filter. JSONL: filters in memory.
    fn set_loop_filter(&mut self, loop_id: Option<&str>);

    // -- Queries (read from in-memory state after refresh) --

    /// Get a task by ID. For JSONL: internal ID. For GitHub: issue number.
    fn get(&self, id: &str) -> TaskSourceResult<Option<Task>>;

    /// Get a task by stable key (used by `ensure` for deduplication).
    fn get_by_key(&self, key: &str) -> TaskSourceResult<Option<Task>>;

    /// All tasks (respecting loop filter).
    fn all(&self) -> TaskSourceResult<Vec<Task>>;

    /// Non-closed tasks: Open + InProgress + Failed. For visibility/prompt injection.
    fn open(&self) -> TaskSourceResult<Vec<Task>>;

    /// Non-terminal tasks: Open + InProgress only. For loop completion checks.
    fn pending(&self) -> TaskSourceResult<Vec<Task>>;

    /// Open tasks with all blockers resolved (terminal). For work selection.
    fn ready(&self) -> TaskSourceResult<Vec<Task>>;

    // -- Mutations (write-through, persist immediately) --

    /// Create a new task. Returns the created task with connector-assigned ID.
    fn add(&mut self, task: Task) -> TaskSourceResult<Task>;

    /// Transition to Closed (done). Returns updated task.
    fn close(&mut self, id: &str) -> TaskSourceResult<Option<Task>>;

    /// Transition to InProgress. Returns updated task.
    fn start(&mut self, id: &str) -> TaskSourceResult<Option<Task>>;

    /// Transition to Failed. Returns updated task.
    fn fail(&mut self, id: &str) -> TaskSourceResult<Option<Task>>;

    /// Transition to Open (reopen). Returns updated task.
    fn reopen(&mut self, id: &str) -> TaskSourceResult<Option<Task>>;

    /// Idempotent upsert by key. If a task with the same key exists,
    /// update title/priority/description but preserve lifecycle state.
    /// Empty `blocked_by` means "no change to blockers" (not "clear all").
    fn ensure(&mut self, task: Task) -> TaskSourceResult<Task>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    // -- Display tests --

    #[test]
    fn display_retryable_error() {
        let err = TaskSourceError::Retryable {
            source: Box::new(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection timed out",
            )),
            retry_after: Some(Duration::from_secs(5)),
        };
        assert_eq!(err.to_string(), "retryable error: connection timed out");
    }

    #[test]
    fn display_retryable_error_without_retry_after() {
        let err = TaskSourceError::Retryable {
            source: Box::new(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe")),
            retry_after: None,
        };
        assert_eq!(err.to_string(), "retryable error: broken pipe");
    }

    #[test]
    fn display_auth_error() {
        let err = TaskSourceError::Auth("invalid token".into());
        assert_eq!(err.to_string(), "auth error: invalid token");
    }

    #[test]
    fn display_not_found_error() {
        let err = TaskSourceError::NotFound("task-123".into());
        assert_eq!(err.to_string(), "not found: task-123");
    }

    #[test]
    fn display_config_error() {
        let err = TaskSourceError::Config("missing api_url".into());
        assert_eq!(err.to_string(), "config error: missing api_url");
    }

    #[test]
    fn display_other_error() {
        let err = TaskSourceError::Other(Box::new(io::Error::new(
            io::ErrorKind::Other,
            "unexpected failure",
        )));
        assert_eq!(err.to_string(), "task source error: unexpected failure");
    }

    // -- From<io::Error> conversion --

    #[test]
    fn from_io_error_maps_to_other() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err: TaskSourceError = io_err.into();
        assert!(
            matches!(err, TaskSourceError::Other(_)),
            "expected Other variant, got: {err:?}"
        );
        assert_eq!(err.to_string(), "task source error: file not found");
    }

    // -- Object safety --

    #[test]
    fn task_source_is_object_safe() {
        // This test validates that TaskSource can be used as a trait object.
        // If the trait were not object-safe, this function would fail to compile.
        fn _assert_object_safe(_: &dyn TaskSource) {}
        fn _assert_boxed(_: Box<dyn TaskSource>) {}
    }
}
