//! Oxid command-line interface.
//!
//! Thin client over the daemon's HTTP API (SPEC.md §5.1). Point it at a running
//! daemon with `OXID_API` (default `http://127.0.0.1:8080`).

use clap::{Parser, Subcommand};
use reqwest::Client;
use serde_json::{Value, json};

/// Ephemeral environments that breathe. Ferrous performance, invisible footprint.
#[derive(Debug, Parser)]
#[command(name = "oxid", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Deploy a branch of the repository in the current directory.
    Up {
        /// Branch to deploy.
        branch: String,
    },
    /// List registered projects.
    Ps,
    /// Manage environment variables / secrets.
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Stream logs of a branch environment.
    Logs {
        /// Branch whose logs to follow.
        branch: String,
        /// Follow output as it is written.
        #[arg(long, short)]
        follow: bool,
    },
}

#[derive(Debug, Subcommand)]
enum EnvAction {
    /// Set a secret, e.g. `oxid env set DB_PASSWORD=secret --scope project`.
    Set {
        /// Variable to set, e.g. `DB_PASSWORD=secret`.
        assignment: String,
        /// Scope: `global`, `project` or `branch`.
        #[arg(long, default_value = "global")]
        scope: String,
        /// Project id (auto-registers from the current directory if omitted).
        #[arg(long)]
        project: Option<u64>,
        /// Branch (required for `--scope branch`).
        #[arg(long)]
        branch: Option<String>,
    },
    /// List secrets in a scope (values are never shown).
    List {
        /// Scope: `global`, `project` or `branch`.
        #[arg(long, default_value = "global")]
        scope: String,
        /// Project id (auto-registers from the current directory if omitted).
        #[arg(long)]
        project: Option<u64>,
        /// Branch (required for `--scope branch`).
        #[arg(long)]
        branch: Option<String>,
    },
    /// Delete a secret.
    Delete {
        /// Name of the secret to delete.
        name: String,
        /// Scope: `global`, `project` or `branch`.
        #[arg(long, default_value = "global")]
        scope: String,
        /// Project id (auto-registers from the current directory if omitted).
        #[arg(long)]
        project: Option<u64>,
        /// Branch (required for `--scope branch`).
        #[arg(long)]
        branch: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// ANSI output (DESIGN.md §3.3: `[+]` green, `[~]` gray, `[>]` orange, `[!]` red)
// ---------------------------------------------------------------------------

const GREEN: &str = "\x1b[32m";
const GRAY: &str = "\x1b[90m";
const ORANGE: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

fn ok(msg: impl std::fmt::Display) {
    println!("{GREEN}[+]{RESET} {msg}");
}

fn bg(msg: impl std::fmt::Display) {
    println!("{GRAY}[~]{RESET} {msg}");
}

fn action(msg: impl std::fmt::Display) {
    println!("{ORANGE}[>]{RESET} {msg}");
}

fn error(msg: impl std::fmt::Display) {
    eprintln!("{RED}[!]{RESET} {msg}");
}

fn api_base() -> String {
    std::env::var("OXID_API").unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned())
}

fn report_error(body: &str, status: reqwest::StatusCode) {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.to_owned());
    error(format!("{status}: {message}"));
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = Client::new();
    let base = api_base();

    let result = match cli.command {
        Command::Up { branch } => cmd_up(&client, &base, &branch).await,
        Command::Ps => cmd_ps(&client, &base).await,
        Command::Env { action } => match action {
            EnvAction::Set {
                assignment,
                scope,
                project,
                branch,
            } => {
                cmd_env_set(
                    &client,
                    &base,
                    &assignment,
                    &scope,
                    project,
                    branch.as_deref(),
                )
                .await
            }
            EnvAction::List {
                scope,
                project,
                branch,
            } => cmd_env_list(&client, &base, &scope, project, branch.as_deref()).await,
            EnvAction::Delete {
                name,
                scope,
                project,
                branch,
            } => cmd_env_delete(&client, &base, &name, &scope, project, branch.as_deref()).await,
        },
        Command::Logs { branch, follow } => {
            let follow = if follow { " -f" } else { "" };
            println!("[>] oxid logs {branch}{follow} (not implemented yet)");
            Ok(())
        }
    };

    if let Err(message) = result {
        error(message);
        std::process::exit(1);
    }
}

/// Registers the repository in the current directory and returns its project.
///
/// Idempotent on the daemon side: repeated registrations return the existing
/// project.
async fn register_project(client: &Client, base: &str) -> Result<Value, String> {
    let repo_dir = std::env::current_dir().map_err(|e| format!("cannot resolve cwd: {e}"))?;
    let response = client
        .post(format!("{base}/api/v1/projects"))
        .json(&json!({ "repo_dir": repo_dir.display().to_string() }))
        .send()
        .await
        .map_err(|e| format!("cannot reach daemon at {base}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        report_error(&body, status);
        return Err("project registration failed".to_owned());
    }
    let project: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    Ok(project)
}

async fn cmd_up(client: &Client, base: &str, branch: &str) -> Result<(), String> {
    action(format!("oxid up {branch}"));
    let project = register_project(client, base).await?;
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;
    ok(format!(
        "Project `{}` registered (id {project_id})",
        project["name"]
    ));

    let response = client
        .post(format!("{base}/api/v1/projects/{project_id}/deploy"))
        .json(&json!({ "branch": branch }))
        .send()
        .await
        .map_err(|e| format!("deploy request failed: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        report_error(&body, status);
        return Err("deployment failed".to_owned());
    }
    let env: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    let url = env["url"].as_str().unwrap_or("?");
    ok(format!("Environment live at: {url}"));
    Ok(())
}

async fn cmd_ps(client: &Client, base: &str) -> Result<(), String> {
    let response = client
        .get(format!("{base}/api/v1/projects"))
        .send()
        .await
        .map_err(|e| format!("cannot reach daemon at {base}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        report_error(&body, status);
        return Err("listing projects failed".to_owned());
    }
    let projects: Vec<Value> =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if projects.is_empty() {
        bg("No projects registered yet.");
        return Ok(());
    }
    println!("{:<5} {:<24} BASE DOMAIN", "ID", "NAME");
    for project in &projects {
        println!(
            "{:<5} {:<24} {}",
            project["id"].as_u64().unwrap_or_default(),
            project["name"].as_str().unwrap_or("?"),
            project["config"]["base_domain"].as_str().unwrap_or("?"),
        );
    }
    Ok(())
}

/// Resolves the endpoint context for a scope: global vs project/branch.
///
/// Returns `(project_id, branch)` used to select the API route and payload.
fn scope_context(
    scope: &str,
    project: Option<u64>,
    branch: Option<&str>,
) -> Result<(Option<u64>, Option<String>), String> {
    match scope {
        "global" => Ok((None, None)),
        "project" => {
            if branch.is_some() {
                return Err("`--branch` is only allowed with `--scope branch`".to_owned());
            }
            Ok((project, None))
        }
        "branch" => {
            let branch = branch
                .map(str::to_owned)
                .ok_or_else(|| "`--branch` is required for `--scope branch`".to_owned())?;
            Ok((project, Some(branch)))
        }
        other => Err(format!(
            "invalid scope `{other}`; expected `global`, `project` or `branch`"
        )),
    }
}

async fn ensure_project_id(
    client: &Client,
    base: &str,
    project: Option<u64>,
) -> Result<u64, String> {
    if let Some(id) = project {
        Ok(id)
    } else {
        let project = register_project(client, base).await?;
        project["id"]
            .as_u64()
            .ok_or_else(|| "daemon response missing project id".to_owned())
    }
}

fn parse_assignment(assignment: &str) -> Result<(&str, &str), String> {
    let (name, value) = assignment
        .split_once('=')
        .ok_or_else(|| format!("expected `KEY=VALUE`, got `{assignment}`"))?;
    if name.trim().is_empty() {
        return Err("secret name cannot be empty".to_owned());
    }
    Ok((name, value))
}

async fn cmd_env_set(
    client: &Client,
    base: &str,
    assignment: &str,
    scope: &str,
    project: Option<u64>,
    branch: Option<&str>,
) -> Result<(), String> {
    let (name, value) = parse_assignment(assignment)?;
    let (project_id, branch) = scope_context(scope, project, branch)?;

    let (url, payload) = match (project_id, scope) {
        (None, "global") => (
            format!("{base}/api/v1/secrets"),
            json!({ "name": name, "scope": scope, "value": value }),
        ),
        (Some(id), _) => (
            format!("{base}/api/v1/projects/{id}/secrets"),
            json!({
                "name": name,
                "scope": scope,
                "value": value,
                "branch": branch,
            }),
        ),
        (None, _) => {
            return Err("`--project` is required for a non-global scope".to_owned());
        }
    };

    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("cannot reach daemon at {base}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        report_error(&body, status);
        return Err("setting secret failed".to_owned());
    }
    match branch {
        Some(b) => ok(format!("Secret `{name}` set for branch `{b}`")),
        None => ok(format!("Secret `{name}` set ({scope})")),
    }
    Ok(())
}

async fn cmd_env_list(
    client: &Client,
    base: &str,
    scope: &str,
    project: Option<u64>,
    branch: Option<&str>,
) -> Result<(), String> {
    let (project_id, branch) = scope_context(scope, project, branch)?;
    let project_id = ensure_project_id(client, base, project_id).await?;

    let url = if scope == "global" {
        format!("{base}/api/v1/secrets")
    } else {
        let branch_qs = branch
            .as_ref()
            .map(|b| format!("?branch={b}"))
            .unwrap_or_default();
        format!("{base}/api/v1/projects/{project_id}/secrets{branch_qs}")
    };
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("cannot reach daemon at {base}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        report_error(&body, status);
        return Err("listing secrets failed".to_owned());
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    let secrets = value["secrets"].as_array().cloned().unwrap_or_default();
    if secrets.is_empty() {
        bg("No secrets in this scope.");
        return Ok(());
    }
    println!("{:<28} SCOPE", "NAME");
    for secret in &secrets {
        println!(
            "{:<28} {}",
            secret["name"].as_str().unwrap_or("?"),
            secret["scope"].as_str().unwrap_or("?"),
        );
    }
    Ok(())
}

async fn cmd_env_delete(
    client: &Client,
    base: &str,
    name: &str,
    scope: &str,
    project: Option<u64>,
    branch: Option<&str>,
) -> Result<(), String> {
    let (project_id, branch) = scope_context(scope, project, branch)?;

    let url = match (project_id, scope) {
        (None, "global") => format!("{base}/api/v1/secrets/{name}"),
        (Some(id), _) => {
            let branch_qs = branch
                .as_ref()
                .map(|b| format!("?branch={b}"))
                .unwrap_or_default();
            format!("{base}/api/v1/projects/{id}/secrets/{name}{branch_qs}")
        }
        (None, _) => {
            return Err("`--project` is required for a non-global scope".to_owned());
        }
    };

    let response = client
        .delete(url)
        .send()
        .await
        .map_err(|e| format!("cannot reach daemon at {base}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        report_error(&body, status);
        return Err("deleting secret failed".to_owned());
    }
    ok(format!("Secret `{name}` deleted"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_up_command() {
        let cli = Cli::try_parse_from(["oxid", "up", "feature-login"]).unwrap();
        match cli.command {
            Command::Up { branch } => assert_eq!(branch, "feature-login"),
            other => panic!("expected Up, got {other:?}"),
        }
    }

    #[test]
    fn parses_ps_command() {
        let cli = Cli::try_parse_from(["oxid", "ps"]).unwrap();
        assert!(matches!(cli.command, Command::Ps));
    }

    #[test]
    fn parses_logs_with_follow() {
        let cli = Cli::try_parse_from(["oxid", "logs", "feature-a", "-f"]).unwrap();
        match cli.command {
            Command::Logs { branch, follow } => {
                assert_eq!(branch, "feature-a");
                assert!(follow);
            }
            other => panic!("expected Logs, got {other:?}"),
        }
    }

    #[test]
    fn parses_env_set() {
        let cli =
            Cli::try_parse_from(["oxid", "env", "set", "DB_PASSWORD=x", "--scope", "project"])
                .unwrap();
        match cli.command {
            Command::Env {
                action:
                    EnvAction::Set {
                        assignment,
                        scope,
                        project,
                        branch,
                    },
            } => {
                assert_eq!(assignment, "DB_PASSWORD=x");
                assert_eq!(scope, "project");
                assert!(project.is_none());
                assert!(branch.is_none());
            }
            other => panic!("expected Env::Set, got {other:?}"),
        }
    }

    #[test]
    fn parses_env_list_with_branch_scope() {
        let cli = Cli::try_parse_from([
            "oxid",
            "env",
            "list",
            "--scope",
            "branch",
            "--branch",
            "feat-a",
            "--project",
            "3",
        ])
        .unwrap();
        match cli.command {
            Command::Env {
                action:
                    EnvAction::List {
                        scope,
                        branch,
                        project,
                        ..
                    },
            } => {
                assert_eq!(scope, "branch");
                assert_eq!(branch.as_deref(), Some("feat-a"));
                assert_eq!(project, Some(3));
            }
            other => panic!("expected Env::List, got {other:?}"),
        }
    }

    #[test]
    fn parses_env_delete() {
        let cli = Cli::try_parse_from(["oxid", "env", "delete", "DB_PASSWORD"]).unwrap();
        match cli.command {
            Command::Env {
                action: EnvAction::Delete { name, .. },
            } => assert_eq!(name, "DB_PASSWORD"),
            other => panic!("expected Env::Delete, got {other:?}"),
        }
    }

    #[test]
    fn scope_context_rejects_branch_without_project_scope() {
        assert!(scope_context("project", None, Some("feat-a")).is_err());
        assert!(scope_context("branch", None, None).is_err());
        assert!(scope_context("runtime", None, None).is_err());
        let (pid, branch) = scope_context("global", None, None).unwrap();
        assert_eq!(pid, None);
        assert_eq!(branch, None);
    }

    #[test]
    fn parses_assignments() {
        assert_eq!(parse_assignment("A=1").unwrap(), ("A", "1"));
        assert!(parse_assignment("missing-equals").is_err());
    }
}
