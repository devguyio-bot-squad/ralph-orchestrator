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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_source::TaskSource;
    use tempfile::TempDir;

    /// Helper: create a `JsonlTaskSource` with default config pointing at a temp dir.
    fn source_in(tmp: &TempDir) -> JsonlTaskSource {
        let config = serde_json::json!({});
        JsonlTaskSource::from_config(&config, tmp.path()).unwrap()
    }

    /// Helper: reload source from the same temp dir (simulates a fresh process).
    fn reload_in(tmp: &TempDir) -> JsonlTaskSource {
        source_in(tmp)
    }

    // ── Ported from legacy task store ───────────────────────────────────

    #[test]
    fn from_config_nonexistent_file() {
        let tmp = TempDir::new().unwrap();
        let src = source_in(&tmp);
        assert_eq!(src.all().unwrap().len(), 0);
    }

    #[test]
    fn add_persists() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        let task = Task::new("Write-through".to_string(), 1);
        src.add(task).unwrap();

        // No explicit save — reload should see the task immediately.
        let loaded = reload_in(&tmp);
        assert_eq!(loaded.all().unwrap().len(), 1);
        assert_eq!(loaded.all().unwrap()[0].title, "Write-through");
    }

    #[test]
    fn get_by_id() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        let task = Task::new("Find me".to_string(), 1);
        let id = task.id.clone();
        src.add(task).unwrap();

        let found = src.get(&id).unwrap().expect("should find task by ID");
        assert_eq!(found.title, "Find me");
    }

    #[test]
    fn get_by_key() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        let task = Task::new("Keyed".to_string(), 2).with_key(Some("phase:design".to_string()));
        src.add(task).unwrap();

        let found = src
            .get_by_key("phase:design")
            .unwrap()
            .expect("should find task by key");
        assert_eq!(found.title, "Keyed");
    }

    #[test]
    fn close_task() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        let task = Task::new("Close me".to_string(), 1);
        let id = task.id.clone();
        src.add(task).unwrap();

        let closed = src.close(&id).unwrap().expect("should return closed task");
        assert_eq!(closed.status, TaskStatus::Closed);
        assert!(closed.closed.is_some());
    }

    #[test]
    fn start_task() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        let task = Task::new("Start me".to_string(), 1);
        let id = task.id.clone();
        src.add(task).unwrap();

        let started = src.start(&id).unwrap().expect("should return started task");
        assert_eq!(started.status, TaskStatus::InProgress);
        assert!(started.started.is_some());
    }

    #[test]
    fn reopen_task() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        let task = Task::new("Reopen me".to_string(), 1);
        let id = task.id.clone();
        src.add(task).unwrap();
        src.close(&id).unwrap();

        let reopened = src
            .reopen(&id)
            .unwrap()
            .expect("should return reopened task");
        assert_eq!(reopened.status, TaskStatus::Open);
        assert!(reopened.closed.is_none());
    }

    #[test]
    fn fail_task() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        let task = Task::new("Fail me".to_string(), 1);
        let id = task.id.clone();
        src.add(task).unwrap();

        let failed = src.fail(&id).unwrap().expect("should return failed task");
        assert_eq!(failed.status, TaskStatus::Failed);
        assert!(failed.closed.is_some());
    }

    #[test]
    fn open_excludes_closed() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        src.add(Task::new("Open one".to_string(), 1)).unwrap();
        let t2 = Task::new("Will close".to_string(), 1);
        let id2 = t2.id.clone();
        src.add(t2).unwrap();
        src.close(&id2).unwrap();

        let open = src.open().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].title, "Open one");
    }

    #[test]
    fn ready_excludes_blocked() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        let t1 = Task::new("Ready".to_string(), 1);
        let id1 = t1.id.clone();
        src.add(t1).unwrap();

        let t2 = Task::new("Blocked".to_string(), 1).with_blocker(id1);
        src.add(t2).unwrap();

        let ready = src.ready().unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].title, "Ready");
    }

    #[test]
    fn ensure_deduplicates_by_key() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        let first = Task::new("First".to_string(), 1).with_key(Some("impl:task-01".to_string()));
        let second = Task::new("Second".to_string(), 3).with_key(Some("impl:task-01".to_string()));

        let id = src.ensure(first).unwrap().id.clone();
        let deduped_id = src.ensure(second).unwrap().id.clone();
        let deduped = src
            .get_by_key("impl:task-01")
            .unwrap()
            .expect("deduped task should exist");

        assert_eq!(src.all().unwrap().len(), 1);
        assert_eq!(deduped_id, id);
        assert_eq!(deduped.title, "Second");
        assert_eq!(deduped.priority, 3);
    }

    #[test]
    fn load_skips_malformed_lines() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        src.add(Task::new("Valid".to_string(), 1)).unwrap();

        // Append garbage lines directly to the file.
        let path = tmp.path().join(".ralph/agent/tasks.jsonl");
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str("this is not json\n");
        content.push_str("{\"broken\": true}\n");
        std::fs::write(&path, content).unwrap();

        let loaded = reload_in(&tmp);
        let all = loaded.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Valid");
    }

    // ── New tests specific to JsonlTaskSource ────────────────────────────

    #[test]
    fn pending_excludes_terminal() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        let t_open = Task::new("Open".to_string(), 1);
        let t_fail = Task::new("Will fail".to_string(), 1);
        let t_close = Task::new("Will close".to_string(), 1);
        let fail_id = t_fail.id.clone();
        let close_id = t_close.id.clone();

        src.add(t_open).unwrap();
        src.add(t_fail).unwrap();
        src.add(t_close).unwrap();
        src.fail(&fail_id).unwrap();
        src.close(&close_id).unwrap();

        let pending = src.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title, "Open");
    }

    #[test]
    fn open_includes_failed() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        let task = Task::new("Will fail".to_string(), 1);
        let id = task.id.clone();
        src.add(task).unwrap();
        src.fail(&id).unwrap();

        // open() excludes Closed but includes Failed.
        let open = src.open().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].status, TaskStatus::Failed);
    }

    #[test]
    fn loop_filter_scopes_queries() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        let t1 = Task::new("Loop A".to_string(), 1).with_loop_id(Some("loop-a".to_string()));
        let t2 = Task::new("Loop B".to_string(), 1).with_loop_id(Some("loop-b".to_string()));
        let t3 = Task::new("No loop".to_string(), 1);
        src.add(t1).unwrap();
        src.add(t2).unwrap();
        src.add(t3).unwrap();

        // Unfiltered: all 3.
        assert_eq!(src.all().unwrap().len(), 3);

        // Filter to loop-a.
        src.set_loop_filter(Some("loop-a"));
        assert_eq!(src.all().unwrap().len(), 1);
        assert_eq!(src.all().unwrap()[0].title, "Loop A");
        assert_eq!(src.open().unwrap().len(), 1);
        assert_eq!(src.pending().unwrap().len(), 1);
        assert_eq!(src.ready().unwrap().len(), 1);

        // Clear filter.
        src.set_loop_filter(None);
        assert_eq!(src.all().unwrap().len(), 3);
    }

    #[test]
    fn metadata_round_trip() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        let mut task = Task::new("With meta".to_string(), 1);
        task.metadata.insert(
            "source".to_string(),
            serde_json::Value::String("github".to_string()),
        );
        task.metadata
            .insert("issue".to_string(), serde_json::Value::Number(42.into()));
        let id = task.id.clone();
        src.add(task).unwrap();

        // Reload from disk and verify metadata survived.
        let loaded = reload_in(&tmp);
        let found = loaded.get(&id).unwrap().expect("task should exist");
        assert_eq!(found.metadata.len(), 2);
        assert_eq!(
            found.metadata["source"],
            serde_json::Value::String("github".to_string())
        );
        assert_eq!(
            found.metadata["issue"],
            serde_json::Value::Number(42.into())
        );
    }

    #[test]
    fn write_through_no_explicit_save() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        let task = Task::new("Persisted".to_string(), 1);
        let id = task.id.clone();
        src.add(task).unwrap();
        src.start(&id).unwrap();

        // Read raw file — the task should already be on disk with InProgress.
        let path = tmp.path().join(".ralph/agent/tasks.jsonl");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("in_progress"), "status should be on disk");
    }

    #[test]
    fn concurrent_mutations_atomic() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = TempDir::new().unwrap();
        // Pre-create the file so both threads can open it.
        let _ = source_in(&tmp);
        let root = tmp.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));

        let root1 = root.clone();
        let b1 = barrier.clone();
        let h1 = thread::spawn(move || {
            let mut src = JsonlTaskSource::from_config(&serde_json::json!({}), &root1).unwrap();
            b1.wait();
            src.add(Task::new("Thread 1".to_string(), 1)).unwrap();
        });

        let root2 = root.clone();
        let b2 = barrier.clone();
        let h2 = thread::spawn(move || {
            let mut src = JsonlTaskSource::from_config(&serde_json::json!({}), &root2).unwrap();
            b2.wait();
            src.add(Task::new("Thread 2".to_string(), 1)).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();

        let final_src = JsonlTaskSource::from_config(&serde_json::json!({}), &root).unwrap();
        assert_eq!(final_src.all().unwrap().len(), 2);
    }

    #[test]
    fn ensure_empty_blocked_by_preserves_existing() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);

        let blocker = Task::new("Blocker".to_string(), 1);
        let blocker_id = blocker.id.clone();
        src.add(blocker).unwrap();

        let task = Task::new("Blocked".to_string(), 2)
            .with_key(Some("k:1".to_string()))
            .with_blocker(blocker_id.clone());
        src.ensure(task).unwrap();

        // Ensure with empty blocked_by should NOT clear existing blockers.
        let update = Task::new("Updated".to_string(), 1).with_key(Some("k:1".to_string()));
        assert!(update.blocked_by.is_empty());
        let result = src.ensure(update).unwrap();
        assert_eq!(result.blocked_by, vec![blocker_id]);
    }

    #[test]
    fn from_config_custom_path() {
        let tmp = TempDir::new().unwrap();
        let custom = tmp.path().join("custom/dir/my-tasks.jsonl");
        let config = serde_json::json!({ "path": custom.to_str().unwrap() });
        let mut src = JsonlTaskSource::from_config(&config, tmp.path()).unwrap();
        src.setup().unwrap();
        src.add(Task::new("Custom path".to_string(), 1)).unwrap();

        let loaded = JsonlTaskSource::from_config(&config, tmp.path()).unwrap();
        assert_eq!(loaded.all().unwrap().len(), 1);
    }

    #[test]
    fn refresh_picks_up_external_changes() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        src.add(Task::new("Original".to_string(), 1)).unwrap();
        assert_eq!(src.all().unwrap().len(), 1);

        // A second writer adds a task directly.
        let mut src2 = reload_in(&tmp);
        src2.add(Task::new("External".to_string(), 1)).unwrap();

        // First source doesn't see it yet (stale in-memory cache).
        assert_eq!(src.all().unwrap().len(), 1);

        // After refresh, it does.
        src.refresh().unwrap();
        assert_eq!(src.all().unwrap().len(), 2);
    }

    #[test]
    fn get_returns_none_for_missing() {
        let tmp = TempDir::new().unwrap();
        let src = source_in(&tmp);
        assert!(src.get("nonexistent-id").unwrap().is_none());
    }

    #[test]
    fn close_returns_none_for_missing() {
        let tmp = TempDir::new().unwrap();
        let mut src = source_in(&tmp);
        assert!(src.close("nonexistent-id").unwrap().is_none());
    }

    // ── Error handling tests ─────────────────────────────────────────────

    #[test]
    fn from_config_invalid_config() {
        let tmp = TempDir::new().unwrap();
        // A non-string "path" value should fall back to the default path (not crash).
        let config = serde_json::json!({ "path": 42 });
        let src = JsonlTaskSource::from_config(&config, tmp.path()).unwrap();
        assert_eq!(src.all().unwrap().len(), 0);
    }

    #[test]
    fn setup_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a/b/c/tasks.jsonl");
        let config = serde_json::json!({ "path": deep.to_str().unwrap() });
        let mut src = JsonlTaskSource::from_config(&config, tmp.path()).unwrap();
        src.setup().unwrap();

        // Parent should now exist.
        assert!(deep.parent().unwrap().exists());

        // And we can add tasks.
        src.add(Task::new("Deep".to_string(), 1)).unwrap();
        assert_eq!(src.all().unwrap().len(), 1);
    }
}
