//! Registry of task source connector factories.
//!
//! Maps type names (e.g. `"jsonl"`) to factory functions that create
//! [`TaskSource`] implementations from configuration.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::config::TaskSourceConfig;
use crate::task_source::{TaskSource, TaskSourceError, TaskSourceResult};
use crate::task_sources::{GithubTaskSource, JsonlTaskSource};

/// Factory function that creates a boxed [`TaskSource`] from config and workspace root.
pub type ConnectorFactory =
    Box<dyn Fn(&Value, &Path) -> TaskSourceResult<Box<dyn TaskSource>> + Send>;

/// Registry mapping task source type names to their factory functions.
pub struct TaskSourceRegistry {
    factories: HashMap<String, ConnectorFactory>,
}

impl Default for TaskSourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskSourceRegistry {
    /// Create a new registry with the built-in `"jsonl"` connector registered.
    pub fn new() -> Self {
        let mut registry = Self {
            factories: HashMap::new(),
        };

        let jsonl_factory: ConnectorFactory = Box::new(|config, workspace_root| {
            let source = JsonlTaskSource::from_config(config, workspace_root)?;
            Ok(Box::new(source))
        });
        registry
            .factories
            .insert("jsonl".to_string(), jsonl_factory);

        let github_factory: ConnectorFactory = Box::new(|config, workspace_root| {
            let source = GithubTaskSource::from_config(config, workspace_root)?;
            Ok(Box::new(source))
        });
        registry
            .factories
            .insert("github".to_string(), github_factory);

        registry
    }

    /// Register a custom connector factory under the given type name.
    pub fn register(&mut self, name: &str, factory: ConnectorFactory) {
        self.factories.insert(name.to_string(), factory);
    }

    /// Create a [`TaskSource`] from the given config, calling `setup()` after construction.
    pub fn create(
        &self,
        config: &TaskSourceConfig,
        workspace_root: &Path,
    ) -> TaskSourceResult<Box<dyn TaskSource>> {
        let (type_name, connector_config) = match config {
            TaskSourceConfig::Named(name) => (name.as_str(), Value::Null),
            TaskSourceConfig::Typed {
                source_type,
                config,
            } => (source_type.as_str(), config.clone()),
        };

        let factory = self.factories.get(type_name).ok_or_else(|| {
            TaskSourceError::Config(format!("unknown task source type: {type_name}"))
        })?;

        let mut source = factory(&connector_config, workspace_root)?;
        source.setup()?;
        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_registers_jsonl() {
        let registry = TaskSourceRegistry::new();
        assert!(registry.factories.contains_key("jsonl"));
    }

    #[test]
    fn create_with_named_config() {
        let registry = TaskSourceRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let config = TaskSourceConfig::Named("jsonl".to_string());

        let source = registry.create(&config, dir.path());
        assert!(source.is_ok());
    }

    #[test]
    fn create_with_typed_config() {
        let registry = TaskSourceRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let config = TaskSourceConfig::Typed {
            source_type: "jsonl".to_string(),
            config: serde_json::json!({}),
        };

        let source = registry.create(&config, dir.path());
        assert!(source.is_ok());
    }

    #[test]
    fn create_with_typed_config_custom_path() {
        let registry = TaskSourceRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let custom_path = dir.path().join("custom/tasks.jsonl");
        let config = TaskSourceConfig::Typed {
            source_type: "jsonl".to_string(),
            config: serde_json::json!({ "path": custom_path.to_str().unwrap() }),
        };

        let source = registry.create(&config, dir.path());
        assert!(source.is_ok());
    }

    #[test]
    fn create_unknown_type_returns_config_error() {
        let registry = TaskSourceRegistry::new();
        let config = TaskSourceConfig::Named("nonexistent".to_string());

        let result = registry.create(&config, Path::new("/tmp"));
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(
            matches!(err, TaskSourceError::Config(ref msg) if msg.contains("unknown task source type: nonexistent")),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn register_custom_factory() {
        let mut registry = TaskSourceRegistry::new();

        let custom_factory: ConnectorFactory = Box::new(|_config, workspace_root| {
            // Reuse jsonl under the hood for testing
            let source = JsonlTaskSource::from_config(&Value::Null, workspace_root)?;
            Ok(Box::new(source))
        });
        registry.register("custom", custom_factory);

        assert!(registry.factories.contains_key("custom"));

        let dir = tempfile::tempdir().unwrap();
        let config = TaskSourceConfig::Named("custom".to_string());
        let source = registry.create(&config, dir.path());
        assert!(source.is_ok());
    }

    #[test]
    fn create_calls_setup() {
        // Setup for jsonl creates parent directories — verify the side effect
        let dir = tempfile::tempdir().unwrap();
        let tasks_dir = dir.path().join("deep/nested");
        let tasks_path = tasks_dir.join("tasks.jsonl");

        let registry = TaskSourceRegistry::new();
        let config = TaskSourceConfig::Typed {
            source_type: "jsonl".to_string(),
            config: serde_json::json!({ "path": tasks_path.to_str().unwrap() }),
        };

        registry.create(&config, dir.path()).unwrap();
        // setup() should have created the parent directory
        assert!(tasks_dir.exists(), "setup() should create parent dirs");
    }

    #[test]
    fn new_registers_github() {
        let registry = TaskSourceRegistry::new();
        assert!(registry.factories.contains_key("github"));
    }

    #[test]
    fn create_github_named_null_config_errors() {
        let registry = TaskSourceRegistry::new();
        let config = TaskSourceConfig::Named("github".to_string());
        let result = registry.create(&config, Path::new("/tmp"));
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(
            matches!(err, TaskSourceError::Config(ref msg) if msg.contains("requires config")),
            "expected Config error about requires config, got: {err:?}"
        );
    }

    #[test]
    fn create_github_typed_missing_repo_errors() {
        let registry = TaskSourceRegistry::new();
        let config = TaskSourceConfig::Typed {
            source_type: "github".to_string(),
            config: serde_json::json!({}),
        };
        let result = registry.create(&config, Path::new("/tmp"));
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(
            matches!(err, TaskSourceError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn default_config_creates_jsonl_source() {
        let registry = TaskSourceRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let config = TaskSourceConfig::default();

        let source = registry.create(&config, dir.path());
        assert!(source.is_ok());
    }
}
