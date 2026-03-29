//! Low-level GitHub API client wrapping the `gh` CLI.
//!
//! All API calls go through `gh api` subprocess, which handles auth,
//! pagination, and base URL. Token is passed via `GH_TOKEN` env var.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::task_source::{TaskSourceError, TaskSourceResult};

/// Regex matching `HTTP NNN` in gh stderr output.
static HTTP_STATUS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"HTTP (\d{3})").expect("http status regex is valid"));

/// Regex matching retry-after hints in rate limit responses.
static RETRY_AFTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)retry.after[:\s]+(\d+)").expect("retry-after regex is valid")
});

/// A label from the GitHub API.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Label {
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A GitHub issue as returned by the REST API.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GhIssue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    pub labels: Vec<Label>,
    pub created_at: String,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub assignees: Vec<GhUser>,
    #[serde(default)]
    pub milestone: Option<GhMilestone>,
    /// Present on pull requests — used to filter them out of issue listings.
    #[serde(default)]
    pub pull_request: Option<Value>,
}

/// A GitHub user (minimal fields).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GhUser {
    pub login: String,
}

/// A GitHub milestone (minimal fields).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GhMilestone {
    pub number: u64,
    pub title: String,
}

/// Fields to update on a GitHub issue. Only `Some` fields are included in the PATCH body.
#[derive(Debug, Default)]
pub struct IssueUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
    /// Full replacement array (atomic label swap).
    pub labels: Option<Vec<String>>,
    pub assignees: Option<Vec<String>>,
}

impl IssueUpdate {
    /// Build a JSON object containing only the set fields.
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        if let Some(ref title) = self.title {
            map.insert("title".into(), Value::String(title.clone()));
        }
        if let Some(ref body) = self.body {
            map.insert("body".into(), Value::String(body.clone()));
        }
        if let Some(ref state) = self.state {
            map.insert("state".into(), Value::String(state.clone()));
        }
        if let Some(ref labels) = self.labels {
            map.insert(
                "labels".into(),
                Value::Array(labels.iter().map(|l| Value::String(l.clone())).collect()),
            );
        }
        if let Some(ref assignees) = self.assignees {
            map.insert(
                "assignees".into(),
                Value::Array(assignees.iter().map(|a| Value::String(a.clone())).collect()),
            );
        }
        Value::Object(map)
    }
}

/// Low-level GitHub API client wrapping the `gh` CLI.
pub struct GhClient {
    owner: String,
    repo: String,
    token: String,
}

impl GhClient {
    pub fn new(owner: String, repo: String, token: String) -> Self {
        Self { owner, repo, token }
    }

    /// Execute a `gh api` call and parse the JSON response.
    fn call(&self, method: &str, endpoint: &str, body: Option<&Value>) -> TaskSourceResult<Value> {
        let mut cmd = Command::new("gh");
        cmd.arg("api")
            .arg("--method")
            .arg(method)
            .arg(endpoint)
            .env("GH_TOKEN", &self.token)
            .env("NO_COLOR", "1");

        if body.is_some() {
            cmd.arg("--input").arg("-");
            cmd.stdin(Stdio::piped());
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                TaskSourceError::Config(
                    "'gh' CLI not found — install from https://cli.github.com".into(),
                )
            } else {
                TaskSourceError::Other(Box::new(e))
            }
        })?;

        if let Some(body) = body {
            let body_str =
                serde_json::to_string(body).map_err(|e| TaskSourceError::Other(Box::new(e)))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(body_str.as_bytes())
                    .map_err(|e| TaskSourceError::Other(Box::new(e)))?;
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|e| TaskSourceError::Other(Box::new(e)))?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.trim().is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(stdout.trim()).map_err(|e| TaskSourceError::Other(Box::new(e)))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Err(map_gh_error(&stderr, output.status.code()))
        }
    }

    /// Execute a paginated `gh api` GET call.
    ///
    /// Uses `gh api --paginate` which automatically follows `Link: rel="next"`
    /// headers and merges array responses into a single JSON array.
    fn call_paginated(&self, endpoint: &str) -> TaskSourceResult<Value> {
        let mut cmd = Command::new("gh");
        cmd.arg("api")
            .arg("--paginate")
            .arg("--method")
            .arg("GET")
            .arg(endpoint)
            .env("GH_TOKEN", &self.token)
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    TaskSourceError::Config(
                        "'gh' CLI not found — install from https://cli.github.com".into(),
                    )
                } else {
                    TaskSourceError::Other(Box::new(e))
                }
            })?
            .wait_with_output()
            .map_err(|e| TaskSourceError::Other(Box::new(e)))?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.trim().is_empty() {
                return Ok(Value::Array(vec![]));
            }
            serde_json::from_str(stdout.trim()).map_err(|e| TaskSourceError::Other(Box::new(e)))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Err(map_gh_error(&stderr, output.status.code()))
        }
    }

    /// List all labels in the repository.
    pub fn list_labels(&self) -> TaskSourceResult<Vec<Label>> {
        let endpoint = format!("/repos/{}/{}/labels?per_page=100", self.owner, self.repo);
        let resp = self.call_paginated(&endpoint)?;
        serde_json::from_value(resp).map_err(|e| TaskSourceError::Other(Box::new(e)))
    }

    // -- Issue CRUD --

    /// List issues filtered by labels and state.
    ///
    /// Filters out pull requests (GitHub's Issues API includes them).
    pub fn list_issues(&self, labels: &[&str], state: &str) -> TaskSourceResult<Vec<GhIssue>> {
        let label_param = labels.join(",");
        let endpoint = format!(
            "/repos/{}/{}/issues?labels={}&state={}&per_page=100&sort=created&direction=asc",
            self.owner, self.repo, label_param, state
        );
        let resp = self.call_paginated(&endpoint)?;
        let issues: Vec<GhIssue> =
            serde_json::from_value(resp).map_err(|e| TaskSourceError::Other(Box::new(e)))?;
        // Filter out pull requests — they have a non-null `pull_request` key.
        Ok(issues
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// Create an issue.
    pub fn create_issue(
        &self,
        title: &str,
        body: &str,
        labels: &[String],
        assignees: &[String],
    ) -> TaskSourceResult<GhIssue> {
        let endpoint = format!("/repos/{}/{}/issues", self.owner, self.repo);
        let mut map = serde_json::Map::new();
        map.insert("title".into(), Value::String(title.into()));
        map.insert("body".into(), Value::String(body.into()));
        map.insert(
            "labels".into(),
            Value::Array(labels.iter().map(|l| Value::String(l.clone())).collect()),
        );
        if !assignees.is_empty() {
            map.insert(
                "assignees".into(),
                Value::Array(assignees.iter().map(|a| Value::String(a.clone())).collect()),
            );
        }
        let body_val = Value::Object(map);
        let resp = self.call("POST", &endpoint, Some(&body_val))?;
        serde_json::from_value(resp).map_err(|e| TaskSourceError::Other(Box::new(e)))
    }

    /// Update an issue. Only fields set in `updates` are sent.
    pub fn update_issue(&self, number: u64, updates: &IssueUpdate) -> TaskSourceResult<GhIssue> {
        let endpoint = format!("/repos/{}/{}/issues/{}", self.owner, self.repo, number);
        let body = updates.to_json();
        let resp = self.call("PATCH", &endpoint, Some(&body))?;
        serde_json::from_value(resp).map_err(|e| TaskSourceError::Other(Box::new(e)))
    }

    /// Get a single issue by number. Returns `None` if the issue doesn't exist.
    pub fn get_issue(&self, number: u64) -> TaskSourceResult<Option<GhIssue>> {
        let endpoint = format!("/repos/{}/{}/issues/{}", self.owner, self.repo, number);
        match self.call("GET", &endpoint, None) {
            Ok(resp) => {
                let issue: GhIssue = serde_json::from_value(resp)
                    .map_err(|e| TaskSourceError::Other(Box::new(e)))?;
                Ok(Some(issue))
            }
            Err(TaskSourceError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Create a label. Returns `Ok(())` if the label already exists (idempotent).
    pub fn create_label(&self, name: &str, color: &str, description: &str) -> TaskSourceResult<()> {
        let endpoint = format!("/repos/{}/{}/labels", self.owner, self.repo);
        let body = serde_json::json!({
            "name": name,
            "color": color,
            "description": description,
        });
        match self.call("POST", &endpoint, Some(&body)) {
            Ok(_) => Ok(()),
            Err(TaskSourceError::Config(msg)) if msg.contains("422") => {
                tracing::debug!(label = name, "label already exists, skipping");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// Map gh CLI stderr output to the appropriate [`TaskSourceError`] variant.
fn map_gh_error(stderr: &str, exit_code: Option<i32>) -> TaskSourceError {
    if let Some(status) = extract_http_status(stderr) {
        match status {
            401 | 403 => {
                TaskSourceError::Auth(format!("GitHub API auth failed (HTTP {status}): {stderr}"))
            }
            404 => TaskSourceError::NotFound(stderr.to_string()),
            422 => {
                TaskSourceError::Config(format!("GitHub API validation error (HTTP 422): {stderr}"))
            }
            429 => TaskSourceError::Retryable {
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("rate limited: {stderr}"),
                )),
                retry_after: extract_retry_after(stderr),
            },
            500..=599 => TaskSourceError::Retryable {
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("server error (HTTP {status}): {stderr}"),
                )),
                retry_after: None,
            },
            _ => TaskSourceError::Other(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("GitHub API error (HTTP {status}): {stderr}"),
            ))),
        }
    } else if stderr.contains("auth") || stderr.contains("login") || stderr.contains("token") {
        TaskSourceError::Auth(format!("GitHub auth issue: {stderr}"))
    } else {
        TaskSourceError::Other(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("gh api failed (exit {}): {stderr}", exit_code.unwrap_or(-1)),
        )))
    }
}

/// Extract HTTP status code from gh stderr (e.g. "HTTP 404", "(HTTP 401)").
fn extract_http_status(stderr: &str) -> Option<u16> {
    HTTP_STATUS_RE
        .captures(stderr)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Extract retry-after seconds from rate limit response.
fn extract_retry_after(stderr: &str) -> Option<Duration> {
    RETRY_AFTER_RE
        .captures(stderr)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_gh_error_401() {
        let err = map_gh_error("gh: authentication required (HTTP 401)", Some(1));
        assert!(matches!(err, TaskSourceError::Auth(_)));
        assert!(err.to_string().contains("401"));
    }

    #[test]
    fn map_gh_error_403() {
        let err = map_gh_error("gh: Resource not accessible (HTTP 403)", Some(1));
        assert!(matches!(err, TaskSourceError::Auth(_)));
        assert!(err.to_string().contains("403"));
    }

    #[test]
    fn map_gh_error_404() {
        let err = map_gh_error("gh: Not Found (HTTP 404)", Some(1));
        assert!(matches!(err, TaskSourceError::NotFound(_)));
    }

    #[test]
    fn map_gh_error_422() {
        let err = map_gh_error("gh: Validation Failed (HTTP 422)", Some(1));
        assert!(matches!(err, TaskSourceError::Config(_)));
        assert!(err.to_string().contains("422"));
    }

    #[test]
    fn map_gh_error_429_with_retry_after() {
        let err = map_gh_error(
            "gh: rate limit exceeded (HTTP 429) retry after 30 seconds",
            Some(1),
        );
        match err {
            TaskSourceError::Retryable {
                retry_after,
                source: _,
            } => {
                assert_eq!(retry_after, Some(Duration::from_secs(30)));
            }
            other => panic!("expected Retryable, got: {other:?}"),
        }
    }

    #[test]
    fn map_gh_error_500() {
        let err = map_gh_error("gh: Internal Server Error (HTTP 500)", Some(1));
        match err {
            TaskSourceError::Retryable {
                retry_after,
                source: _,
            } => {
                assert!(retry_after.is_none());
            }
            other => panic!("expected Retryable, got: {other:?}"),
        }
    }

    #[test]
    fn map_gh_error_auth_keywords() {
        for keyword in &["auth", "login", "token"] {
            let stderr = format!("failed to {keyword}: something went wrong");
            let err = map_gh_error(&stderr, Some(1));
            assert!(
                matches!(err, TaskSourceError::Auth(_)),
                "expected Auth for keyword '{keyword}', got: {err:?}"
            );
        }
    }

    #[test]
    fn map_gh_error_unknown() {
        let err = map_gh_error("something unexpected happened", Some(42));
        assert!(matches!(err, TaskSourceError::Other(_)));
        assert!(err.to_string().contains("exit 42"));
    }

    #[test]
    fn extract_http_status_various_patterns() {
        assert_eq!(extract_http_status("HTTP 404"), Some(404));
        assert_eq!(extract_http_status("(HTTP 401)"), Some(401));
        assert_eq!(extract_http_status("gh: Not Found (HTTP 404)"), Some(404));
        assert_eq!(extract_http_status("no status here"), None);
        assert_eq!(extract_http_status(""), None);
    }

    #[test]
    fn extract_retry_after_various_patterns() {
        assert_eq!(
            extract_retry_after("retry after 60 seconds"),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            extract_retry_after("Retry-After: 30"),
            Some(Duration::from_secs(30))
        );
        assert_eq!(extract_retry_after("no retry info"), None);
    }

    #[test]
    fn gh_issue_deserialize_full() {
        let json = serde_json::json!({
            "number": 42,
            "title": "Fix the thing",
            "body": "Detailed description",
            "state": "open",
            "labels": [{"name": "bug", "color": "d73a4a"}],
            "created_at": "2026-01-15T10:00:00Z",
            "closed_at": "2026-01-16T12:00:00Z",
            "assignees": [{"login": "alice"}],
            "milestone": {"number": 3, "title": "v1.0"}
        });
        let issue: GhIssue = serde_json::from_value(json).unwrap();
        assert_eq!(issue.number, 42);
        assert_eq!(issue.title, "Fix the thing");
        assert_eq!(issue.body.as_deref(), Some("Detailed description"));
        assert_eq!(issue.state, "open");
        assert_eq!(issue.labels.len(), 1);
        assert_eq!(issue.labels[0].name, "bug");
        assert_eq!(issue.closed_at.as_deref(), Some("2026-01-16T12:00:00Z"));
        assert_eq!(issue.assignees.len(), 1);
        assert_eq!(issue.assignees[0].login, "alice");
        assert_eq!(issue.milestone.as_ref().unwrap().title, "v1.0");
        assert!(issue.pull_request.is_none());
    }

    #[test]
    fn gh_issue_deserialize_minimal() {
        let json = serde_json::json!({
            "number": 1,
            "title": "Simple issue",
            "state": "closed",
            "labels": [],
            "created_at": "2026-01-01T00:00:00Z"
        });
        let issue: GhIssue = serde_json::from_value(json).unwrap();
        assert_eq!(issue.number, 1);
        assert!(issue.body.is_none());
        assert!(issue.closed_at.is_none());
        assert!(issue.assignees.is_empty());
        assert!(issue.milestone.is_none());
    }

    #[test]
    fn issue_update_to_json_full() {
        let update = IssueUpdate {
            title: Some("New title".into()),
            body: Some("New body".into()),
            state: Some("closed".into()),
            labels: Some(vec!["bug".into(), "urgent".into()]),
            assignees: Some(vec!["alice".into()]),
        };
        let json = update.to_json();
        assert_eq!(json["title"], "New title");
        assert_eq!(json["body"], "New body");
        assert_eq!(json["state"], "closed");
        assert_eq!(json["labels"], serde_json::json!(["bug", "urgent"]));
        assert_eq!(json["assignees"], serde_json::json!(["alice"]));
    }

    #[test]
    fn issue_update_to_json_partial() {
        let update = IssueUpdate {
            labels: Some(vec!["status/done".into()]),
            ..Default::default()
        };
        let json = update.to_json();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert_eq!(json["labels"], serde_json::json!(["status/done"]));
    }

    #[test]
    fn issue_update_to_json_empty() {
        let update = IssueUpdate::default();
        let json = update.to_json();
        let obj = json.as_object().unwrap();
        assert!(obj.is_empty());
    }

    #[test]
    fn empty_array_deserializes_to_empty_issues() {
        let empty = Value::Array(vec![]);
        let issues: Vec<GhIssue> = serde_json::from_value(empty).unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn list_issues_pr_filter_logic() {
        // Simulate the filter logic used in list_issues: issues with a
        // non-null `pull_request` field should be excluded.
        let issue1 = serde_json::from_value::<GhIssue>(serde_json::json!({
            "number": 1,
            "title": "A real issue",
            "state": "open",
            "labels": [],
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        let pr = serde_json::from_value::<GhIssue>(serde_json::json!({
            "number": 2,
            "title": "A pull request",
            "state": "open",
            "labels": [],
            "created_at": "2026-01-01T00:00:00Z",
            "pull_request": {"url": "https://api.github.com/repos/o/r/pulls/2"}
        }))
        .unwrap();
        let issue3 = serde_json::from_value::<GhIssue>(serde_json::json!({
            "number": 3,
            "title": "Another issue",
            "state": "closed",
            "labels": [{"name": "bug", "color": "d73a4a"}],
            "created_at": "2026-01-02T00:00:00Z"
        }))
        .unwrap();
        let issues = [issue1, pr, issue3];

        let filtered: Vec<&GhIssue> = issues.iter().filter(|i| i.pull_request.is_none()).collect();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].number, 1);
        assert_eq!(filtered[1].number, 3);
    }
}
