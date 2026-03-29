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
