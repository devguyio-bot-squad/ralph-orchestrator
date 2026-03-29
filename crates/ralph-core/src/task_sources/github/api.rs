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

    /// List all labels in the repository.
    pub fn list_labels(&self) -> TaskSourceResult<Vec<Label>> {
        let endpoint = format!("/repos/{}/{}/labels?per_page=100", self.owner, self.repo);
        let resp = self.call("GET", &endpoint, None)?;
        serde_json::from_value(resp).map_err(|e| TaskSourceError::Other(Box::new(e)))
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
}
