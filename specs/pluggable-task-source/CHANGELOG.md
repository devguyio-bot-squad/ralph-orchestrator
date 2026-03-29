# Pluggable Task Source — CHANGELOG

## 2026-03-29

### Added
- `TaskSource` trait (`Send`) with 12 methods: setup, refresh, add, close, reopen, fail, get, get_by_key, all, open, pending, ready
- `TaskSourceError` with 6 typed variants: Auth, NotFound, Config, Retryable, RateLimit, Other — each with `on_error` strategy (fail/skip/retry)
- `JsonlTaskSource` — file-backed implementation replacing legacy `TaskStore`, with write-through semantics and owned returns
- `GitHubTaskSource` — GitHub Issues connector with label/milestone filtering, ETag caching, rate limit handling, Projects v2 column sync
- `MockTaskSource` — test double with error injection (`inject_error`), call tracking (`call_count`), and `RefCell`-based interior mutability for query methods
- `TaskSourceConfig` with `source` enum (jsonl/github), `on_error` strategies, and `meta` key-value pairs
- `create_task_source()` factory function for config-driven instantiation
- `--meta KEY=VALUE` CLI flag on `task add`, `task ensure`, `task close`, `task start`, `task reopen`, `task fail`
- `metadata: HashMap<String, String>` field on `Task` struct
- Event loop integration: `prepend_ready_tasks`, `verify_tasks_complete`, `count_tasks` all route through `TaskSource`
- 120+ new tests across unit, integration, and error-path coverage

### Changed
- `Task::is_ready()` — failed blockers now unblock dependents (was: only closed)
- `verify_tasks_complete` — uses `pending()` semantics instead of `open().is_empty()`
- Event loop field `task_source` is now actively used (removed stale `#[allow(dead_code)]`)

### Removed
- `TaskStore` struct and `task_store.rs` module (666 lines) — fully replaced by `JsonlTaskSource`

### Crates Affected
- **ralph-core**: TaskSource trait, JsonlTaskSource, MockTaskSource, GitHubTaskSource, config, factory, event loop integration
- **ralph-cli**: `--meta` flag, task source wiring through CLI commands
- **ralph-adapters**: (no changes)

## Suggested AGENTS.md Updates

1. **Task source pattern**: New code touching task operations should use the `TaskSource` trait, not direct file I/O. The factory function `create_task_source()` in `task_sources/mod.rs` handles instantiation from config.

2. **Error handling convention**: `TaskSourceError` uses typed variants with `on_error` strategies (fail/skip/retry). Follow this pattern for new connectors — errors carry both the cause and the recommended recovery action.

3. **Testing with MockTaskSource**: Use `MockTaskSource` for unit/integration tests. It supports error injection via `inject_error("method", MockError::...)` and call tracking via `call_count("method")`. Query methods use `RefCell` for interior mutability.

4. **GitHub connector caching**: `GitHubTaskSource` uses ETag-based HTTP caching. When adding new API calls, include `If-None-Match` headers and handle 304 responses.

5. **Key files update**: Task system code is now in `crates/ralph-core/src/task.rs` + `task_sources/` (not `task_store.rs`).
