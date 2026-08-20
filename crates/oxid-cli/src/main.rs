//! Oxid command-line interface.
//!
//! Thin client over the daemon's HTTP API (SPEC.md §5.1). Point it at a running
//! daemon with `OXID_API` (default `http://127.0.0.1:8080`), or override
//! per-invocation with `--api`.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
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
    /// Print machine-readable JSON instead of formatted text. Errors still
    /// go to stderr as plain text; use the process exit code to distinguish
    /// failure kinds in scripts.
    #[arg(long, global = true)]
    json: bool,
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
    /// Redeploy a branch at a prior commit instead of its current head.
    Rollback {
        /// Branch to roll back.
        branch: String,
        /// Specific commit to roll back to. Defaults to the deploy
        /// immediately before the current live one.
        #[arg(long)]
        to: Option<String>,
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
    /// Show the audit trail (deploy/pause/wake/destroy history).
    Audit {
        /// Branch to show the full history of. Omit for the most recent
        /// events across every project the daemon knows about.
        branch: Option<String>,
        /// Maximum number of events to show (default 50, only applies
        /// without `branch`).
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Download a consistent snapshot of the daemon's database + secret key.
    Backup {
        /// File to write the `.tar` archive to.
        file: String,
    },
    /// Upload a `.tar` produced by `oxid backup` to restore on the daemon's
    /// next restart (requires `OXID_ALLOW_RESTORE=1` on the daemon).
    Restore {
        /// The `.tar` archive to upload.
        file: String,
    },
    /// Manage named API tokens (requires the master `OXID_API_TOKEN`).
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Checks the daemon is reachable, reports its version/latency, and
    /// whether the configured `--token`/`OXID_TOKEN` authenticates.
    Doctor,
    /// Rotates the master encryption key: re-encrypts every secret under a
    /// fresh key with zero downtime (requires the master `OXID_API_TOKEN`).
    RotateKey,
    /// List deploys waiting for host capacity (see `oxid up`'s "queued"
    /// response), oldest/highest-priority first.
    Queue,
    /// Changes the current project's idle/lifetime policy — `oxid.toml`
    /// only ever seeds these at first registration otherwise.
    Configure {
        /// New idle timeout before scale-to-zero pause, e.g. `45m`.
        #[arg(long)]
        pause_after: Option<String>,
        /// New max lifetime before permanent teardown, e.g. `3d`.
        #[arg(long)]
        destroy_after: Option<String>,
        /// Git access token for a private repository (e.g. a GitHub PAT) —
        /// required for the daemon to clone/fetch it, since its own
        /// git-cache clone doesn't inherit any credential helper from your
        /// shell. Pass an empty string to clear it.
        #[arg(long)]
        git_token: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TokenAction {
    /// Mint a new named token. Prints the raw token once — it's never
    /// retrievable again after this.
    Create {
        /// Name identifying who/what this token is for.
        name: String,
    },
    /// List every token (revoked ones included), without the raw value.
    List,
    /// Revoke a token by id.
    Revoke {
        /// Token id, from `oxid token list`.
        id: u64,
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

/// Set once at the top of `main()` from `--json`, read everywhere output is
/// produced. A CLI process runs exactly one command per invocation on a
/// single thread, so a global here is simpler and just as correct as
/// threading a `json: bool` through every `cmd_*`/helper signature.
static JSON_MODE: AtomicBool = AtomicBool::new(false);

fn json_mode() -> bool {
    JSON_MODE.load(Ordering::Relaxed)
}

/// Prints a JSON value to stdout when `--json` is set. Returns whether it
/// did, so callers can skip their normal formatted-text output.
fn emit_json(value: &Value) -> bool {
    if json_mode() {
        println!("{value}");
        true
    } else {
        false
    }
}

fn ok(msg: impl std::fmt::Display) {
    if !json_mode() {
        println!("{GREEN}[+]{RESET} {msg}");
    }
}

fn bg(msg: impl std::fmt::Display) {
    if !json_mode() {
        println!("{GRAY}[~]{RESET} {msg}");
    }
}

fn action(msg: impl std::fmt::Display) {
    if !json_mode() {
        println!("{ORANGE}[>]{RESET} {msg}");
    }
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

/// Builds the address a human should actually open for `env`. Without
/// Traefik, `env["url"]` is a `branch.base-domain` hostname that only means
/// anything as a Traefik `Host()` rule — it isn't reachable at all without
/// DNS/hosts pointing it somewhere. Prefers `env["public_port"]` — the
/// branch's stable address (Oxid's own built-in zero-downtime proxy, which
/// stays the same across redeploys); falls back to `env["host_port"]` (the
/// container's own published port, which changes every redeploy) for
/// environments that predate `public_port`, then to the Traefik-style `url`
/// when neither exists.
fn env_display_address(base: &str, env: &Value) -> String {
    let fallback = env["url"].as_str().unwrap_or("?").to_owned();
    let port = env["public_port"]
        .as_u64()
        .or_else(|| env["host_port"].as_u64());
    let Some(port) = port else {
        return fallback;
    };
    let host = base
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', ':'])
        .next()
        .unwrap_or("127.0.0.1");
    format!("http://{host}:{port}/")
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

// ---------------------------------------------------------------------------
// Errors — a message plus an exit code a script can branch on, instead of
// every failure exiting `1` indistinguishably.
// ---------------------------------------------------------------------------

const EXIT_GENERIC: i32 = 1;
const EXIT_NOT_FOUND: i32 = 2;
const EXIT_UNREACHABLE: i32 = 3;
const EXIT_UNAUTHORIZED: i32 = 4;

#[derive(Debug)]
struct CliError {
    message: String,
    code: i32,
}

impl CliError {
    fn new(message: impl Into<String>, code: i32) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }
}

/// Any existing `.map_err(|e| format!(...))?` site keeps compiling
/// unchanged and gets the generic exit code — only the handful of call
/// sites that need a *specific* code (a daemon response with a real status,
/// or a connection failure) build a [`CliError`] explicitly.
impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self::new(message, EXIT_GENERIC)
    }
}

fn classify_status(status: StatusCode) -> i32 {
    match status {
        StatusCode::NOT_FOUND => EXIT_NOT_FOUND,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => EXIT_UNAUTHORIZED,
        _ => EXIT_GENERIC,
    }
}

/// Prints the daemon's error response and returns a [`CliError`] carrying
/// `context` and a code classified from `status`.
fn response_error(body: &str, status: StatusCode, context: &str) -> CliError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.to_owned());
    error(format!("{status}: {message}"));
    CliError::new(context, classify_status(status))
}

/// Builds a [`CliError`] for a request that never reached the daemon at
/// all (connection refused, DNS failure, timeout) — distinct from the
/// daemon replying with an error status.
fn connect_error(url: &str, e: &reqwest::Error) -> CliError {
    CliError::new(
        format!("cannot reach daemon at {url}: {e}"),
        EXIT_UNREACHABLE,
    )
}

// The CLI is a short-lived process making a handful of sequential HTTP
// calls — it never benefits from a multi-threaded work-stealing runtime,
// which would otherwise spin up one OS thread per CPU core just to run
// them one at a time. `current_thread` avoids that startup cost entirely.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    JSON_MODE.store(cli.json, Ordering::Relaxed);
    let base = api_base(cli.api.as_deref());
    let token = api_token(cli.token.as_deref());
    let client = match build_client(token.as_deref()) {
        Ok(client) => client,
        Err(message) => {
            error(message);
            std::process::exit(EXIT_GENERIC);
        }
    };

    let result = match cli.command {
        Command::Up { branch } => cmd_up(&client, &base, &branch).await,
        Command::Rollback { branch, to } => {
            cmd_rollback(&client, &base, &branch, to.as_deref()).await
        }
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
        Command::Audit { branch, limit } => {
            cmd_audit(&client, &base, branch.as_deref(), limit).await
        }
        Command::Backup { file } => cmd_backup(&client, &base, &file).await,
        Command::Restore { file } => cmd_restore(&client, &base, &file).await,
        Command::Token { action } => match action {
            TokenAction::Create { name } => cmd_token_create(&client, &base, &name).await,
            TokenAction::List => cmd_token_list(&client, &base).await,
            TokenAction::Revoke { id } => cmd_token_revoke(&client, &base, id).await,
        },
        Command::Doctor => cmd_doctor(&client, &base).await,
        Command::RotateKey => cmd_rotate_key(&client, &base).await,
        Command::Queue => cmd_queue(&client, &base).await,
        Command::Configure {
            pause_after,
            destroy_after,
            git_token,
        } => {
            cmd_configure(
                &client,
                &base,
                pause_after.as_deref(),
                destroy_after.as_deref(),
                git_token.as_deref(),
            )
            .await
        }
    };

    if let Err(err) = result {
        error(err.message);
        std::process::exit(err.code);
    }
}

/// Registers the repository in the current directory and returns its project.
///
/// Idempotent on the daemon side: repeated registrations return the existing
/// project.
async fn register_project(client: &Client, base: &str) -> Result<Value, CliError> {
    let repo_dir = std::env::current_dir().map_err(|e| format!("cannot resolve cwd: {e}"))?;
    let url = format!("{base}/api/v1/projects");
    let response = client
        .post(&url)
        .json(&json!({ "repo_dir": repo_dir.display().to_string() }))
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "project registration failed"));
    }
    let project: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    Ok(project)
}

async fn get_json(client: &Client, url: String) -> Result<Value, CliError> {
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "request failed"));
    }
    serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}").into())
}

/// Registers the project for `cwd`, then resolves `branch` to its single
/// running environment. Fails with a hint to `oxid status` when the branch
/// has no environment.
async fn resolve_environment(
    client: &Client,
    base: &str,
    branch: &str,
) -> Result<(u64, Value), CliError> {
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
        CliError::new(
            format!(
                "no environment found for branch `{branch}`; run `oxid status` to see what's live"
            ),
            EXIT_NOT_FOUND,
        )
    })?;
    let env_id = env["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing environment id".to_owned())?;
    Ok((env_id, env.clone()))
}

async fn cmd_up(client: &Client, base: &str, branch: &str) -> Result<(), CliError> {
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
    let url = format!("{base}/api/v1/projects/{project_id}/deploy");
    let response = client
        .post(&url)
        .json(&json!({ "branch": branch }))
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "deployment failed"));
    }
    let env: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if env["status"].as_str() == Some("queued") {
        if !emit_json(&env) {
            let position = env["position"].as_u64().unwrap_or(0);
            ok(format!(
                "Queued, waiting for host capacity (position {position}) — run `oxid queue` to check, or `oxid status` once it deploys"
            ));
        }
        return Ok(());
    }
    if !emit_json(&env) {
        ok(format!(
            "Environment live at: {}",
            env_display_address(base, &env)
        ));
    }
    Ok(())
}

async fn cmd_rollback(
    client: &Client,
    base: &str,
    branch: &str,
    to_sha: Option<&str>,
) -> Result<(), CliError> {
    action(format!("oxid rollback {branch}"));
    let project = register_project(client, base).await?;
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;

    let url = format!("{base}/api/v1/projects/{project_id}/rollback");
    let response = client
        .post(&url)
        .json(&json!({ "branch": branch, "to_sha": to_sha }))
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "rollback failed"));
    }
    let env: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if !emit_json(&env) {
        let sha = env["branch"]["commit_sha"].as_str().unwrap_or("?");
        ok(format!(
            "Rolled back to {sha} — environment live at: {}",
            env_display_address(base, &env)
        ));
    }
    Ok(())
}

async fn cmd_status(client: &Client, base: &str) -> Result<(), CliError> {
    let project = register_project(client, base).await?;
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;
    let envs = get_json(
        client,
        format!("{base}/api/v1/projects/{project_id}/environments"),
    )
    .await?;
    if emit_json(&envs) {
        return Ok(());
    }
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
    let mut latest: Vec<(&str, &str, String)> = Vec::new();
    for env in envs {
        let branch = env["branch"]["name"].as_str().unwrap_or("?");
        let state = env["state"].as_str().unwrap_or("?");
        let address = env_display_address(base, env);
        match latest.iter_mut().find(|(b, ..)| *b == branch) {
            Some(entry) => *entry = (branch, state, address),
            None => latest.push((branch, state, address)),
        }
    }
    println!("{:<24} {:<24} URL", "BRANCH", "STATE");
    for (branch, state, address) in latest {
        println!("{:<24} {:<33} {}", branch, colored_state(state), address);
    }
    Ok(())
}

async fn cmd_down(
    client: &Client,
    base: &str,
    branch: &str,
    force: bool,
    purge_secrets: bool,
) -> Result<(), CliError> {
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
        .delete(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(
            &body,
            status,
            "destroying environment failed",
        ));
    }
    if !emit_json(&json!({ "status": "destroyed", "branch": branch })) {
        ok(format!("Environment `{branch}` destroyed"));
        if purge_secrets {
            bg(format!("Branch `{branch}` secrets purged"));
        }
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

async fn cmd_rm_project(client: &Client, base: &str, force: bool) -> Result<(), CliError> {
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
    let url = format!("{base}/api/v1/projects/{project_id}");
    let response = client
        .delete(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "deleting project failed"));
    }
    if !emit_json(&json!({ "status": "deleted", "project": name })) {
        ok(format!("Project `{name}` deleted"));
    }
    Ok(())
}

async fn cmd_pause(client: &Client, base: &str, branch: &str) -> Result<(), CliError> {
    let (env_id, _) = resolve_environment(client, base, branch).await?;
    post_empty(client, format!("{base}/api/v1/environments/{env_id}/pause")).await?;
    if !emit_json(&json!({ "status": "paused", "branch": branch })) {
        bg(format!("Environment `{branch}` paused"));
    }
    Ok(())
}

async fn cmd_wake(client: &Client, base: &str, branch: &str) -> Result<(), CliError> {
    let (env_id, _) = resolve_environment(client, base, branch).await?;
    post_empty(client, format!("{base}/api/v1/environments/{env_id}/wake")).await?;
    if !emit_json(&json!({ "status": "woken", "branch": branch })) {
        ok(format!("Environment `{branch}` woken"));
    }
    Ok(())
}

async fn post_empty(client: &Client, url: String) -> Result<(), CliError> {
    let response = client
        .post(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "request failed"));
    }
    Ok(())
}

/// Fetches an environment's logs. Without `follow`, a one-shot snapshot;
/// with `follow`, consumes the daemon's SSE `/logs/stream` endpoint and
/// prints each `data:` line as it arrives — a real live stream, not polling.
async fn cmd_logs(client: &Client, base: &str, branch: &str, follow: bool) -> Result<(), CliError> {
    let (env_id, _) = resolve_environment(client, base, branch).await?;

    if !follow {
        let url = format!("{base}/api/v1/environments/{env_id}/logs");
        let value = get_json(client, url).await?;
        if emit_json(&value) {
            return Ok(());
        }
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
        .map_err(|e| connect_error(&url, &e))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(response_error(&body, status, "request failed"));
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

/// Formats an `AuditEvent.occurred_at` (`time::OffsetDateTime`'s default,
/// non-human-readable serde array: `[year, ordinal_day, hour, min, sec, ...]`)
/// into a compact, readable timestamp without pulling in a formatting crate
/// on the CLI side.
fn format_occurred_at(value: &Value) -> String {
    let parts = value.as_array().map_or(&[][..], Vec::as_slice);
    let get = |i: usize| parts.get(i).and_then(Value::as_i64).unwrap_or(0);
    format!(
        "{:04}-day{:03} {:02}:{:02}:{:02}",
        get(0),
        get(1),
        get(2),
        get(3),
        get(4)
    )
}

async fn cmd_audit(
    client: &Client,
    base: &str,
    branch: Option<&str>,
    limit: Option<u64>,
) -> Result<(), CliError> {
    let url = if let Some(branch) = branch {
        let (env_id, _) = resolve_environment(client, base, branch).await?;
        format!("{base}/api/v1/environments/{env_id}/audit")
    } else {
        let limit = limit.unwrap_or(50);
        format!("{base}/api/v1/audit?limit={limit}")
    };
    let value = get_json(client, url).await?;
    if emit_json(&value) {
        return Ok(());
    }
    let events = value
        .as_array()
        .ok_or_else(|| "invalid daemon response: expected an array".to_owned())?;
    if events.is_empty() {
        bg("No audit events yet.");
        return Ok(());
    }
    println!("{:<24} {:<14} DETAIL", "WHEN", "EVENT");
    for event in events {
        let when = format_occurred_at(&event["occurred_at"]);
        let kind = event["kind"].as_str().unwrap_or("?");
        let detail = event["detail"].as_str().unwrap_or("");
        println!("{when:<24} {kind:<14} {detail}");
    }
    Ok(())
}

async fn cmd_configure(
    client: &Client,
    base: &str,
    pause_after: Option<&str>,
    destroy_after: Option<&str>,
    git_token: Option<&str>,
) -> Result<(), CliError> {
    if pause_after.is_none() && destroy_after.is_none() && git_token.is_none() {
        return Err(
            "nothing to configure — pass --pause-after, --destroy-after and/or --git-token"
                .to_owned()
                .into(),
        );
    }
    let project = register_project(client, base).await?;
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;

    let url = format!("{base}/api/v1/projects/{project_id}");
    let response = client
        .patch(&url)
        .json(&json!({
            "pause_after": pause_after,
            "destroy_after": destroy_after,
            "git_token": git_token,
        }))
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(
            &body,
            status,
            "updating project settings failed",
        ));
    }
    let updated: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if !emit_json(&updated) {
        let mut msg = format!(
            "Updated `{}`: pause_after={} destroy_after={}",
            updated["name"].as_str().unwrap_or("?"),
            updated["config"]["pause_after"].as_str().unwrap_or("?"),
            updated["config"]["destroy_after"].as_str().unwrap_or("?"),
        );
        if let Some(token) = git_token {
            msg.push_str(if token.is_empty() {
                " git_token=cleared"
            } else {
                " git_token=set"
            });
        }
        ok(msg);
    }
    Ok(())
}

async fn cmd_queue(client: &Client, base: &str) -> Result<(), CliError> {
    let value = get_json(client, format!("{base}/api/v1/queue")).await?;
    if emit_json(&value) {
        return Ok(());
    }
    let entries = value
        .as_array()
        .ok_or_else(|| "invalid daemon response: expected an array".to_owned())?;
    if entries.is_empty() {
        bg("Queue is empty — every deploy has capacity.");
        return Ok(());
    }
    println!("{:<5} {:<24} {:<20} OPERATOR", "POS", "REQUESTED", "BRANCH");
    for (i, entry) in entries.iter().enumerate() {
        let when = format_occurred_at(&entry["requested_at"]);
        let branch = entry["branch"].as_str().unwrap_or("?");
        let operator = entry["operator"].as_str().unwrap_or("-");
        println!("{:<5} {when:<24} {branch:<20} {operator}", i + 1);
    }
    Ok(())
}

async fn cmd_backup(client: &Client, base: &str, file: &str) -> Result<(), CliError> {
    let url = format!("{base}/api/v1/backup");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(response_error(&body, status, "backup failed"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    std::fs::write(file, &bytes).map_err(|e| format!("cannot write `{file}`: {e}"))?;
    if !emit_json(&json!({ "status": "backed-up", "file": file, "bytes": bytes.len() })) {
        ok(format!(
            "Backup written to `{file}` ({} bytes)",
            bytes.len()
        ));
    }
    Ok(())
}

async fn cmd_restore(client: &Client, base: &str, file: &str) -> Result<(), CliError> {
    let bytes = std::fs::read(file).map_err(|e| format!("cannot read `{file}`: {e}"))?;
    let url = format!("{base}/api/v1/backup/restore");
    let response = client
        .post(&url)
        .body(bytes)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "restore failed"));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if !emit_json(&value) {
        let message = value["message"].as_str().unwrap_or("restore staged");
        ok(message);
    }
    Ok(())
}

async fn cmd_token_create(client: &Client, base: &str, name: &str) -> Result<(), CliError> {
    let url = format!("{base}/api/v1/tokens");
    let response = client
        .post(&url)
        .json(&json!({ "name": name }))
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "creating token failed"));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if !emit_json(&value) {
        let token = value["token"].as_str().unwrap_or("?");
        let id = value["id"].as_u64().unwrap_or(0);
        ok(format!("Token `{name}` created (id {id}): {token}"));
        bg("This is the only time the raw token is shown — store it now.");
    }
    Ok(())
}

async fn cmd_token_list(client: &Client, base: &str) -> Result<(), CliError> {
    let value = get_json(client, format!("{base}/api/v1/tokens")).await?;
    if emit_json(&value) {
        return Ok(());
    }
    let tokens = value
        .as_array()
        .ok_or_else(|| "invalid daemon response: expected an array".to_owned())?;
    if tokens.is_empty() {
        bg("No tokens issued yet.");
        return Ok(());
    }
    println!("{:<5} {:<24} {:<10} CREATED", "ID", "NAME", "STATUS");
    for token in tokens {
        let status = if token["revoked"].as_bool().unwrap_or(false) {
            "revoked"
        } else {
            "active"
        };
        println!(
            "{:<5} {:<24} {:<10} {}",
            token["id"].as_u64().unwrap_or_default(),
            token["name"].as_str().unwrap_or("?"),
            status,
            format_occurred_at(&token["created_at"]),
        );
    }
    Ok(())
}

async fn cmd_token_revoke(client: &Client, base: &str, id: u64) -> Result<(), CliError> {
    let url = format!("{base}/api/v1/tokens/{id}");
    let response = client
        .delete(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "revoking token failed"));
    }
    if !emit_json(&json!({ "status": "revoked", "id": id })) {
        ok(format!("Token {id} revoked"));
    }
    Ok(())
}

/// Preflight/diagnostic check: is the daemon reachable, what version is it
/// running, and (if a token is configured) does it actually authenticate —
/// instead of a command failing halfway through with a less obvious error.
async fn cmd_doctor(client: &Client, base: &str) -> Result<(), CliError> {
    let start = std::time::Instant::now();
    let url = format!("{base}/api/v1/health");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let latency = start.elapsed();
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "daemon health check failed"));
    }
    let health: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    let version = health["version"].as_str().unwrap_or("unknown");

    let (auth_status, auth_ok) = match get_json(client, format!("{base}/api/v1/projects")).await {
        Ok(_) => ("ok", true),
        Err(err) if err.code == EXIT_UNAUTHORIZED => ("unauthorized", false),
        Err(err) => return Err(err),
    };

    if !emit_json(&json!({
        "reachable": true,
        "version": version,
        "latency_ms": latency.as_millis(),
        "auth": auth_status,
    })) {
        ok(format!(
            "Daemon reachable at {base} (v{version}, {}ms)",
            latency.as_millis()
        ));
        if auth_ok {
            ok("Control API authenticates correctly");
        } else {
            bg(
                "Control API requires a token that wasn't provided/didn't work — pass --token or set OXID_TOKEN",
            );
        }
    }
    if auth_ok {
        Ok(())
    } else {
        Err(CliError::new(
            "authentication check failed",
            EXIT_UNAUTHORIZED,
        ))
    }
}

async fn cmd_rotate_key(client: &Client, base: &str) -> Result<(), CliError> {
    let url = format!("{base}/api/v1/rotate-key");
    let response = client
        .post(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "rotating master key failed"));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if !emit_json(&value) {
        ok("Master key rotated — every secret re-encrypted, zero downtime");
        if let Some(note) = value["note"].as_str() {
            bg(note);
        }
    }
    Ok(())
}

async fn cmd_ps(client: &Client, base: &str) -> Result<(), CliError> {
    let url = format!("{base}/api/v1/projects");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "listing projects failed"));
    }
    let projects: Vec<Value> =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if emit_json(&Value::Array(projects.clone())) {
        return Ok(());
    }
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
) -> Result<(Option<u64>, Option<String>), CliError> {
    match scope {
        "global" => Ok((None, None)),
        "project" => {
            if branch.is_some() {
                return Err("`--branch` is only allowed with `--scope branch`"
                    .to_owned()
                    .into());
            }
            Ok((project, None))
        }
        "branch" => {
            let branch = branch
                .map(str::to_owned)
                .ok_or_else(|| "`--branch` is required for `--scope branch`".to_owned())?;
            Ok((project, Some(branch)))
        }
        other => {
            Err(format!("invalid scope `{other}`; expected `global`, `project` or `branch`").into())
        }
    }
}

async fn ensure_project_id(
    client: &Client,
    base: &str,
    project: Option<u64>,
) -> Result<u64, CliError> {
    if let Some(id) = project {
        Ok(id)
    } else {
        let project = register_project(client, base).await?;
        project["id"]
            .as_u64()
            .ok_or_else(|| "daemon response missing project id".to_owned().into())
    }
}

fn parse_assignment(assignment: &str) -> Result<(&str, &str), CliError> {
    let (name, value) = assignment
        .split_once('=')
        .ok_or_else(|| format!("expected `KEY=VALUE`, got `{assignment}`"))?;
    if name.trim().is_empty() {
        return Err("secret name cannot be empty".to_owned().into());
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
) -> Result<(), CliError> {
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
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "setting secret failed"));
    }
    if !emit_json(&json!({ "status": "set", "name": name, "scope": scope })) {
        match branch {
            Some(b) => ok(format!("Secret `{name}` set for branch `{b}`")),
            None => ok(format!("Secret `{name}` set ({scope})")),
        }
    }
    Ok(())
}

async fn cmd_env_list(
    client: &Client,
    base: &str,
    scope: &str,
    project: Option<u64>,
    branch: Option<&str>,
) -> Result<(), CliError> {
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
        .get(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "listing secrets failed"));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    let secrets = value["secrets"].as_array().cloned().unwrap_or_default();
    if emit_json(&Value::Array(secrets.clone())) {
        return Ok(());
    }
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
) -> Result<(), CliError> {
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
        .delete(&url)
        .send()
        .await
        .map_err(|e| connect_error(&url, &e))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("cannot read daemon response: {e}"))?;
    if !status.is_success() {
        return Err(response_error(&body, status, "deleting secret failed"));
    }
    if !emit_json(&json!({ "status": "deleted", "name": name })) {
        ok(format!("Secret `{name}` deleted"));
    }
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
    fn parses_queue_command() {
        let cli = Cli::try_parse_from(["oxid", "queue"]).unwrap();
        assert!(matches!(cli.command, Command::Queue), "{:?}", cli.command);
    }

    #[test]
    fn parses_audit_without_branch() {
        let cli = Cli::try_parse_from(["oxid", "audit"]).unwrap();
        match cli.command {
            Command::Audit { branch, limit } => {
                assert_eq!(branch, None);
                assert_eq!(limit, None);
            }
            other => panic!("expected Audit, got {other:?}"),
        }
    }

    #[test]
    fn parses_audit_with_branch_and_limit() {
        let cli = Cli::try_parse_from(["oxid", "audit", "main", "--limit", "10"]).unwrap();
        match cli.command {
            Command::Audit { branch, limit } => {
                assert_eq!(branch.as_deref(), Some("main"));
                assert_eq!(limit, Some(10));
            }
            other => panic!("expected Audit, got {other:?}"),
        }
    }

    #[test]
    fn parses_backup_and_restore() {
        let cli = Cli::try_parse_from(["oxid", "backup", "out.tar"]).unwrap();
        match cli.command {
            Command::Backup { file } => assert_eq!(file, "out.tar"),
            other => panic!("expected Backup, got {other:?}"),
        }
        let cli = Cli::try_parse_from(["oxid", "restore", "out.tar"]).unwrap();
        match cli.command {
            Command::Restore { file } => assert_eq!(file, "out.tar"),
            other => panic!("expected Restore, got {other:?}"),
        }
    }

    #[test]
    fn parses_token_subcommands() {
        let cli = Cli::try_parse_from(["oxid", "token", "create", "alice"]).unwrap();
        match cli.command {
            Command::Token {
                action: TokenAction::Create { name },
            } => assert_eq!(name, "alice"),
            other => panic!("expected Token::Create, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["oxid", "token", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Token {
                action: TokenAction::List
            }
        ));

        let cli = Cli::try_parse_from(["oxid", "token", "revoke", "7"]).unwrap();
        match cli.command {
            Command::Token {
                action: TokenAction::Revoke { id },
            } => assert_eq!(id, 7),
            other => panic!("expected Token::Revoke, got {other:?}"),
        }
    }

    #[test]
    fn parses_doctor_command() {
        let cli = Cli::try_parse_from(["oxid", "doctor"]).unwrap();
        assert!(matches!(cli.command, Command::Doctor));
    }

    #[test]
    fn parses_rotate_key_command() {
        let cli = Cli::try_parse_from(["oxid", "rotate-key"]).unwrap();
        assert!(matches!(cli.command, Command::RotateKey));
    }

    #[test]
    fn parses_rollback_without_to() {
        let cli = Cli::try_parse_from(["oxid", "rollback", "main"]).unwrap();
        match cli.command {
            Command::Rollback { branch, to } => {
                assert_eq!(branch, "main");
                assert_eq!(to, None);
            }
            other => panic!("expected Rollback, got {other:?}"),
        }
    }

    #[test]
    fn parses_rollback_with_to() {
        let cli = Cli::try_parse_from(["oxid", "rollback", "main", "--to", "abc123"]).unwrap();
        match cli.command {
            Command::Rollback { branch, to } => {
                assert_eq!(branch, "main");
                assert_eq!(to.as_deref(), Some("abc123"));
            }
            other => panic!("expected Rollback, got {other:?}"),
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
    fn parses_global_json_flag() {
        let cli = Cli::try_parse_from(["oxid", "--json", "ps"]).unwrap();
        assert!(cli.json);
        let cli = Cli::try_parse_from(["oxid", "ps"]).unwrap();
        assert!(!cli.json);
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
        assert_eq!(parse_assignment("A=1").unwrap().0, "A");
        assert_eq!(parse_assignment("A=1").unwrap().1, "1");
        assert!(parse_assignment("missing-equals").is_err());
    }

    #[test]
    fn classify_status_distinguishes_error_kinds() {
        assert_eq!(classify_status(StatusCode::NOT_FOUND), EXIT_NOT_FOUND);
        assert_eq!(classify_status(StatusCode::UNAUTHORIZED), EXIT_UNAUTHORIZED);
        assert_eq!(classify_status(StatusCode::FORBIDDEN), EXIT_UNAUTHORIZED);
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            EXIT_GENERIC
        );
    }

    #[test]
    fn string_errors_default_to_the_generic_exit_code() {
        let err: CliError = "boom".to_owned().into();
        assert_eq!(err.code, EXIT_GENERIC);
        assert_eq!(err.message, "boom");
    }
}
