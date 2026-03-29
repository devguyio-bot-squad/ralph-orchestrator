//! Parse and write Ralph metadata in GitHub issue bodies.
//!
//! Ralph stores internal fields (blocked_by, started, key, etc.) as an
//! invisible HTML comment in the issue body:
//!
//! ```text
//! User-visible description here.
//!
//! <!-- ralph:metadata:v1 {"blocked_by":["41","43"]} -->
//! ```
//!
//! The comment is invisible when rendered on GitHub but survives round-trips
//! through the API. Functions in this module never fail — they degrade
//! gracefully to defaults on malformed input.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// Regex matching the metadata comment. Captures the JSON payload.
static METADATA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<!-- ralph:metadata:v\d+ (.*?) -->").expect("metadata regex is valid")
});

/// Ralph-internal metadata stored in a GitHub issue body as an HTML comment.
///
/// Fields that cannot be represented in GitHub's native data model are
/// round-tripped through this struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IssueMetadata {
    /// Task IDs (issue numbers as strings) this task is blocked by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,

    /// ISO 8601 timestamp when the task was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,

    /// ISO 8601 timestamp when the task was closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<String>,

    /// Stable key for idempotent task identification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// Loop ID that created/owns this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_id: Option<String>,

    /// Original priority (1–5). Labels also encode priority but this is the
    /// canonical source for round-tripping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
}

impl IssueMetadata {
    /// Returns `true` when every field is at its default value.
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Extract [`IssueMetadata`] from an issue body.
///
/// If the body contains multiple metadata comments the **last** one wins.
/// Malformed JSON is logged as a warning and returns the default (empty)
/// metadata — this function never panics or errors.
pub fn parse_metadata(body: &str) -> IssueMetadata {
    let json_str = match METADATA_RE.find_iter(body).last() {
        Some(m) => {
            // Re-capture to get the inner group from the last match.
            let caps = METADATA_RE.captures(m.as_str()).expect("already matched");
            caps.get(1).expect("group 1 exists").as_str().to_string()
        }
        None => return IssueMetadata::default(),
    };

    match serde_json::from_str::<IssueMetadata>(&json_str) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::warn!(
                json = %json_str,
                error = %e,
                "failed to parse ralph metadata comment — using defaults"
            );
            IssueMetadata::default()
        }
    }
}

/// Write (or replace) the metadata comment in an issue body.
///
/// - If a metadata comment already exists it is replaced in-place.
/// - If the metadata is all-default, any existing comment is removed and no
///   new one is appended (keeps the body clean).
/// - Otherwise the comment is appended after a blank line.
pub fn write_metadata(body: &str, metadata: &IssueMetadata) -> String {
    let has_existing = METADATA_RE.is_match(body);

    if metadata.is_empty() {
        // Remove existing comment if present; otherwise return body as-is.
        if has_existing {
            return strip_metadata(body);
        }
        return body.to_string();
    }

    let json = serde_json::to_string(metadata).expect("IssueMetadata is always serializable");
    let comment = format!("<!-- ralph:metadata:v1 {json} -->");

    if has_existing {
        METADATA_RE.replace_all(body, comment.as_str()).to_string()
    } else if body.is_empty() {
        comment
    } else {
        format!("{body}\n\n{comment}")
    }
}

/// Strip the metadata comment from an issue body, returning only
/// user-visible text. Trailing whitespace left by removal is trimmed.
pub fn strip_metadata(body: &str) -> String {
    let stripped = METADATA_RE.replace_all(body, "");
    stripped.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_body() {
        assert_eq!(parse_metadata(""), IssueMetadata::default());
    }

    #[test]
    fn parse_no_comment() {
        let body = "Just a plain issue description.\n\nWith multiple paragraphs.";
        assert_eq!(parse_metadata(body), IssueMetadata::default());
    }

    #[test]
    fn parse_valid_comment() {
        let body = concat!(
            "Fix the login bug.\n\n",
            "<!-- ralph:metadata:v1 {",
            r#""blocked_by":["41","43"],"#,
            r#""started":"2026-03-28T10:00:00Z","#,
            r#""closed":"2026-03-28T12:00:00Z","#,
            r#""key":"spec:auth-01","#,
            r#""loop_id":"loop-abc","#,
            r#""priority":2"#,
            "} -->"
        );
        let meta = parse_metadata(body);
        assert_eq!(meta.blocked_by, vec!["41", "43"]);
        assert_eq!(meta.started.as_deref(), Some("2026-03-28T10:00:00Z"));
        assert_eq!(meta.closed.as_deref(), Some("2026-03-28T12:00:00Z"));
        assert_eq!(meta.key.as_deref(), Some("spec:auth-01"));
        assert_eq!(meta.loop_id.as_deref(), Some("loop-abc"));
        assert_eq!(meta.priority, Some(2));
    }

    #[test]
    fn parse_partial_fields() {
        let body = r#"Desc.

<!-- ralph:metadata:v1 {"blocked_by":["7"]} -->"#;
        let meta = parse_metadata(body);
        assert_eq!(meta.blocked_by, vec!["7"]);
        assert!(meta.started.is_none());
        assert!(meta.key.is_none());
        assert!(meta.priority.is_none());
    }

    #[test]
    fn parse_multiple_comments_last_wins() {
        let body = concat!(
            "<!-- ralph:metadata:v1 {\"key\":\"first\"} -->\n",
            "Some text.\n",
            "<!-- ralph:metadata:v1 {\"key\":\"second\"} -->"
        );
        let meta = parse_metadata(body);
        assert_eq!(meta.key.as_deref(), Some("second"));
    }

    #[test]
    fn parse_corrupt_json_returns_default() {
        let body = "<!-- ralph:metadata:v1 {not valid json} -->";
        let meta = parse_metadata(body);
        assert_eq!(meta, IssueMetadata::default());
    }

    #[test]
    fn write_to_empty_body() {
        let meta = IssueMetadata {
            key: Some("k1".to_string()),
            ..Default::default()
        };
        let result = write_metadata("", &meta);
        assert!(result.starts_with("<!-- ralph:metadata:v1 "));
        assert!(result.contains(r#""key":"k1""#));
        assert!(result.ends_with(" -->"));
    }

    #[test]
    fn write_to_existing_body() {
        let meta = IssueMetadata {
            blocked_by: vec!["5".to_string()],
            ..Default::default()
        };
        let result = write_metadata("Fix the bug.", &meta);
        assert!(result.starts_with("Fix the bug.\n\n<!-- ralph:metadata:v1 "));
    }

    #[test]
    fn write_replaces_existing() {
        let body = "Desc.\n\n<!-- ralph:metadata:v1 {\"key\":\"old\"} -->";
        let meta = IssueMetadata {
            key: Some("new".to_string()),
            ..Default::default()
        };
        let result = write_metadata(body, &meta);
        assert!(result.contains(r#""key":"new""#));
        assert!(!result.contains(r#""key":"old""#));
        // Should only have one comment.
        assert_eq!(result.matches("<!-- ralph:metadata:v1").count(), 1);
    }

    #[test]
    fn write_default_metadata_no_comment() {
        let body = "Clean issue body.";
        let result = write_metadata(body, &IssueMetadata::default());
        assert_eq!(result, body);
        assert!(!result.contains("ralph:metadata"));
    }

    #[test]
    fn round_trip() {
        let original = IssueMetadata {
            blocked_by: vec!["10".into(), "20".into()],
            started: Some("2026-03-28T10:00:00Z".into()),
            closed: None,
            key: Some("spec:task-01".into()),
            loop_id: Some("loop-xyz".into()),
            priority: Some(1),
        };
        let body = write_metadata("Issue description.", &original);
        let parsed = parse_metadata(&body);
        assert_eq!(parsed, original);
    }

    #[test]
    fn strip_metadata_preserves_user_text() {
        let body = "User text here.\n\n<!-- ralph:metadata:v1 {\"key\":\"x\"} -->";
        let stripped = strip_metadata(body);
        assert_eq!(stripped, "User text here.");
        assert!(!stripped.contains("ralph:metadata"));
    }
}
