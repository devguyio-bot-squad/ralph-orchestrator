//! CLI commands for the `ralph task` namespace.
//!
//! Provides subcommands for managing tasks:
//! - `add`: Create a new task
//! - `ensure`: Create or reuse a keyed task
//! - `list`: List all tasks
//! - `ready`: Show unblocked tasks
//! - `start`: Mark a task as in progress
//! - `close`: Mark a task as complete
//! - `reopen`: Reopen a closed/failed task
//! - `show`: Show a single task by ID

use crate::{display::colors, resolve_workspace_root};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use ralph_core::{Task, TaskStatus};
use std::collections::HashMap;
use std::path::PathBuf;

/// Output format for task commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table format
    #[default]
    Table,
    /// JSON format for programmatic access
    Json,
    /// ID-only output for scripting
    Quiet,
}

/// Task management commands for tracking work items.
#[derive(Parser, Debug)]
pub struct TaskArgs {
    #[command(subcommand)]
    pub command: TaskCommands,

    /// Working directory (default: current directory)
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum TaskCommands {
    /// Create a new task
    Add(AddArgs),

    /// Create or reuse a task by stable key
    Ensure(EnsureArgs),

    /// List all tasks
    List(ListArgs),

    /// Show unblocked tasks
    Ready(ReadyArgs),

    /// Mark a task as in progress
    Start(StartArgs),

    /// Mark a task as complete
    Close(CloseArgs),

    /// Mark a task as failed
    Fail(FailArgs),

    /// Reopen a closed or failed task
    Reopen(ReopenArgs),

    /// Show a single task by ID
    Show(ShowArgs),
}

/// Arguments for the `task add` command.
#[derive(Parser, Debug)]
pub struct AddArgs {
    /// Task title
    pub title: String,

    /// Priority (1-5, default 3)
    #[arg(short = 'p', long, default_value = "3")]
    pub priority: u8,

    /// Task description
    #[arg(short = 'd', long)]
    pub description: Option<String>,

    /// Task IDs that must complete first (comma-separated)
    #[arg(long)]
    pub blocked_by: Option<String>,

    /// Metadata key=value pairs (repeatable)
    #[arg(long = "meta", value_name = "KEY=VALUE")]
    pub meta: Vec<String>,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Arguments for the `task ensure` command.
#[derive(Parser, Debug)]
pub struct EnsureArgs {
    /// Task title
    pub title: String,

    /// Stable key used to deduplicate orchestrator-managed tasks
    #[arg(long)]
    pub key: String,

    /// Priority (1-5, default 3)
    #[arg(short = 'p', long, default_value = "3")]
    pub priority: u8,

    /// Task description
    #[arg(short = 'd', long)]
    pub description: Option<String>,

    /// Task IDs that must complete first (comma-separated)
    #[arg(long)]
    pub blocked_by: Option<String>,

    /// Metadata key=value pairs (repeatable)
    #[arg(long = "meta", value_name = "KEY=VALUE")]
    pub meta: Vec<String>,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Arguments for the `task list` command.
#[derive(Parser, Debug)]
pub struct ListArgs {
    /// Filter by status: open, in_progress, closed, failed
    #[arg(short = 's', long)]
    pub status: Option<String>,

    /// Show only tasks from the last N days
    #[arg(long, short = 'd')]
    pub days: Option<i64>,

    /// Limit the number of tasks displayed
    #[arg(long, short = 'l')]
    pub limit: Option<usize>,

    /// Show all tasks including closed and failed (hidden by default)
    #[arg(long, short = 'a')]
    pub all: bool,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Arguments for the `task ready` command.
#[derive(Parser, Debug)]
pub struct ReadyArgs {
    /// Show tasks from all loops, not just the current one
    #[arg(long, short = 'a')]
    pub all: bool,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Arguments for the `task start` command.
#[derive(Parser, Debug)]
pub struct StartArgs {
    /// Task ID to mark as in progress
    pub id: String,
}

/// Arguments for the `task close` command.
#[derive(Parser, Debug)]
pub struct CloseArgs {
    /// Task ID to close
    pub id: String,
}

/// Arguments for the `task fail` command.
#[derive(Parser, Debug)]
pub struct FailArgs {
    /// Task ID to mark as failed
    pub id: String,
}

/// Arguments for the `task reopen` command.
#[derive(Parser, Debug)]
pub struct ReopenArgs {
    /// Task ID to reopen
    pub id: String,
}

/// Arguments for the `task show` command.
#[derive(Parser, Debug)]
pub struct ShowArgs {
    /// Task ID
    pub id: String,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

fn read_current_loop_id(root: Option<&PathBuf>) -> Option<String> {
    let loop_id_marker = resolve_workspace_root(root).join(".ralph/current-loop-id");

    let loop_id = std::fs::read_to_string(loop_id_marker).ok()?;
    let loop_id = loop_id.trim().to_string();
    (!loop_id.is_empty()).then_some(loop_id)
}

fn add_common_task_fields(
    mut task: Task,
    root: Option<&PathBuf>,
    description: Option<String>,
    blocked_by: Option<String>,
) -> Task {
    if let Some(loop_id) = read_current_loop_id(root) {
        task = task.with_loop_id(Some(loop_id));
    }

    if let Some(desc) = description {
        task = task.with_description(Some(desc));
    }

    if let Some(blockers) = blocked_by {
        for blocker_id in blockers
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            task = task.with_blocker(blocker_id.to_string());
        }
    }

    task
}

/// Parses `--meta key=value` arguments into a metadata map.
///
/// Splits on the first `=` only, so `--meta filter=status=open` becomes
/// key=`filter`, value=`status=open`. Entries without `=` are silently ignored.
/// All values are stored as `serde_json::Value::String`.
fn parse_meta(meta_args: &[String]) -> HashMap<String, serde_json::Value> {
    let mut map = HashMap::new();
    for entry in meta_args {
        if let Some((key, value)) = entry.split_once('=') {
            map.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
    map
}

fn status_matches_filter(status: TaskStatus, filter: &str) -> bool {
    let normalized = filter.to_lowercase().replace(['_', '-'], "");
    match status {
        TaskStatus::Open => normalized == "open",
        TaskStatus::InProgress => normalized == "inprogress",
        TaskStatus::Closed => normalized == "closed",
        TaskStatus::Failed => normalized == "failed",
    }
}

fn filter_tasks_for_list(tasks: Vec<Task>, args: &ListArgs) -> Vec<Task> {
    let mut tasks: Vec<_> = if let Some(status_str) = args.status.as_deref() {
        tasks
            .into_iter()
            .filter(|t| status_matches_filter(t.status, status_str))
            .collect()
    } else if args.all {
        tasks
    } else {
        tasks
            .into_iter()
            .filter(|t| !matches!(t.status, TaskStatus::Closed | TaskStatus::Failed))
            .collect()
    };

    if let Some(days) = args.days {
        let cutoff = Utc::now() - chrono::Duration::days(days);
        tasks.retain(|t| {
            if DateTime::parse_from_rfc3339(&t.created)
                .map(|c| c.with_timezone(&Utc) > cutoff)
                .unwrap_or(false)
            {
                return true;
            }

            if t.closed.as_ref().is_some_and(|closed_str| {
                DateTime::parse_from_rfc3339(closed_str)
                    .map(|c| c.with_timezone(&Utc) > cutoff)
                    .unwrap_or(false)
            }) {
                return true;
            }
            false
        });
    }

    tasks.sort_by(|a, b| {
        let status_rank = |s: TaskStatus| match s {
            TaskStatus::InProgress => 0,
            TaskStatus::Open => 1,
            TaskStatus::Closed => 2,
            TaskStatus::Failed => 3,
        };

        let rank_a = status_rank(a.status);
        let rank_b = status_rank(b.status);

        if rank_a != rank_b {
            return rank_a.cmp(&rank_b);
        }

        if a.priority != b.priority {
            return a.priority.cmp(&b.priority);
        }

        a.created.cmp(&b.created)
    });

    if let Some(limit) = args.limit {
        tasks.truncate(limit);
    }

    tasks
}

/// Executes task CLI commands.
pub fn execute(
    args: TaskArgs,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let root = args.root.clone();

    match args.command {
        TaskCommands::Add(add_args) => {
            execute_add(add_args, root.as_ref(), config_sources, use_colors)
        }
        TaskCommands::Ensure(ensure_args) => {
            execute_ensure(ensure_args, root.as_ref(), config_sources, use_colors)
        }
        TaskCommands::List(list_args) => {
            execute_list(list_args, root.as_ref(), config_sources, use_colors)
        }
        TaskCommands::Ready(ready_args) => {
            execute_ready(ready_args, root.as_ref(), config_sources, use_colors)
        }
        TaskCommands::Start(start_args) => {
            execute_start(start_args, root.as_ref(), config_sources, use_colors)
        }
        TaskCommands::Close(close_args) => {
            execute_close(close_args, root.as_ref(), config_sources, use_colors)
        }
        TaskCommands::Fail(fail_args) => {
            execute_fail(fail_args, root.as_ref(), config_sources, use_colors)
        }
        TaskCommands::Reopen(reopen_args) => {
            execute_reopen(reopen_args, root.as_ref(), config_sources, use_colors)
        }
        TaskCommands::Show(show_args) => {
            execute_show(show_args, root.as_ref(), config_sources, use_colors)
        }
    }
}

/// Creates a task source from config using the TaskSourceRegistry.
fn create_source(
    config_sources: &[crate::ConfigSource],
    root: Option<&PathBuf>,
) -> anyhow::Result<Box<dyn ralph_core::TaskSource>> {
    let config = crate::load_config_with_overrides(config_sources)?;
    let workspace_root = if let Some(r) = root {
        resolve_workspace_root(Some(r))
    } else {
        PathBuf::from(&config.core.workspace_root)
    };
    let registry = ralph_core::TaskSourceRegistry::new();
    registry
        .create(&config.tasks.source, &workspace_root)
        .map_err(|e| anyhow::anyhow!("Failed to create task source: {e}"))
}

fn execute_add(
    args: AddArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let mut source = create_source(config_sources, root)?;

    let mut task = add_common_task_fields(
        Task::new(args.title, args.priority),
        root,
        args.description,
        args.blocked_by,
    );

    let meta = parse_meta(&args.meta);
    if !meta.is_empty() {
        task.metadata = meta;
    }

    let task = source
        .add(task)
        .map_err(|e| anyhow::anyhow!("Failed to add task: {e}"))?;

    match args.format {
        OutputFormat::Table => {
            if use_colors {
                println!("{}Created task {}{}", colors::GREEN, task.id, colors::RESET);
            } else {
                println!("Created task {}", task.id);
            }
            println!("  Title: {}", task.title);
            println!("  Priority: {}", task.priority);
            if let Some(key) = &task.key {
                println!("  Key: {}", key);
            }
            if !task.blocked_by.is_empty() {
                println!("  Blocked by: {}", task.blocked_by.join(", "));
            }
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string(&task)?);
        }
        OutputFormat::Quiet => {
            println!("{}", task.id);
        }
    }

    Ok(())
}

fn execute_ensure(
    args: EnsureArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let mut source = create_source(config_sources, root)?;

    let mut task = add_common_task_fields(
        Task::new(args.title, args.priority).with_key(Some(args.key.clone())),
        root,
        args.description,
        args.blocked_by,
    );

    let meta = parse_meta(&args.meta);
    if !meta.is_empty() {
        task.metadata = meta;
    }

    let key = task.key.clone().expect("ensure key should be set");

    let existed = source
        .all()
        .map(|tasks| tasks.iter().any(|t| t.key.as_deref() == Some(&key)))
        .unwrap_or(false);

    let ensured = source
        .ensure(task)
        .map_err(|e| anyhow::anyhow!("Failed to ensure task: {e}"))?;

    match args.format {
        OutputFormat::Table => {
            let verb = if existed { "Reused" } else { "Ensured" };
            if use_colors {
                println!(
                    "{}{} task {}{}",
                    colors::GREEN,
                    verb,
                    ensured.id,
                    colors::RESET
                );
            } else {
                println!("{} task {}", verb, ensured.id);
            }
            println!("  Title: {}", ensured.title);
            println!("  Key: {}", key);
            println!("  Priority: {}", ensured.priority);
            if !ensured.blocked_by.is_empty() {
                println!("  Blocked by: {}", ensured.blocked_by.join(", "));
            }
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string(&ensured)?);
        }
        OutputFormat::Quiet => {
            println!("{}", ensured.id);
        }
    }

    Ok(())
}

fn execute_list(
    args: ListArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let source = create_source(config_sources, root)?;
    let all_tasks = source
        .all()
        .map_err(|e| anyhow::anyhow!("Failed to list tasks: {e}"))?;

    let tasks = filter_tasks_for_list(all_tasks, &args);

    match args.format {
        OutputFormat::Table => {
            if tasks.is_empty() {
                println!("No tasks found");
            } else {
                if use_colors {
                    println!(
                        "{}{:<20} {:<15} {:<8} {:<60} {:<24}{}",
                        colors::DIM,
                        "ID",
                        "Status",
                        "Priority",
                        "Title",
                        "Key",
                        colors::RESET
                    );
                    println!("{}{}{}", colors::DIM, "-".repeat(131), colors::RESET);
                } else {
                    println!(
                        "{:<20} {:<15} {:<8} {:<60} {:<24}",
                        "ID", "Status", "Priority", "Title", "Key"
                    );
                    println!("{}", "-".repeat(131));
                }

                for task in &tasks {
                    let (status_str, status_color) = match task.status {
                        TaskStatus::Open => ("open", colors::GREEN),
                        TaskStatus::InProgress => ("in_progress", colors::BLUE),
                        TaskStatus::Closed => ("closed", colors::DIM),
                        TaskStatus::Failed => ("failed", colors::RED),
                    };

                    let priority_color = match task.priority {
                        1 => colors::RED,
                        2 => colors::YELLOW,
                        _ => colors::RESET,
                    };

                    let title_truncated = if task.title.len() > 60 {
                        crate::display::truncate(&task.title, 60)
                    } else {
                        task.title.clone()
                    };

                    if use_colors {
                        println!(
                            "{}{:<20}{} {}{:<15}{} {}{:<8}{} {:<60} {:<24}",
                            colors::DIM,
                            task.id,
                            colors::RESET,
                            status_color,
                            status_str,
                            colors::RESET,
                            priority_color,
                            task.priority,
                            colors::RESET,
                            title_truncated,
                            task.key.as_deref().unwrap_or("-")
                        );
                    } else {
                        println!(
                            "{:<20} {:<15} {:<8} {:<60} {:<24}",
                            task.id,
                            status_str,
                            task.priority,
                            title_truncated,
                            task.key.as_deref().unwrap_or("-")
                        );
                    }
                }
            }
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&tasks)?);
        }
        OutputFormat::Quiet => {
            for task in &tasks {
                println!("{}", task.id);
            }
        }
    }

    Ok(())
}

fn execute_ready(
    args: ReadyArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let mut source = create_source(config_sources, root)?;

    if !args.all
        && let Some(loop_id) = read_current_loop_id(root)
    {
        source.set_loop_filter(Some(&loop_id));
    }

    let ready = source
        .ready()
        .map_err(|e| anyhow::anyhow!("Failed to get ready tasks: {e}"))?;

    match args.format {
        OutputFormat::Table => {
            if ready.is_empty() {
                println!("No ready tasks");
            } else {
                if use_colors {
                    println!(
                        "{}{:<20} {:<8} {:<60} {:<24}{}",
                        colors::DIM,
                        "ID",
                        "Priority",
                        "Title",
                        "Key",
                        colors::RESET
                    );
                    println!("{}{}{}", colors::DIM, "-".repeat(115), colors::RESET);
                } else {
                    println!(
                        "{:<20} {:<8} {:<60} {:<24}",
                        "ID", "Priority", "Title", "Key"
                    );
                    println!("{}", "-".repeat(115));
                }

                for task in &ready {
                    let title_truncated = if task.title.len() > 60 {
                        crate::display::truncate(&task.title, 60)
                    } else {
                        task.title.clone()
                    };

                    let priority_color = match task.priority {
                        1 => colors::RED,
                        2 => colors::YELLOW,
                        _ => colors::RESET,
                    };

                    if use_colors {
                        println!(
                            "{}{:<20}{} {}{:<8}{} {:<60} {:<24}",
                            colors::DIM,
                            task.id,
                            colors::RESET,
                            priority_color,
                            task.priority,
                            colors::RESET,
                            title_truncated,
                            task.key.as_deref().unwrap_or("-")
                        );
                    } else {
                        println!(
                            "{:<20} {:<8} {:<60} {:<24}",
                            task.id,
                            task.priority,
                            title_truncated,
                            task.key.as_deref().unwrap_or("-")
                        );
                    }
                }
            }
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&ready)?);
        }
        OutputFormat::Quiet => {
            for task in &ready {
                println!("{}", task.id);
            }
        }
    }

    Ok(())
}

fn execute_start(
    args: StartArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let mut source = create_source(config_sources, root)?;

    let task_id = args.id;
    let started = source
        .start(&task_id)
        .map_err(|e| anyhow::anyhow!("Failed to start task: {e}"))?
        .context(format!("Task {} not found", task_id))?;

    if use_colors {
        println!(
            "{}Started task: {} - {}{}",
            colors::BLUE,
            task_id,
            started.title,
            colors::RESET
        );
    } else {
        println!("Started task: {} - {}", task_id, started.title);
    }

    Ok(())
}

fn execute_close(
    args: CloseArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let mut source = create_source(config_sources, root)?;

    let task_id = args.id;
    let closed = source
        .close(&task_id)
        .map_err(|e| anyhow::anyhow!("Failed to close task: {e}"))?
        .context(format!("Task {} not found", task_id))?;

    if use_colors {
        println!(
            "{}Closed task: {} - {}{}",
            colors::GREEN,
            task_id,
            closed.title,
            colors::RESET
        );
    } else {
        println!("Closed task: {} - {}", task_id, closed.title);
    }

    Ok(())
}

fn execute_fail(
    args: FailArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let mut source = create_source(config_sources, root)?;

    let task_id = args.id;
    let failed = source
        .fail(&task_id)
        .map_err(|e| anyhow::anyhow!("Failed to fail task: {e}"))?
        .context(format!("Task {} not found", task_id))?;

    if use_colors {
        println!(
            "{}Failed task: {} - {}{}",
            colors::RED,
            task_id,
            failed.title,
            colors::RESET
        );
    } else {
        println!("Failed task: {} - {}", task_id, failed.title);
    }

    Ok(())
}

fn execute_show(
    args: ShowArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let source = create_source(config_sources, root)?;

    let task = source
        .get(&args.id)
        .map_err(|e| anyhow::anyhow!("Failed to get task: {e}"))?
        .context(format!("Task {} not found", args.id))?;

    match args.format {
        OutputFormat::Table => {
            let status_str = match task.status {
                TaskStatus::Open => "open",
                TaskStatus::InProgress => "in_progress",
                TaskStatus::Closed => "closed",
                TaskStatus::Failed => "failed",
            };

            if use_colors {
                let status_color = match task.status {
                    TaskStatus::Open => colors::GREEN,
                    TaskStatus::InProgress => colors::BLUE,
                    TaskStatus::Closed => colors::DIM,
                    TaskStatus::Failed => colors::RED,
                };
                let priority_color = match task.priority {
                    1 => colors::RED,
                    2 => colors::YELLOW,
                    _ => colors::RESET,
                };

                println!("{}ID:          {}{}", colors::DIM, task.id, colors::RESET);
                println!("Title:       {}", task.title);
                if let Some(desc) = &task.description {
                    println!("Description: {}", desc);
                }
                println!(
                    "Status:      {}{}{}",
                    status_color,
                    status_str,
                    colors::RESET
                );
                println!(
                    "Priority:    {}{}{}",
                    priority_color,
                    task.priority,
                    colors::RESET
                );
                if let Some(key) = &task.key {
                    println!("Key:         {}", key);
                }
                if !task.blocked_by.is_empty() {
                    println!("Blocked by:  {}", task.blocked_by.join(", "));
                }
                println!("Created:     {}", task.created);
                if let Some(started) = &task.started {
                    println!("Started:     {}", started);
                }
                if let Some(closed) = &task.closed {
                    println!("Closed:      {}", closed);
                }
            } else {
                println!("ID:          {}", task.id);
                println!("Title:       {}", task.title);
                if let Some(desc) = &task.description {
                    println!("Description: {}", desc);
                }
                println!("Status:      {}", status_str);
                println!("Priority:    {}", task.priority);
                if let Some(key) = &task.key {
                    println!("Key:         {}", key);
                }
                if !task.blocked_by.is_empty() {
                    println!("Blocked by:  {}", task.blocked_by.join(", "));
                }
                println!("Created:     {}", task.created);
                if let Some(started) = &task.started {
                    println!("Started:     {}", started);
                }
                if let Some(closed) = &task.closed {
                    println!("Closed:      {}", closed);
                }
            }
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        OutputFormat::Quiet => {
            println!("{}", task.id);
        }
    }

    Ok(())
}

fn execute_reopen(
    args: ReopenArgs,
    root: Option<&PathBuf>,
    config_sources: &[crate::ConfigSource],
    use_colors: bool,
) -> Result<()> {
    let mut source = create_source(config_sources, root)?;

    let task_id = args.id;
    let reopened = source
        .reopen(&task_id)
        .map_err(|e| anyhow::anyhow!("Failed to reopen task: {e}"))?
        .context(format!("Task {} not found", task_id))?;

    if use_colors {
        println!(
            "{}Reopened task: {} - {}{}",
            colors::YELLOW,
            task_id,
            reopened.title,
            colors::RESET
        );
    } else {
        println!("Reopened task: {} - {}", task_id, reopened.title);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn get_tasks_path(root: Option<&PathBuf>) -> PathBuf {
        crate::resolve_workspace_root(root).join(".ralph/agent/tasks.jsonl")
    }

    #[test]
    fn test_list_status_filter_accepts_in_progress() {
        let mut open_task = Task::new("Open".to_string(), 2);
        open_task.status = TaskStatus::Open;
        let mut in_progress = Task::new("In progress".to_string(), 2);
        in_progress.status = TaskStatus::InProgress;

        let tasks = vec![open_task, in_progress];

        let args = ListArgs {
            status: Some("in_progress".to_string()),
            days: None,
            limit: None,
            all: true,
            format: OutputFormat::Quiet,
        };

        let filtered = filter_tasks_for_list(tasks, &args);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].status, TaskStatus::InProgress);
    }

    #[test]
    fn test_read_current_loop_id_ignores_empty_marker() {
        let temp_dir = TempDir::new().expect("temp dir");
        let root = temp_dir.path().to_path_buf();
        let marker_dir = root.join(".ralph");
        std::fs::create_dir_all(&marker_dir).expect("marker dir");
        std::fs::write(marker_dir.join("current-loop-id"), "  ").expect("write marker");

        assert_eq!(read_current_loop_id(Some(&root)), None);
    }

    #[test]
    fn test_get_tasks_path_discovers_workspace_root_from_nested_dir() {
        let temp_dir = TempDir::new().expect("temp dir");
        let root = temp_dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".ralph/agent")).expect("agent dir");

        assert_eq!(
            get_tasks_path(Some(&root)),
            root.join(".ralph/agent/tasks.jsonl")
        );
    }
}
