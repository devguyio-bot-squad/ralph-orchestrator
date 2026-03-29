//! GitHub connector configuration.
//!
//! Deserialized from the opaque `serde_json::Value` passed by the task source
//! registry's `from_config()`.

use serde::{Deserialize, Serialize};

/// GitHub connector configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubTaskSourceConfig {
    /// Repository in "owner/repo" format (required).
    pub repo: String,

    /// Operating mode: "simple" (default) or "projects-v2".
    #[serde(default)]
    pub mode: GithubMode,

    /// Projects v2 config (projects-v2 mode only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectV2Config>,

    /// Caching config.
    #[serde(default)]
    pub cache: CacheConfig,

    /// Behavior when GitHub API is unreachable.
    #[serde(default)]
    pub on_error: ErrorBehavior,

    /// Auth token override. If unset, uses env vars or `gh auth token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl GithubTaskSourceConfig {
    /// Validate config constraints. Called by `from_config()`.
    pub fn validate(&self) -> Result<(), String> {
        if !self.repo.contains('/') {
            return Err(format!(
                "repo must be in 'owner/repo' format, got: {}",
                self.repo
            ));
        }
        if matches!(self.on_error, ErrorBehavior::UseCached) && !self.cache.enabled {
            return Err("on_error: use-cached requires cache.enabled: true".to_string());
        }
        if matches!(self.mode, GithubMode::ProjectsV2) && self.project.is_none() {
            return Err("projects-v2 mode requires project config".to_string());
        }
        Ok(())
    }

    /// Parse owner and repo from the "owner/repo" format.
    ///
    /// Panics if `repo` has not been validated (no `/`).
    pub fn owner_repo(&self) -> (&str, &str) {
        self.repo
            .split_once('/')
            .expect("repo must contain '/' — call validate() first")
    }
}

/// GitHub operating mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GithubMode {
    #[default]
    Simple,
    ProjectsV2,
}

/// Projects v2 configuration (required for projects-v2 mode).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectV2Config {
    /// Org that owns the project (for org-level projects).
    pub org: Option<String>,
    /// Project number.
    pub number: u32,
}

/// Caching configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheConfig {
    /// Whether caching is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Cache TTL in seconds (default: 60).
    #[serde(default = "default_cache_ttl")]
    pub ttl_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_seconds: 60,
        }
    }
}

fn default_cache_ttl() -> u64 {
    60
}

/// Error handling strategy when the GitHub API is unreachable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorBehavior {
    /// Stop the loop with an error (default).
    #[default]
    Fail,
    /// Use last cached state. Requires `cache.enabled: true`.
    UseCached,
    /// Log warning and return empty task list.
    Warn,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_config() {
        let json = r#"{"repo": "acme/widgets"}"#;
        let config: GithubTaskSourceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.repo, "acme/widgets");
        assert_eq!(config.mode, GithubMode::Simple);
        assert!(!config.cache.enabled);
        assert_eq!(config.cache.ttl_seconds, 60);
        assert_eq!(config.on_error, ErrorBehavior::Fail);
        assert!(config.token.is_none());
        assert!(config.project.is_none());
    }

    #[test]
    fn deserialize_full_config() {
        let json = r#"{
            "repo": "acme/widgets",
            "mode": "projects-v2",
            "project": {"org": "acme", "number": 42},
            "cache": {"enabled": true, "ttl_seconds": 120},
            "on_error": "use-cached",
            "token": "ghp_secret"
        }"#;
        let config: GithubTaskSourceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.mode, GithubMode::ProjectsV2);
        assert_eq!(
            config.project,
            Some(ProjectV2Config {
                org: Some("acme".to_string()),
                number: 42,
            })
        );
        assert!(config.cache.enabled);
        assert_eq!(config.cache.ttl_seconds, 120);
        assert_eq!(config.on_error, ErrorBehavior::UseCached);
        assert_eq!(config.token, Some("ghp_secret".to_string()));
    }

    #[test]
    fn default_mode_is_simple() {
        let json = r#"{"repo": "a/b"}"#;
        let config: GithubTaskSourceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.mode, GithubMode::Simple);
    }

    #[test]
    fn default_cache_is_disabled_with_ttl_60() {
        let cache = CacheConfig::default();
        assert!(!cache.enabled);
        assert_eq!(cache.ttl_seconds, 60);
    }

    #[test]
    fn default_on_error_is_fail() {
        let behavior = ErrorBehavior::default();
        assert_eq!(behavior, ErrorBehavior::Fail);
    }

    #[test]
    fn validate_repo_without_slash() {
        let config = GithubTaskSourceConfig {
            repo: "no-slash".to_string(),
            mode: GithubMode::default(),
            project: None,
            cache: CacheConfig::default(),
            on_error: ErrorBehavior::default(),
            token: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("owner/repo"), "got: {err}");
    }

    #[test]
    fn validate_use_cached_without_cache_enabled() {
        let config = GithubTaskSourceConfig {
            repo: "a/b".to_string(),
            mode: GithubMode::default(),
            project: None,
            cache: CacheConfig::default(),
            on_error: ErrorBehavior::UseCached,
            token: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("cache.enabled"), "got: {err}");
    }

    #[test]
    fn validate_projects_v2_without_project_config() {
        let config = GithubTaskSourceConfig {
            repo: "a/b".to_string(),
            mode: GithubMode::ProjectsV2,
            project: None,
            cache: CacheConfig::default(),
            on_error: ErrorBehavior::default(),
            token: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("project config"), "got: {err}");
    }

    #[test]
    fn owner_repo_parses_correctly() {
        let config = GithubTaskSourceConfig {
            repo: "acme/widgets".to_string(),
            mode: GithubMode::default(),
            project: None,
            cache: CacheConfig::default(),
            on_error: ErrorBehavior::default(),
            token: None,
        };
        assert_eq!(config.owner_repo(), ("acme", "widgets"));
    }

    #[test]
    fn validate_valid_config_passes() {
        let config = GithubTaskSourceConfig {
            repo: "acme/widgets".to_string(),
            mode: GithubMode::ProjectsV2,
            project: Some(ProjectV2Config {
                org: Some("acme".to_string()),
                number: 1,
            }),
            cache: CacheConfig {
                enabled: true,
                ttl_seconds: 30,
            },
            on_error: ErrorBehavior::UseCached,
            token: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn round_trip_serialization() {
        let config = GithubTaskSourceConfig {
            repo: "acme/widgets".to_string(),
            mode: GithubMode::Simple,
            project: None,
            cache: CacheConfig::default(),
            on_error: ErrorBehavior::Warn,
            token: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: GithubTaskSourceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.repo, config.repo);
        assert_eq!(back.on_error, ErrorBehavior::Warn);
    }
}
