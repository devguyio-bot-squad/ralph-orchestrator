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

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::task::Task;
use crate::task_source::{TaskSource, TaskSourceError, TaskSourceResult};

use self::api::GhClient;
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
        // Stub — implemented in sub-task 10.6
        Ok(())
    }

    fn set_loop_filter(&mut self, loop_id: Option<&str>) {
        self.loop_filter = loop_id.map(String::from);
    }

    // -- Queries (stubs, implemented in sub-task 10.6) --

    fn get(&self, _id: &str) -> TaskSourceResult<Option<Task>> {
        Ok(None)
    }

    fn get_by_key(&self, _key: &str) -> TaskSourceResult<Option<Task>> {
        Ok(None)
    }

    fn all(&self) -> TaskSourceResult<Vec<Task>> {
        Ok(self.tasks.clone())
    }

    fn open(&self) -> TaskSourceResult<Vec<Task>> {
        Ok(Vec::new())
    }

    fn pending(&self) -> TaskSourceResult<Vec<Task>> {
        Ok(Vec::new())
    }

    fn ready(&self) -> TaskSourceResult<Vec<Task>> {
        Ok(Vec::new())
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
}
