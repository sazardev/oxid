//! Oxid command-line interface.
//!
//! Thin client over the daemon's HTTP API (SPEC.md §5.1). Point it at a running
//! daemon with `OXID_API` (default `http://127.0.0.1:8080`), or override
//! per-invocation with `--api`.

use std::io::Write as _;

use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};

/// Ephemeral environments that breathe. Ferrous performance, invisible footprint.
#[derive(Debug, Parser)]
#[command(name = "oxid", version, about, long_about = None)]
struct Cli {
    /// Daemon base URL. Overrides `OXID_API` when set.
    #[arg(long, global = true)]
    api: Option<String>,
    /// Bearer token for daemons configured with `OXID_API_TOKEN`. Overrides
    /// `OXID_TOKEN` when set.
    #[arg(long, global = true)]
    token: Option<String>,
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
    /// List environments for the project in the current directory.
    Status,
    /// Destroy a branch's environment permanently.
    Down {
        /// Branch whose environment to destroy.
        branch: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
        /// Also delete this branch's secrets (kept by default, so a
        /// recurring feature branch's config survives redeploy).
        #[arg(long)]
        purge_secrets: bool,
    },
    /// Permanently delete the project registered for the current directory,
    /// destroying every environment and removing its git cache.
    RmProject {
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Suspend a branch's environment (scale-to-zero).
    Pause {
        /// Branch to pause.
        branch: String,
    },
    /// Wake a suspended branch environment.
    Wake {
        /// Branch to wake.
        branch: String,
    },
    /// List registered projects.
    Ps,
    /// Manage environment variables / secrets.
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Show logs of a branch environment.
    Logs {
        /// Branch whose logs to show.
        branch: String,
        /// Stream new output live (SSE) as it's written, instead of a
        /// one-shot snapshot.
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
const DIM_ITALIC: &str = "\x1b[3;90m";
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

/// Colors a lifecycle state per DESIGN.md §3.1 (Running green, Building
/// orange, Paused/Hibernating dim-italic "asleep" look).
fn colored_state(state: &str) -> String {
    match state {
        "running" => format!("{GREEN}{state}{RESET}"),
        "building" => format!("{ORANGE}{state}{RESET}"),
        "paused" | "hibernating" => format!("{DIM_ITALIC}{state}{RESET}"),
        "destroyed" => format!("{RED}{state}{RESET}"),
        other => other.to_owned(),
    }
}

fn api_base(cli_flag: Option<&str>) -> String {
    cli_flag
        .map(str::to_owned)
        .or_else(|| std::env::var("OXID_API").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_owned())
}

fn api_token(cli_flag: Option<&str>) -> Option<String> {
    cli_flag
        .map(str::to_owned)
        .or_else(|| std::env::var("OXID_TOKEN").ok())
}

/// Builds the shared HTTP client, attaching `Authorization: Bearer <token>`
/// to every outgoing request when one is configured — every `cmd_*`
/// function reuses this one client, so this is the single place that needs
/// to know about auth at all.
fn build_client(token: Option<&str>) -> Result<Client, String> {
    let mut builder = Client::builder();
    if let Some(token) = token {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| format!("invalid --token value: {e}"))?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
        builder = builder.default_headers(headers);
    }
    builder
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))
}

fn report_error(body: &str, status: reqwest::StatusCode) {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.to_owned());
    error(format!("{status}: {message}"));
}

// The CLI is a short-lived process making a handful of sequential HTTP
// calls — it never benefits from a multi-threaded work-stealing runtime,
// which would otherwise spin up one OS thread per CPU core just to run
// them one at a time. `current_thread` avoids that startup cost entirely.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let base = api_base(cli.api.as_deref());
    let token = api_token(cli.token.as_deref());
    let client = match build_client(token.as_deref()) {
        Ok(client) => client,
        Err(message) => {
            error(message);
            std::process::exit(1);
        }
    };

    let result = match cli.command {
        Command::Up { branch } => cmd_up(&client, &base, &branch).await,
        Command::Status => cmd_status(&client, &base).await,
        Command::Down {
            branch,
            force,
            purge_secrets,
        } => cmd_down(&client, &base, &branch, force, purge_secrets).await,
        Command::RmProject { force } => cmd_rm_project(&client, &base, force).await,
        Command::Pause { branch } => cmd_pause(&client, &base, &branch).await,
        Command::Wake { branch } => cmd_wake(&client, &base, &branch).await,
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
        Command::Logs { branch, follow } => cmd_logs(&client, &base, &branch, follow).await,
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

async fn get_json(client: &Client, url: String) -> Result<Value, String> {
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("cannot reach daemon at {url}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        report_error(&body, status);
        return Err("request failed".to_owned());
    }
    serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))
}

/// Registers the project for `cwd`, then resolves `branch` to its single
/// running environment. Fails with a hint to `oxid status` when the branch
/// has no environment.
async fn resolve_environment(
    client: &Client,
    base: &str,
    branch: &str,
) -> Result<(u64, Value), String> {
    let project = register_project(client, base).await?;
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;
    let envs = get_json(
        client,
        format!("{base}/api/v1/projects/{project_id}/environments?branch={branch}"),
    )
    .await?;
    let envs = envs
        .as_array()
        .ok_or_else(|| "invalid daemon response: expected an array".to_owned())?;
    let env = envs.first().ok_or_else(|| {
        format!("no environment found for branch `{branch}`; run `oxid status` to see what's live")
    })?;
    let env_id = env["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing environment id".to_owned())?;
    Ok((env_id, env.clone()))
}

async fn cmd_up(client: &Client, base: &str, branch: &str) -> Result<(), String> {
    action(format!("oxid up {branch}"));
    let project = register_project(client, base).await?;
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;
    ok("Parsed oxid.toml successfully");
    ok(format!(
        "Project `{}` registered (id {project_id})",
        project["name"].as_str().unwrap_or("?")
    ));

    action(format!("Building image for {branch}..."));
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

async fn cmd_status(client: &Client, base: &str) -> Result<(), String> {
    let project = register_project(client, base).await?;
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;
    let envs = get_json(
        client,
        format!("{base}/api/v1/projects/{project_id}/environments"),
    )
    .await?;
    let envs = envs
        .as_array()
        .ok_or_else(|| "invalid daemon response: expected an array".to_owned())?;
    if envs.is_empty() {
        bg(format!(
            "No environments for `{}` yet. Deploy one with `oxid up <branch>`.",
            project["name"].as_str().unwrap_or("?")
        ));
        return Ok(());
    }
    // The daemon keeps one row per historical deploy (audit trail); only the
    // most recent row per branch reflects what's actually live. `envs` is
    // returned in ascending id order, so a later entry always overwrites an
    // earlier one for the same branch here.
    let mut latest: Vec<(&str, &str, &str)> = Vec::new();
    for env in envs {
        let branch = env["branch"]["name"].as_str().unwrap_or("?");
        let state = env["state"].as_str().unwrap_or("?");
        let url = env["url"].as_str().unwrap_or("?");
        match latest.iter_mut().find(|(b, ..)| *b == branch) {
            Some(entry) => *entry = (branch, state, url),
            None => latest.push((branch, state, url)),
        }
    }
    println!("{:<24} {:<24} URL", "BRANCH", "STATE");
    for (branch, state, url) in latest {
        println!("{:<24} {:<33} {}", branch, colored_state(state), url);
    }
    Ok(())
}

async fn cmd_down(
    client: &Client,
    base: &str,
    branch: &str,
    force: bool,
    purge_secrets: bool,
) -> Result<(), String> {
    let (env_id, _) = resolve_environment(client, base, branch).await?;
    if !force
        && !confirm(&format!(
            "This will permanently destroy `{branch}` and its container. Continue? [y/N] "
        ))
    {
        bg("Aborted (re-run with --force to skip this prompt).");
        return Ok(());
    }
    let url = if purge_secrets {
        format!("{base}/api/v1/environments/{env_id}?purge_secrets=true")
    } else {
        format!("{base}/api/v1/environments/{env_id}")
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
        return Err("destroying environment failed".to_owned());
    }
    ok(format!("Environment `{branch}` destroyed"));
    if purge_secrets {
        bg(format!("Branch `{branch}` secrets purged"));
    }
    Ok(())
}

/// Prompts for a `y`/`N` confirmation on stdin.
fn confirm(prompt: &str) -> bool {
    print!("{ORANGE}[>]{RESET} {prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

async fn cmd_rm_project(client: &Client, base: &str, force: bool) -> Result<(), String> {
    let project = register_project(client, base).await?;
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;
    let name = project["name"].as_str().unwrap_or("?");
    if !force
        && !confirm(&format!(
            "This will permanently delete project `{name}` — every environment, secret and its git cache. Continue? [y/N] "
        ))
    {
        bg("Aborted (re-run with --force to skip this prompt).");
        return Ok(());
    }
    let response = client
        .delete(format!("{base}/api/v1/projects/{project_id}"))
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
        return Err("deleting project failed".to_owned());
    }
    ok(format!("Project `{name}` deleted"));
    Ok(())
}

async fn cmd_pause(client: &Client, base: &str, branch: &str) -> Result<(), String> {
    let (env_id, _) = resolve_environment(client, base, branch).await?;
    post_empty(client, format!("{base}/api/v1/environments/{env_id}/pause")).await?;
    bg(format!("Environment `{branch}` paused"));
    Ok(())
}

async fn cmd_wake(client: &Client, base: &str, branch: &str) -> Result<(), String> {
    let (env_id, _) = resolve_environment(client, base, branch).await?;
    post_empty(client, format!("{base}/api/v1/environments/{env_id}/wake")).await?;
    ok(format!("Environment `{branch}` woken"));
    Ok(())
}

async fn post_empty(client: &Client, url: String) -> Result<(), String> {
    let response = client
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("cannot reach daemon at {url}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        report_error(&body, status);
        return Err("request failed".to_owned());
    }
    Ok(())
}

/// Fetches an environment's logs. Without `follow`, a one-shot snapshot;
/// with `follow`, consumes the daemon's SSE `/logs/stream` endpoint and
/// prints each `data:` line as it arrives — a real live stream, not polling.
async fn cmd_logs(client: &Client, base: &str, branch: &str, follow: bool) -> Result<(), String> {
    let (env_id, _) = resolve_environment(client, base, branch).await?;

    if !follow {
        let url = format!("{base}/api/v1/environments/{env_id}/logs");
        let value = get_json(client, url).await?;
        let logs = value["logs"].as_str().unwrap_or("");
        for line in logs.lines() {
            println!("{line}");
        }
        return Ok(());
    }

    let url = format!("{base}/api/v1/environments/{env_id}/logs/stream");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("request failed with status {}", response.status()));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream error: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buf.find("\n\n") {
            let frame: String = buf.drain(..pos + 2).collect();
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    println!("{}", data.trim_start());
                }
            }
        }
    }
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

    let (url, payload) = if scope == "global" {
        (
            format!("{base}/api/v1/secrets"),
            json!({ "name": name, "scope": scope, "value": value }),
        )
    } else {
        let project_id = ensure_project_id(client, base, project_id).await?;
        (
            format!("{base}/api/v1/projects/{project_id}/secrets"),
            json!({
                "name": name,
                "scope": scope,
                "value": value,
                "branch": branch,
            }),
        )
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

    let url = if scope == "global" {
        format!("{base}/api/v1/secrets/{name}")
    } else {
        let project_id = ensure_project_id(client, base, project_id).await?;
        let branch_qs = branch
            .as_ref()
            .map(|b| format!("?branch={b}"))
            .unwrap_or_default();
        format!("{base}/api/v1/projects/{project_id}/secrets/{name}{branch_qs}")
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
    fn parses_status_command() {
        let cli = Cli::try_parse_from(["oxid", "status"]).unwrap();
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn parses_down_with_force() {
        let cli = Cli::try_parse_from(["oxid", "down", "feature-a", "--force"]).unwrap();
        match cli.command {
            Command::Down {
                branch,
                force,
                purge_secrets,
            } => {
                assert_eq!(branch, "feature-a");
                assert!(force);
                assert!(!purge_secrets);
            }
            other => panic!("expected Down, got {other:?}"),
        }
    }

    #[test]
    fn parses_down_without_force() {
        let cli = Cli::try_parse_from(["oxid", "down", "feature-a"]).unwrap();
        match cli.command {
            Command::Down { force, .. } => assert!(!force),
            other => panic!("expected Down, got {other:?}"),
        }
    }

    #[test]
    fn parses_down_with_purge_secrets() {
        let cli = Cli::try_parse_from(["oxid", "down", "feature-a", "--force", "--purge-secrets"])
            .unwrap();
        match cli.command {
            Command::Down { purge_secrets, .. } => assert!(purge_secrets),
            other => panic!("expected Down, got {other:?}"),
        }
    }

    #[test]
    fn parses_rm_project() {
        let cli = Cli::try_parse_from(["oxid", "rm-project", "--force"]).unwrap();
        match cli.command {
            Command::RmProject { force } => assert!(force),
            other => panic!("expected RmProject, got {other:?}"),
        }
    }

    #[test]
    fn parses_pause_and_wake() {
        let cli = Cli::try_parse_from(["oxid", "pause", "feature-a"]).unwrap();
        assert!(matches!(cli.command, Command::Pause { branch } if branch == "feature-a"));
        let cli = Cli::try_parse_from(["oxid", "wake", "feature-a"]).unwrap();
        assert!(matches!(cli.command, Command::Wake { branch } if branch == "feature-a"));
    }

    #[test]
    fn parses_global_api_flag() {
        let cli = Cli::try_parse_from(["oxid", "--api", "http://example.com", "ps"]).unwrap();
        assert_eq!(cli.api.as_deref(), Some("http://example.com"));

        let cli = Cli::try_parse_from(["oxid", "ps", "--api", "http://example.com"]).unwrap();
        assert_eq!(cli.api.as_deref(), Some("http://example.com"));
    }

    #[test]
    fn api_base_prefers_flag_over_env() {
        assert_eq!(api_base(Some("http://flag:1")), "http://flag:1".to_owned());
    }

    #[test]
    fn api_token_prefers_flag_over_env() {
        assert_eq!(api_token(Some("flag-token")), Some("flag-token".to_owned()));
    }

    #[test]
    fn parses_global_token_flag() {
        let cli = Cli::try_parse_from(["oxid", "--token", "s3cr3t", "ps"]).unwrap();
        assert_eq!(cli.token.as_deref(), Some("s3cr3t"));
    }

    #[test]
    fn build_client_accepts_no_token() {
        assert!(build_client(None).is_ok());
    }

    #[test]
    fn build_client_accepts_a_token() {
        assert!(build_client(Some("s3cr3t")).is_ok());
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
