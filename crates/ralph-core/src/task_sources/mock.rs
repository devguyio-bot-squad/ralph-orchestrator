//! In-memory task source for testing.
//!
//! Provides a [`TaskSource`] implementation with configurable error injection
//! and call tracking — useful for integration tests that exercise event loop
//! wiring without filesystem or subprocess dependencies.

use std::collections::HashMap;

use crate::task::{Task, TaskStatus};
use crate::task_source::{TaskSource, TaskSourceError, TaskSourceResult};

/// Describes an error to inject. Separate from [`TaskSourceError`] because
/// `TaskSourceError::Other` / `Retryable` contain a `Box<dyn Error + Send>`
/// which is not `Clone`.
#[derive(Debug, Clone)]
pub enum MockError {
    Auth(String),
    NotFound(String),
    Config(String),
    Retryable(String),
    Other(String),
}

impl MockError {
    fn into_task_source_error(self) -> TaskSourceError {
        match self {
            MockError::Auth(msg) => TaskSourceError::Auth(msg),
            MockError::NotFound(msg) => TaskSourceError::NotFound(msg),
            MockError::Config(msg) => TaskSourceError::Config(msg),
            MockError::Retryable(msg) => TaskSourceError::Retryable {
                source: Box::new(std::io::Error::new(std::io::ErrorKind::Other, msg)),
                retry_after: None,
            },
            MockError::Other(msg) => TaskSourceError::Other(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                msg,
            ))),
        }
    }
}

/// In-memory [`TaskSource`] with configurable error injection and call tracking.
pub struct MockTaskSource {
    tasks: Vec<Task>,
    loop_filter: Option<String>,
    inject_errors: HashMap<String, MockError>,
    call_counts: HashMap<String, usize>,
}

impl MockTaskSource {
    /// Creates an empty mock source.
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            loop_filter: None,
            inject_errors: HashMap::new(),
            call_counts: HashMap::new(),
        }
    }

    /// Creates a mock source pre-populated with tasks.
    pub fn with_tasks(tasks: Vec<Task>) -> Self {
        Self {
            tasks,
            loop_filter: None,
            inject_errors: HashMap::new(),
            call_counts: HashMap::new(),
        }
    }

    /// Inject an error for a specific method. The error is returned on every
    /// call to that method until cleared.
    pub fn inject_error(&mut self, method: &str, error: MockError) {
        self.inject_errors.insert(method.to_string(), error);
    }

    /// Clear an injected error for a method.
    pub fn clear_error(&mut self, method: &str) {
        self.inject_errors.remove(method);
    }

    /// Get the call count for a method.
    pub fn call_count(&self, method: &str) -> usize {
        self.call_counts.get(method).copied().unwrap_or(0)
    }

    fn track(&mut self, method: &str) {
        *self.call_counts.entry(method.to_string()).or_insert(0) += 1;
    }

    fn check_error(&mut self, method: &str) -> Option<TaskSourceError> {
        self.inject_errors
            .get(method)
            .cloned()
            .map(MockError::into_task_source_error)
    }

    fn filtered_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| {
            self.loop_filter
                .as_ref()
                .is_none_or(|filter| t.loop_id.as_deref() == Some(filter.as_str()))
        })
    }
}

impl TaskSource for MockTaskSource {
    fn setup(&mut self) -> TaskSourceResult<()> {
        self.track("setup");
        if let Some(err) = self.check_error("setup") {
            return Err(err);
        }
        Ok(())
    }

    fn refresh(&mut self) -> TaskSourceResult<()> {
        self.track("refresh");
        if let Some(err) = self.check_error("refresh") {
            return Err(err);
        }
        Ok(())
    }

    fn set_loop_filter(&mut self, loop_id: Option<&str>) {
        self.loop_filter = loop_id.map(String::from);
    }

    fn get(&self, id: &str) -> TaskSourceResult<Option<Task>> {
        Ok(self.tasks.iter().find(|t| t.id == id).cloned())
    }

    fn get_by_key(&self, key: &str) -> TaskSourceResult<Option<Task>> {
        Ok(self
            .tasks
            .iter()
            .find(|t| t.key.as_deref() == Some(key))
            .cloned())
    }

    fn all(&self) -> TaskSourceResult<Vec<Task>> {
        Ok(self.filtered_tasks().cloned().collect())
    }

    fn open(&self) -> TaskSourceResult<Vec<Task>> {
        Ok(self
            .filtered_tasks()
            .filter(|t| t.status != TaskStatus::Closed)
            .cloned()
            .collect())
    }

    fn pending(&self) -> TaskSourceResult<Vec<Task>> {
        Ok(self
            .filtered_tasks()
            .filter(|t| !t.status.is_terminal())
            .cloned()
            .collect())
    }

    fn ready(&self) -> TaskSourceResult<Vec<Task>> {
        let all_tasks: Vec<Task> = self.filtered_tasks().cloned().collect();
        Ok(all_tasks
            .iter()
            .filter(|t| t.is_ready(&all_tasks))
            .cloned()
            .collect())
    }

    fn add(&mut self, task: Task) -> TaskSourceResult<Task> {
        self.track("add");
        if let Some(err) = self.check_error("add") {
            return Err(err);
        }
        self.tasks.push(task.clone());
        Ok(task)
    }

    fn close(&mut self, id: &str) -> TaskSourceResult<Option<Task>> {
        self.track("close");
        if let Some(err) = self.check_error("close") {
            return Err(err);
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
            task.status = TaskStatus::Closed;
            task.closed = Some(chrono::Utc::now().to_rfc3339());
            Ok(Some(task.clone()))
        } else {
            Ok(None)
        }
    }

    fn start(&mut self, id: &str) -> TaskSourceResult<Option<Task>> {
        self.track("start");
        if let Some(err) = self.check_error("start") {
            return Err(err);
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
            task.start();
            Ok(Some(task.clone()))
        } else {
            Ok(None)
        }
    }

    fn fail(&mut self, id: &str) -> TaskSourceResult<Option<Task>> {
        self.track("fail");
        if let Some(err) = self.check_error("fail") {
            return Err(err);
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
            task.status = TaskStatus::Failed;
            task.closed = Some(chrono::Utc::now().to_rfc3339());
            Ok(Some(task.clone()))
        } else {
            Ok(None)
        }
    }

    fn reopen(&mut self, id: &str) -> TaskSourceResult<Option<Task>> {
        self.track("reopen");
        if let Some(err) = self.check_error("reopen") {
            return Err(err);
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
            task.reopen();
            Ok(Some(task.clone()))
        } else {
            Ok(None)
        }
    }

    fn ensure(&mut self, task: Task) -> TaskSourceResult<Task> {
        self.track("ensure");
        if let Some(err) = self.check_error("ensure") {
            return Err(err);
        }
        if let Some(key) = task.key.as_deref()
            && let Some(existing) = self
                .tasks
                .iter_mut()
                .find(|t| t.key.as_deref() == Some(key))
        {
            existing.title = task.title;
            existing.priority = task.priority;
            if task.description.is_some() {
                existing.description = task.description;
            }
            if !task.blocked_by.is_empty() {
                existing.blocked_by = task.blocked_by;
            }
            return Ok(existing.clone());
        }
        self.tasks.push(task.clone());
        Ok(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_get() {
        let mut src = MockTaskSource::new();
        let task = Task::new("Test task".to_string(), 2);
        let id = task.id.clone();
        let added = src.add(task).unwrap();
        assert_eq!(added.title, "Test task");

        let found = src.get(&id).unwrap().expect("should find task by id");
        assert_eq!(found.title, "Test task");
        assert_eq!(found.priority, 2);
    }

    #[test]
    fn error_injection() {
        let mut src = MockTaskSource::new();

        // Inject an Auth error on refresh.
        src.inject_error("refresh", MockError::Auth("bad token".into()));
        let err = src.refresh().unwrap_err();
        assert!(
            err.to_string().contains("bad token"),
            "expected auth error, got: {err}"
        );

        // Clear error — refresh should succeed.
        src.clear_error("refresh");
        src.refresh().unwrap();
    }

    #[test]
    fn call_tracking() {
        let mut src = MockTaskSource::new();
        assert_eq!(src.call_count("refresh"), 0);

        src.refresh().unwrap();
        src.refresh().unwrap();
        src.refresh().unwrap();
        assert_eq!(src.call_count("refresh"), 3);
        assert_eq!(src.call_count("setup"), 0);
    }

    #[test]
    fn lifecycle() {
        let mut src = MockTaskSource::new();
        let task = Task::new("Lifecycle".to_string(), 1);
        let id = task.id.clone();
        src.add(task).unwrap();

        // start
        let started = src.start(&id).unwrap().expect("should start");
        assert_eq!(started.status, TaskStatus::InProgress);
        assert!(started.started.is_some());

        // fail
        let failed = src.fail(&id).unwrap().expect("should fail");
        assert_eq!(failed.status, TaskStatus::Failed);
        assert!(failed.closed.is_some());

        // reopen
        let reopened = src.reopen(&id).unwrap().expect("should reopen");
        assert_eq!(reopened.status, TaskStatus::Open);
        assert!(reopened.closed.is_none());

        // close
        let closed = src.close(&id).unwrap().expect("should close");
        assert_eq!(closed.status, TaskStatus::Closed);
        assert!(closed.closed.is_some());
    }
}
