//! JSONL-backed task source.
//!
//! Implements [`TaskSource`] with write-through persistence to a JSONL file.
//! Each mutation acquires an exclusive flock, re-reads from disk, applies the
//! change, and writes all tasks back — ensuring concurrent loops never lose data.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::warn;

use crate::file_lock::FileLock;
use crate::task::{Task, TaskStatus};
use crate::task_source::{TaskSource, TaskSourceError, TaskSourceResult};

/// A [`TaskSource`] backed by a JSONL file with file-locking for multi-loop safety.
pub struct JsonlTaskSource {
    path: PathBuf,
    tasks: Vec<Task>,
    lock: FileLock,
    loop_filter: Option<String>,
}

/// Parses a JSONL line into a Task, logging a warning on failure.
fn parse_task_line(line: &str) -> Option<Task> {
    match serde_json::from_str(line) {
        Ok(task) => Some(task),
        Err(e) => {
            warn!(
                error = %e,
                line = line.chars().take(200).collect::<String>(),
                "Skipping malformed task line in JSONL"
            );
            None
        }
    }
}

/// Reads tasks from a JSONL file, skipping blank and malformed lines.
fn read_tasks_from_file(path: &Path) -> std::io::Result<Vec<Task>> {
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        Ok(content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(parse_task_line)
            .collect())
    } else {
        Ok(Vec::new())
    }
}

/// Serializes tasks to JSONL and writes to disk.
fn write_tasks_to_file(path: &Path, tasks: &[Task]) -> TaskSourceResult<()> {
    let content: String = tasks
        .iter()
        .map(|t| {
            serde_json::to_string(t).map_err(|e| {
                TaskSourceError::Other(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("task serialization failed: {e}"),
                )))
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    std::fs::write(
        path,
        if content.is_empty() {
            String::new()
        } else {
            content + "\n"
        },
    )?;
    Ok(())
}

impl JsonlTaskSource {
    /// Creates a new `JsonlTaskSource` from config.
    ///
    /// Config may contain an optional `path` key. If absent, defaults to
    /// `workspace_root/.ralph/agent/tasks.jsonl`.
    ///
    /// Loads existing tasks from disk under a shared lock.
    pub fn from_config(config: &Value, workspace_root: &Path) -> TaskSourceResult<Self> {
        let path = match config.get("path").and_then(Value::as_str) {
            Some(p) => PathBuf::from(p),
            None => workspace_root.join(".ralph/agent/tasks.jsonl"),
        };

        let lock = FileLock::new(&path)?;
        let tasks = {
            let _guard = lock.shared()?;
            read_tasks_from_file(&path)?
        };

        Ok(Self {
            path,
            tasks,
            lock,
            loop_filter: None,
        })
    }

    /// Returns tasks filtered by the current loop filter.
    fn filtered_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| {
            self.loop_filter
                .as_ref()
                .is_none_or(|filter| t.loop_id.as_deref() == Some(filter.as_str()))
        })
    }

    /// Acquires an exclusive lock, re-reads tasks from disk, applies a
    /// mutation, writes back, and returns the result.
    fn mutate<F, T>(&mut self, f: F) -> TaskSourceResult<T>
    where
        F: FnOnce(&mut Vec<Task>) -> T,
    {
        let _guard = self.lock.exclusive()?;
        self.tasks = read_tasks_from_file(&self.path)?;
        let result = f(&mut self.tasks);
        write_tasks_to_file(&self.path, &self.tasks)?;
        Ok(result)
    }
}

impl TaskSource for JsonlTaskSource {
    fn setup(&mut self) -> TaskSourceResult<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    fn refresh(&mut self) -> TaskSourceResult<()> {
        let _guard = self.lock.shared()?;
        self.tasks = read_tasks_from_file(&self.path)?;
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
        self.mutate(|tasks| {
            tasks.push(task.clone());
            task
        })
    }

    fn close(&mut self, id: &str) -> TaskSourceResult<Option<Task>> {
        let id = id.to_string();
        self.mutate(|tasks| {
            if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                task.status = TaskStatus::Closed;
                task.closed = Some(chrono::Utc::now().to_rfc3339());
                Some(task.clone())
            } else {
                None
            }
        })
    }

    fn start(&mut self, id: &str) -> TaskSourceResult<Option<Task>> {
        let id = id.to_string();
        self.mutate(|tasks| {
            if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                task.start();
                Some(task.clone())
            } else {
                None
            }
        })
    }

    fn fail(&mut self, id: &str) -> TaskSourceResult<Option<Task>> {
        let id = id.to_string();
        self.mutate(|tasks| {
            if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                task.status = TaskStatus::Failed;
                task.closed = Some(chrono::Utc::now().to_rfc3339());
                Some(task.clone())
            } else {
                None
            }
        })
    }

    fn reopen(&mut self, id: &str) -> TaskSourceResult<Option<Task>> {
        let id = id.to_string();
        self.mutate(|tasks| {
            if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                task.reopen();
                Some(task.clone())
            } else {
                None
            }
        })
    }

    fn ensure(&mut self, task: Task) -> TaskSourceResult<Task> {
        self.mutate(|tasks| {
            if let Some(key) = task.key.as_deref()
                && let Some(existing) = tasks.iter_mut().find(|t| t.key.as_deref() == Some(key))
            {
                existing.title = task.title;
                existing.priority = task.priority;
                if task.description.is_some() {
                    existing.description = task.description;
                }
                // Empty blocked_by means "no change to blockers" — preserves
                // existing blockers when the caller doesn't specify new ones.
                if !task.blocked_by.is_empty() {
                    existing.blocked_by = task.blocked_by;
                }
                return existing.clone();
            }
            tasks.push(task.clone());
            task
        })
    }
}
