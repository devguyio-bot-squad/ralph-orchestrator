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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::task::{Task, TaskStatus};
use crate::task_source::{TaskSource, TaskSourceError, TaskSourceResult};

use self::api::{GhClient, GhIssue};
use self::config::GithubTaskSourceConfig;

/// Status labels created during setup.
const STATUS_LABELS: &[(&str, &str, &str)] = &[
    ("status/todo", "0e8a16", "Ralph: task is open"),
    ("status/in-progress", "fbca04", "Ralph: task in progress"),
    ("status/done", "6f42c1", "Ralph: task completed"),
    ("status/failed", "d93f0b", "Ralph: task failed"),
];

/// Priority labels created during setup.
const PRIORITY_LABELS: &[(&str, &str, &str)] = &[
    ("priority/1", "b60205", "Ralph: highest priority"),
    ("priority/2", "d93f0b", "Ralph: high priority"),
    ("priority/3", "e99695", "Ralph: medium priority"),
    ("priority/4", "c2e0c6", "Ralph: low priority"),
    ("priority/5", "0e8a16", "Ralph: lowest priority"),
];

/// Setup cache validity duration (24 hours).
const SETUP_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// GitHub Issues-backed task source.
///
/// Maps GitHub issues with `status/*` and `priority/*` labels to the
/// [`TaskSource`] trait. Supports simple mode (label-based) with
/// Projects v2 mode planned for a future step.
pub struct GithubTaskSource {
    config: GithubTaskSourceConfig,
    client: GhClient,
    tasks: Vec<Task>,
    loop_filter: Option<String>,
    workspace_root: PathBuf,
}

impl std::fmt::Debug for GithubTaskSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubTaskSource")
            .field("repo", &self.config.repo)
            .field("tasks", &self.tasks.len())
            .field("loop_filter", &self.loop_filter)
            .finish_non_exhaustive()
    }
}

impl GithubTaskSource {
    /// Create from config JSON. Validates config, resolves auth token,
    /// and builds the API client.
    pub fn from_config(config: &Value, workspace_root: &Path) -> TaskSourceResult<Self> {
        if config.is_null() {
            return Err(TaskSourceError::Config(
                "GitHub task source requires config".into(),
            ));
        }

        let cfg: GithubTaskSourceConfig = serde_json::from_value(config.clone())
            .map_err(|e| TaskSourceError::Config(format!("invalid GitHub config: {e}")))?;

        cfg.validate().map_err(TaskSourceError::Config)?;

        let token = resolve_token(cfg.token.as_deref())?;
        let (owner, repo) = cfg.owner_repo();
        let client = GhClient::new(owner.to_string(), repo.to_string(), token);

        Ok(Self {
            config: cfg,
            client,
            tasks: Vec::new(),
            loop_filter: None,
            workspace_root: workspace_root.to_path_buf(),
        })
    }

    /// Path to the setup cache sentinel file.
    fn setup_cache_path(&self) -> PathBuf {
        self.workspace_root.join(".ralph/.github-setup-done")
    }

    /// Check if setup was completed recently enough to skip.
    fn setup_cache_is_valid(&self) -> bool {
        let path = self.setup_cache_path();
        match std::fs::metadata(&path) {
            Ok(meta) => meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < SETUP_CACHE_TTL),
            Err(_) => false,
        }
    }

    /// Convert a GitHub issue to a [`Task`].
    ///
    /// Extracts status from `status/*` labels, priority from `priority/*`
    /// labels, and Ralph metadata from the issue body comment.
    fn issue_to_task(issue: &GhIssue) -> Task {
        let body = issue.body.as_deref().unwrap_or("");
        let meta = metadata::parse_metadata(body);

        // Status from status/* label (default: Open)
        let status = issue
            .labels
            .iter()
            .find_map(|l| match l.name.as_str() {
                "status/todo" => Some(TaskStatus::Open),
                "status/in-progress" => Some(TaskStatus::InProgress),
                "status/done" => Some(TaskStatus::Closed),
                "status/failed" => Some(TaskStatus::Failed),
                _ => None,
            })
            .unwrap_or(TaskStatus::Open);

        // Priority from priority/{N} label (default: 3)
        let priority = issue
            .labels
            .iter()
            .find_map(|l| {
                l.name
                    .strip_prefix("priority/")
                    .and_then(|n| n.parse::<u8>().ok())
            })
            .unwrap_or(3);

        // Loop ID from loop/{id} label, falling back to metadata
        let loop_id = issue
            .labels
            .iter()
            .find_map(|l| l.name.strip_prefix("loop/").map(String::from))
            .or(meta.loop_id);

        // Build metadata HashMap
        let mut task_meta = HashMap::new();
        if !issue.assignees.is_empty() {
            task_meta.insert(
                "assignees".to_string(),
                Value::Array(
                    issue
                        .assignees
                        .iter()
                        .map(|u| Value::String(u.login.clone()))
                        .collect(),
                ),
            );
        }
        if let Some(ref ms) = issue.milestone {
            task_meta.insert("milestone".to_string(), Value::String(ms.title.clone()));
        }
        // User labels: everything that isn't status/*, priority/*, loop/*
        let user_labels: Vec<Value> = issue
            .labels
            .iter()
            .filter(|l| {
                !l.name.starts_with("status/")
                    && !l.name.starts_with("priority/")
                    && !l.name.starts_with("loop/")
            })
            .map(|l| Value::String(l.name.clone()))
            .collect();
        if !user_labels.is_empty() {
            task_meta.insert("user_labels".to_string(), Value::Array(user_labels));
        }

        Task {
            id: issue.number.to_string(),
            title: issue.title.clone(),
            description: Some(metadata::strip_metadata(body)).filter(|d| !d.is_empty()),
            key: meta.key.or(Some(issue.number.to_string())),
            status,
            priority,
            blocked_by: meta.blocked_by,
            loop_id,
            created: issue.created_at.clone(),
            started: meta.started,
            closed: issue.closed_at.clone().or(meta.closed),
            metadata: task_meta,
        }
    }

    /// Filter tasks by the active loop filter.
    fn filtered_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| {
            self.loop_filter
                .as_ref()
                .is_none_or(|filter| t.loop_id.as_deref() == Some(filter.as_str()))
        })
    }

    /// Write the setup cache sentinel.
    fn write_setup_cache(&self) -> TaskSourceResult<()> {
        let path = self.setup_cache_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, chrono::Utc::now().to_rfc3339())?;
        Ok(())
    }
}

/// Resolve a GitHub auth token via the cascade:
/// config.token → GITHUB_TOKEN env → RALPH_GITHUB_TOKEN env → `gh auth token`.
fn resolve_token(config_token: Option<&str>) -> TaskSourceResult<String> {
    // 1. Explicit config token
    if let Some(token) = config_token
        && !token.is_empty()
    {
        return Ok(token.to_owned());
    }

    // 2. GITHUB_TOKEN env
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        return Ok(token);
    }

    // 3. RALPH_GITHUB_TOKEN env
    if let Ok(token) = std::env::var("RALPH_GITHUB_TOKEN")
        && !token.is_empty()
    {
        return Ok(token);
    }

    // 4. `gh auth token` subprocess
    match std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if token.is_empty() {
                Err(TaskSourceError::Auth(
                    "gh auth token returned empty".into(),
                ))
            } else {
                Ok(token)
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(TaskSourceError::Auth(format!(
                "gh auth token failed: {stderr}"
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(TaskSourceError::Config(
            "no GitHub token found: set config.token, GITHUB_TOKEN, RALPH_GITHUB_TOKEN, or install gh CLI".into(),
        )),
        Err(e) => Err(TaskSourceError::Auth(format!(
            "failed to run gh auth token: {e}"
        ))),
    }
}

/// Testable version of token resolution that accepts environment values as parameters
/// instead of reading from the actual environment.
#[cfg(test)]
fn resolve_token_from(
    config_token: Option<&str>,
    github_token: Option<&str>,
    ralph_token: Option<&str>,
) -> Option<String> {
    if let Some(t) = config_token
        && !t.is_empty()
    {
        return Some(t.to_owned());
    }
    if let Some(t) = github_token
        && !t.is_empty()
    {
        return Some(t.to_owned());
    }
    if let Some(t) = ralph_token
        && !t.is_empty()
    {
        return Some(t.to_owned());
    }
    None
}

impl TaskSource for GithubTaskSource {
    fn setup(&mut self) -> TaskSourceResult<()> {
        if self.setup_cache_is_valid() {
            tracing::debug!("GitHub setup cache is fresh, skipping label creation");
            return Ok(());
        }

        tracing::info!("Creating GitHub labels for Ralph task tracking");

        for &(name, color, description) in STATUS_LABELS.iter().chain(PRIORITY_LABELS.iter()) {
            self.client.create_label(name, color, description)?;
        }

        self.write_setup_cache()?;
        Ok(())
    }

    fn refresh(&mut self) -> TaskSourceResult<()> {
        let status_labels: Vec<&str> = STATUS_LABELS.iter().map(|&(name, _, _)| name).collect();
        let issues = self.client.list_issues(&status_labels, "all")?;
        self.tasks = issues.iter().map(Self::issue_to_task).collect();
        Ok(())
    }

    fn set_loop_filter(&mut self, loop_id: Option<&str>) {
        self.loop_filter = loop_id.map(String::from);
    }

    // -- Queries --

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

    // -- Mutations (stubs, implemented in sub-task 10.7) --

    fn add(&mut self, _task: Task) -> TaskSourceResult<Task> {
        Err(TaskSourceError::Config(
            "GitHub add not yet implemented".into(),
        ))
    }

    fn close(&mut self, _id: &str) -> TaskSourceResult<Option<Task>> {
        Err(TaskSourceError::Config(
            "GitHub close not yet implemented".into(),
        ))
    }

    fn start(&mut self, _id: &str) -> TaskSourceResult<Option<Task>> {
        Err(TaskSourceError::Config(
            "GitHub start not yet implemented".into(),
        ))
    }

    fn fail(&mut self, _id: &str) -> TaskSourceResult<Option<Task>> {
        Err(TaskSourceError::Config(
            "GitHub fail not yet implemented".into(),
        ))
    }

    fn reopen(&mut self, _id: &str) -> TaskSourceResult<Option<Task>> {
        Err(TaskSourceError::Config(
            "GitHub reopen not yet implemented".into(),
        ))
    }

    fn ensure(&mut self, _task: Task) -> TaskSourceResult<Task> {
        Err(TaskSourceError::Config(
            "GitHub ensure not yet implemented".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_sources::github::api::{GhMilestone, GhUser, Label};

    #[test]
    fn from_config_null_returns_error() {
        let result = GithubTaskSource::from_config(&Value::Null, Path::new("/tmp"));
        let err = result.unwrap_err();
        assert!(
            matches!(err, TaskSourceError::Config(_)),
            "expected Config error, got: {err:?}"
        );
        assert!(err.to_string().contains("requires config"));
    }

    #[test]
    fn from_config_invalid_repo_returns_error() {
        let config = serde_json::json!({"repo": "no-slash", "token": "ghp_test"});
        let result = GithubTaskSource::from_config(&config, Path::new("/tmp"));
        let err = result.unwrap_err();
        assert!(
            matches!(err, TaskSourceError::Config(_)),
            "expected Config error, got: {err:?}"
        );
        assert!(err.to_string().contains("owner/repo"));
    }

    #[test]
    fn from_config_minimal_with_token() {
        let config = serde_json::json!({"repo": "acme/widgets", "token": "ghp_test123"});
        let source = GithubTaskSource::from_config(&config, Path::new("/tmp")).unwrap();
        assert_eq!(source.config.repo, "acme/widgets");
        assert!(source.tasks.is_empty());
        assert!(source.loop_filter.is_none());
    }

    #[test]
    fn resolve_token_config_takes_priority() {
        let result =
            resolve_token_from(Some("config_token"), Some("env_token"), Some("ralph_token"));
        assert_eq!(result, Some("config_token".to_string()));
    }

    #[test]
    fn resolve_token_env_fallback() {
        let result = resolve_token_from(None, Some("env_token"), Some("ralph_token"));
        assert_eq!(result, Some("env_token".to_string()));
    }

    #[test]
    fn resolve_token_ralph_env_fallback() {
        let result = resolve_token_from(None, None, Some("ralph_token"));
        assert_eq!(result, Some("ralph_token".to_string()));
    }

    #[test]
    fn resolve_token_empty_strings_skipped() {
        let result = resolve_token_from(Some(""), Some(""), Some("ralph_token"));
        assert_eq!(result, Some("ralph_token".to_string()));
    }

    #[test]
    fn resolve_token_none_when_all_missing() {
        let result = resolve_token_from(None, None, None);
        assert!(result.is_none());
    }

    #[test]
    fn setup_cache_write_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let config = serde_json::json!({"repo": "acme/widgets", "token": "ghp_test123"});
        let source = GithubTaskSource::from_config(&config, dir.path()).unwrap();

        // No cache yet
        assert!(!source.setup_cache_is_valid());

        // Write cache
        source.write_setup_cache().unwrap();

        // Now valid
        assert!(source.setup_cache_is_valid());
    }

    #[test]
    fn set_loop_filter_stores_value() {
        let config = serde_json::json!({"repo": "acme/widgets", "token": "ghp_test123"});
        let mut source = GithubTaskSource::from_config(&config, Path::new("/tmp")).unwrap();

        assert!(source.loop_filter.is_none());
        source.set_loop_filter(Some("loop-42"));
        assert_eq!(source.loop_filter.as_deref(), Some("loop-42"));
        source.set_loop_filter(None);
        assert!(source.loop_filter.is_none());
    }

    // -- issue_to_task tests --

    /// Helper to build a minimal GhIssue for testing.
    fn make_issue(number: u64, title: &str, labels: &[&str]) -> GhIssue {
        GhIssue {
            number,
            title: title.to_string(),
            body: None,
            state: "open".to_string(),
            labels: labels
                .iter()
                .map(|n| Label {
                    name: n.to_string(),
                    color: "000000".to_string(),
                    description: None,
                })
                .collect(),
            created_at: "2026-03-28T10:00:00Z".to_string(),
            closed_at: None,
            assignees: Vec::new(),
            milestone: None,
            pull_request: None,
        }
    }

    #[test]
    fn issue_to_task_full() {
        let mut issue = make_issue(
            42,
            "Fix login",
            &["status/in-progress", "priority/2", "bug", "loop/abc"],
        );
        issue.body = Some(
            "Fix the bug.\n\n<!-- ralph:metadata:v1 {\"blocked_by\":[\"10\"],\"key\":\"spec:auth\",\"started\":\"2026-03-28T09:00:00Z\"} -->"
                .to_string(),
        );
        issue.closed_at = Some("2026-03-28T12:00:00Z".to_string());
        issue.assignees = vec![GhUser {
            login: "alice".to_string(),
        }];
        issue.milestone = Some(GhMilestone {
            number: 1,
            title: "v1.0".to_string(),
        });

        let task = GithubTaskSource::issue_to_task(&issue);

        assert_eq!(task.id, "42");
        assert_eq!(task.title, "Fix login");
        assert_eq!(task.description.as_deref(), Some("Fix the bug."));
        assert_eq!(task.key.as_deref(), Some("spec:auth"));
        assert_eq!(task.status, TaskStatus::InProgress);
        assert_eq!(task.priority, 2);
        assert_eq!(task.blocked_by, vec!["10"]);
        assert_eq!(task.loop_id.as_deref(), Some("abc"));
        assert_eq!(task.started.as_deref(), Some("2026-03-28T09:00:00Z"));
        assert_eq!(task.closed.as_deref(), Some("2026-03-28T12:00:00Z"));
        assert_eq!(task.metadata["assignees"], serde_json::json!(["alice"]));
        assert_eq!(task.metadata["milestone"], serde_json::json!("v1.0"));
        assert_eq!(task.metadata["user_labels"], serde_json::json!(["bug"]));
    }

    #[test]
    fn issue_to_task_minimal() {
        let issue = make_issue(1, "Simple", &["status/todo"]);
        let task = GithubTaskSource::issue_to_task(&issue);

        assert_eq!(task.id, "1");
        assert_eq!(task.title, "Simple");
        assert!(task.description.is_none());
        assert_eq!(task.key.as_deref(), Some("1"));
        assert_eq!(task.status, TaskStatus::Open);
        assert_eq!(task.priority, 3);
        assert!(task.blocked_by.is_empty());
        assert!(task.loop_id.is_none());
        assert!(task.started.is_none());
        assert!(task.closed.is_none());
        assert!(task.metadata.is_empty());
    }

    #[test]
    fn issue_to_task_no_status_label_defaults_open() {
        let issue = make_issue(5, "No status", &["bug", "priority/1"]);
        let task = GithubTaskSource::issue_to_task(&issue);

        assert_eq!(task.status, TaskStatus::Open);
        assert_eq!(task.priority, 1);
    }

    #[test]
    fn issue_to_task_strips_metadata_from_description() {
        let mut issue = make_issue(7, "Has meta", &["status/todo"]);
        issue.body =
            Some("User visible text.\n\n<!-- ralph:metadata:v1 {\"key\":\"test\"} -->".to_string());

        let task = GithubTaskSource::issue_to_task(&issue);
        assert_eq!(task.description.as_deref(), Some("User visible text."));
        assert_eq!(task.key.as_deref(), Some("test"));
    }

    #[test]
    fn query_all_respects_loop_filter() {
        let config = serde_json::json!({"repo": "acme/widgets", "token": "ghp_test123"});
        let mut source = GithubTaskSource::from_config(&config, Path::new("/tmp")).unwrap();

        let mut t1 = Task::new("Loop A".to_string(), 1);
        t1.loop_id = Some("loop-a".to_string());
        let mut t2 = Task::new("Loop B".to_string(), 1);
        t2.loop_id = Some("loop-b".to_string());
        let t3 = Task::new("No loop".to_string(), 1);

        source.tasks = vec![t1, t2, t3];

        // No filter: all 3
        assert_eq!(source.all().unwrap().len(), 3);

        // Filter to loop-a: only 1
        source.set_loop_filter(Some("loop-a"));
        let filtered = source.all().unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "Loop A");
    }

    #[test]
    fn query_open_excludes_closed_includes_failed() {
        let config = serde_json::json!({"repo": "acme/widgets", "token": "ghp_test123"});
        let mut source = GithubTaskSource::from_config(&config, Path::new("/tmp")).unwrap();

        let mut t_open = Task::new("Open".to_string(), 1);
        t_open.status = TaskStatus::Open;
        let mut t_closed = Task::new("Closed".to_string(), 1);
        t_closed.status = TaskStatus::Closed;
        let mut t_failed = Task::new("Failed".to_string(), 1);
        t_failed.status = TaskStatus::Failed;
        let mut t_ip = Task::new("InProgress".to_string(), 1);
        t_ip.status = TaskStatus::InProgress;

        source.tasks = vec![t_open, t_closed, t_failed, t_ip];

        let open = source.open().unwrap();
        assert_eq!(open.len(), 3); // Open, Failed, InProgress
        assert!(!open.iter().any(|t| t.title == "Closed"));
    }

    #[test]
    fn query_pending_excludes_terminal() {
        let config = serde_json::json!({"repo": "acme/widgets", "token": "ghp_test123"});
        let mut source = GithubTaskSource::from_config(&config, Path::new("/tmp")).unwrap();

        let mut t_open = Task::new("Open".to_string(), 1);
        t_open.status = TaskStatus::Open;
        let mut t_closed = Task::new("Closed".to_string(), 1);
        t_closed.status = TaskStatus::Closed;
        let mut t_failed = Task::new("Failed".to_string(), 1);
        t_failed.status = TaskStatus::Failed;
        let mut t_ip = Task::new("InProgress".to_string(), 1);
        t_ip.status = TaskStatus::InProgress;

        source.tasks = vec![t_open, t_closed, t_failed, t_ip];

        let pending = source.pending().unwrap();
        assert_eq!(pending.len(), 2); // Open, InProgress
        assert!(pending.iter().all(|t| !t.status.is_terminal()));
    }

    #[test]
    fn query_ready_checks_blockers() {
        let config = serde_json::json!({"repo": "acme/widgets", "token": "ghp_test123"});
        let mut source = GithubTaskSource::from_config(&config, Path::new("/tmp")).unwrap();

        let t1 = Task::new("Unblocked".to_string(), 1);
        let t1_id = t1.id.clone();
        let mut t2 = Task::new("Blocked".to_string(), 2);
        t2.blocked_by = vec![t1_id];

        source.tasks = vec![t1, t2];

        let ready = source.ready().unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].title, "Unblocked");
    }
}
