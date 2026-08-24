//! Oxid command-line interface.
//!
//! Thin client over the daemon's HTTP API (SPEC.md §5.1). Point it at a running
//! daemon with `OXID_API` (default `http://127.0.0.1:8080`), or override
//! per-invocation with `--api`.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

mod config;

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
    Status {
        /// Sort rows by `branch`, `state` or `updated` (deploy recency).
        /// Default is server order (unsorted).
        #[arg(long)]
        sort: Option<SortKey>,
        /// Keep only rows whose state matches exactly, or whose branch name
        /// contains this text (case-insensitive).
        #[arg(long)]
        filter: Option<String>,
    },
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
    Ps {
        /// Sort rows by `branch`, `state` or `updated`. `Ps` only has
        /// `branch`-equivalent (name) and no state/updated columns, but the
        /// flag is accepted for symmetry with `status`; only `branch` has
        /// an effect here.
        #[arg(long)]
        sort: Option<SortKey>,
        /// Keep only rows whose name contains this text (case-insensitive).
        #[arg(long)]
        filter: Option<String>,
    },
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
        /// Only events for this project id.
        #[arg(long)]
        project: Option<u64>,
        /// Only events for this branch name — a server-side filter on
        /// `GET /api/v1/audit`, distinct from the positional `branch` above
        /// (which instead resolves the current directory's environment and
        /// switches to its full per-environment history endpoint). Use this
        /// one together with `--project` to filter the cross-project feed
        /// without needing a local checkout of that branch.
        #[arg(long = "branch")]
        branch_filter: Option<String>,
        /// Only events at/after this RFC3339 timestamp (e.g.
        /// `2026-08-01T00:00:00Z`).
        #[arg(long)]
        since: Option<String>,
        /// Only events at/before this RFC3339 timestamp.
        #[arg(long)]
        until: Option<String>,
        /// Only events of this kind (e.g. `deploy`, `pause`, `wake`,
        /// `destroy`).
        #[arg(long)]
        kind: Option<String>,
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
    /// Show the daemon's Docker capacity (CPUs, memory) and how many
    /// environments are currently running.
    Stats,
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
    /// Manage named daemon contexts (`kubectl config`-style), persisted at
    /// `~/.config/oxid/config.toml`, so `--api`/`--token` don't need to be
    /// repeated for every daemon you talk to.
    Context {
        #[command(subcommand)]
        action: ContextAction,
    },
    /// Prints a shell completion script to stdout — pipe it into your
    /// shell's completion directory, e.g.
    /// `oxid completions zsh > ~/.zfunc/_oxid`.
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
    },
    /// Inspect or automate the Docker network + Traefik container real
    /// scale-to-zero (wake-on-request) needs — see `oxid infra status` and
    /// `oxid infra setup`.
    Infra {
        #[command(subcommand)]
        action: InfraAction,
    },
}

#[derive(Debug, Subcommand)]
enum InfraAction {
    /// Read-only: reports whether the Docker network, the built-in Traefik
    /// container, and this daemon's own wake-on-request wiring are in
    /// place. Never creates or changes anything.
    Status,
    /// Idempotently creates the Docker network and starts the built-in
    /// Traefik container if either is missing. Safe to re-run — an
    /// already-satisfied step is left untouched, not recreated.
    ///
    /// This never touches the daemon's own container/labels: Docker can't
    /// relabel a running container without recreating it, so that step is
    /// only ever detected and reported, never automated.
    Setup,
}

#[derive(Debug, Subcommand)]
enum ContextAction {
    /// Add (or overwrite) a named context.
    Add {
        /// Name for this context, e.g. `staging` or `prod`.
        name: String,
        /// Daemon base URL for this context.
        #[arg(long)]
        api: String,
        /// Bearer token for this context's daemon, if it requires one.
        #[arg(long)]
        token: Option<String>,
    },
    /// Switch the active context.
    Use {
        /// Name of a context previously added with `oxid context add`.
        name: String,
    },
    /// List every configured context; the active one is marked. Tokens are
    /// masked to their last 4 characters.
    List,
    /// Print the name of the active context.
    Current,
    /// Remove a context. Refuses if it's the active one unless `--force`.
    Remove {
        /// Name of the context to remove.
        name: String,
        /// Remove even if it's the currently active context.
        #[arg(long)]
        force: bool,
    },
}

/// Sort key shared by `status` and `ps` table output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SortKey {
    Branch,
    State,
    Updated,
}

#[derive(Debug, Subcommand)]
enum TokenAction {
    /// Mint a new named token. Prints the raw token once — it's never
    /// retrievable again after this.
    Create {
        /// Name identifying who/what this token is for.
        name: String,
        /// Scope the token to a project id (repeatable). Omit for full
        /// access; a scoped token can only act on its projects and gets
        /// 404s everywhere else.
        #[arg(long = "project")]
        project: Vec<u64>,
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

/// Precedence: `--api` flag > `OXID_API` env > active context in
/// `~/.config/oxid/config.toml` > hardcoded default. A config file that
/// fails to parse is treated the same as a missing one here — `oxid
/// context` subcommands are where a bad file gets surfaced as a real error.
fn api_base(cli_flag: Option<&str>) -> String {
    cli_flag
        .map(str::to_owned)
        .or_else(|| std::env::var("OXID_API").ok())
        .or_else(|| {
            config::load()
                .ok()
                .and_then(|cfg| config::current(&cfg).map(|ctx| ctx.api.clone()))
        })
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

/// Same precedence as [`api_base`]: `--token` flag > `OXID_TOKEN` env >
/// active context's token > none.
fn api_token(cli_flag: Option<&str>) -> Option<String> {
    cli_flag
        .map(str::to_owned)
        .or_else(|| std::env::var("OXID_TOKEN").ok())
        .or_else(|| {
            config::load()
                .ok()
                .and_then(|cfg| config::current(&cfg).and_then(|ctx| ctx.token.clone()))
        })
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
        // Bound only the *connection* phase: a hung daemon (or a firewalled
        // port that silently drops SYNs) must fail fast with the actionable
        // `connect_error` hint instead of blocking on the OS's ~2min TCP
        // timeout — or forever, once connected. There is deliberately NO
        // total request timeout: `logs -f`, a long build behind `up`, and
        // `backup` downloads are all legitimately slow responses.
        .connect_timeout(std::time::Duration::from_secs(10))
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
/// Conventional `SIGINT` exit code — `cmd_up` exits with this when the user
/// interrupts a long-running deploy (which continues server-side).
const EXIT_INTERRUPTED: i32 = 130;

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
/// daemon replying with an error status. Differentiates the common causes
/// with an actionable suggestion instead of just echoing `reqwest`'s raw
/// error text, since "cannot reach daemon: error sending request" reads the
/// same whether the daemon is down, the host doesn't resolve, or it's just
/// slow.
fn connect_error(url: &str, e: &reqwest::Error) -> CliError {
    let hint = if e.is_timeout() {
        "the daemon didn't respond in time — it may be overloaded or stuck; \
         retry, or check its logs on the host running `oxidd`"
    } else if e.is_connect() {
        "connection refused/unreachable — check the daemon is running and that \
         --api/OXID_API points at the right host and port"
    } else if e.to_string().contains("dns error")
        || e.to_string().contains("failed to lookup address")
    {
        "DNS resolution failed — check the hostname in --api/OXID_API is spelled \
         correctly and resolves from this machine"
    } else {
        "the request never reached the daemon"
    };
    CliError::new(
        format!("cannot reach daemon at {url}: {e} ({hint})"),
        EXIT_UNREACHABLE,
    )
}

// The CLI is a short-lived process making a handful of sequential HTTP
// calls — it never benefits from a multi-threaded work-stealing runtime,
// which would otherwise spin up one OS thread per CPU core just to run
// them one at a time. `current_thread` avoids that startup cost entirely.
// A single flat `match` over every subcommand is what pushes this past
// clippy's line-count heuristic; splitting the dispatch table into its own
// function would just move the length elsewhere for no clarity gain.
#[allow(clippy::too_many_lines)]
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
        Command::Status { sort, filter } => {
            cmd_status(&client, &base, sort, filter.as_deref()).await
        }
        Command::Down {
            branch,
            force,
            purge_secrets,
        } => cmd_down(&client, &base, &branch, force, purge_secrets).await,
        Command::RmProject { force } => cmd_rm_project(&client, &base, force).await,
        Command::Pause { branch } => cmd_pause(&client, &base, &branch).await,
        Command::Wake { branch } => cmd_wake(&client, &base, &branch).await,
        Command::Ps { sort, filter } => cmd_ps(&client, &base, sort, filter.as_deref()).await,
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
        Command::Audit {
            branch,
            limit,
            project,
            branch_filter,
            since,
            until,
            kind,
        } => {
            cmd_audit(
                &client,
                &base,
                branch.as_deref(),
                limit,
                AuditFilters {
                    project,
                    branch: branch_filter,
                    since,
                    until,
                    kind,
                },
            )
            .await
        }
        Command::Backup { file } => cmd_backup(&client, &base, &file).await,
        Command::Restore { file } => cmd_restore(&client, &base, &file).await,
        Command::Token { action } => match action {
            TokenAction::Create { name, project } => {
                cmd_token_create(&client, &base, &name, &project).await
            }
            TokenAction::List => cmd_token_list(&client, &base).await,
            TokenAction::Revoke { id } => cmd_token_revoke(&client, &base, id).await,
        },
        Command::Doctor => cmd_doctor(&client, &base).await,
        Command::RotateKey => cmd_rotate_key(&client, &base).await,
        Command::Queue => cmd_queue(&client, &base).await,
        Command::Stats => cmd_stats(&client, &base).await,
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
        Command::Context { action } => cmd_context(action),
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_owned();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(())
        }
        Command::Infra { action } => match action {
            InfraAction::Status => cmd_infra_status(&client, &base).await,
            InfraAction::Setup => cmd_infra_setup(&client, &base).await,
        },
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
    let request = async {
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
        serde_json::from_str::<Value>(&body)
            .map_err(|e| format!("invalid daemon response: {e}").into())
    };
    // A build can take minutes; Ctrl+C must not leave the user thinking
    // they cancelled it. The daemon keeps deploying — say so, loudly.
    let env: Value = tokio::select! {
        result = request => result?,
        _ = tokio::signal::ctrl_c() => {
            error("Interrupted — the deploy keeps running server-side; check it with `oxid status` or `oxid queue`");
            std::process::exit(EXIT_INTERRUPTED);
        }
    };
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
        print_deploy_report(&env);
        ok(format!(
            "Environment live at: {}",
            env_display_address(base, &env)
        ));
    }
    Ok(())
}

/// Prints the build/provisioning half of a deploy response — the
/// `"build"`/`"dependencies"` sibling keys the daemon attaches alongside
/// the environment (DESIGN.md §3.3's "[+] Shared Postgres instance
/// detected. Created `db_feature_login` → [>] Building image (Cache hit:
/// 85%)"). Both keys are optional: older daemons and *queued* deploys
/// don't carry them, in which case nothing extra is printed.
fn print_deploy_report(env: &Value) {
    if let Some(build) = env.get("build") {
        let took = format_duration_ms(build["duration_ms"].as_u64().unwrap_or(0));
        match build["cache_hit_percent"].as_u64().map(u8::try_from) {
            Some(Ok(pct)) => ok(format!("Image built (cache hit: {pct}%, {took})")),
            _ => ok(format!("Image built ({took})")),
        }
    }
    for dep in env["dependencies"].as_array().into_iter().flatten() {
        if let Some(line) = dep.as_str() {
            ok(line);
        }
    }
}

/// Human-friendly milliseconds: whole ms below 10s, one decimal in s above.
fn format_duration_ms(ms: u64) -> String {
    if ms >= 10_000 {
        #[allow(clippy::cast_precision_loss)]
        let seconds = ms as f64 / 1000.0;
        format!("{seconds:.1}s")
    } else {
        format!("{ms}ms")
    }
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
        print_deploy_report(&env);
        let sha = env["branch"]["commit_sha"].as_str().unwrap_or("?");
        ok(format!(
            "Rolled back to {sha} — environment live at: {}",
            env_display_address(base, &env)
        ));
    }
    Ok(())
}

/// Extracts a lexicographically-comparable key from a `time`-serialized
/// timestamp field (an array like `occurred_at`'s: `[year, ordinal, hour,
/// minute, second, ...]`). Comparing the arrays element-by-element sorts
/// chronologically since each field is listed in decreasing significance.
fn timestamp_sort_key(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .map(|parts| parts.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// Applies `--filter` to a row: matches an exact (case-insensitive) state,
/// or a case-insensitive substring of the branch/name.
fn row_matches_filter(branch_or_name: &str, state: Option<&str>, filter: &str) -> bool {
    let filter = filter.to_lowercase();
    if let Some(state) = state
        && state.to_lowercase() == filter
    {
        return true;
    }
    branch_or_name.to_lowercase().contains(&filter)
}

async fn cmd_status(
    client: &Client,
    base: &str,
    sort: Option<SortKey>,
    filter: Option<&str>,
) -> Result<(), CliError> {
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
    // The daemon keeps one row per historical deploy (audit trail); only the
    // most recent row per branch reflects what's actually live. `envs` is
    // returned in ascending id order, so a later entry always overwrites an
    // earlier one for the same branch here.
    let mut latest: Vec<(&str, &str, String, Vec<i64>)> = Vec::new();
    for env in envs {
        let branch = env["branch"]["name"].as_str().unwrap_or("?");
        let state = env["state"].as_str().unwrap_or("?");
        let address = env_display_address(base, env);
        let updated = timestamp_sort_key(&env["updated_at"]);
        match latest.iter_mut().find(|(b, ..)| *b == branch) {
            Some(entry) => *entry = (branch, state, address, updated),
            None => latest.push((branch, state, address, updated)),
        }
    }
    if let Some(filter) = filter {
        latest.retain(|(branch, state, ..)| row_matches_filter(branch, Some(state), filter));
    }
    match sort {
        Some(SortKey::Branch) => latest.sort_by(|a, b| a.0.cmp(b.0)),
        Some(SortKey::State) => latest.sort_by(|a, b| a.1.cmp(b.1)),
        Some(SortKey::Updated) => latest.sort_by(|a, b| a.3.cmp(&b.3)),
        None => {}
    }
    if emit_json(&json!(
        latest
            .iter()
            .map(|(branch, state, address, _)| json!({
                "branch": branch,
                "state": state,
                "url": address,
            }))
            .collect::<Vec<_>>()
    )) {
        return Ok(());
    }
    if latest.is_empty() {
        bg(format!(
            "No environments for `{}` yet. Deploy one with `oxid up <branch>`.",
            project["name"].as_str().unwrap_or("?")
        ));
        return Ok(());
    }
    println!("{:<24} {:<24} URL", "BRANCH", "STATE");
    for (branch, state, address, _) in latest {
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

/// Server-side filters for `GET /api/v1/audit` and
/// `GET /api/v1/environments/{id}/audit` (`project_id`, `branch`, `since`,
/// `until`, `kind`) — the daemon side of this contract may still be landing
/// in parallel; an unknown query param is either ignored or answered with a
/// `400`, which surfaces through the CLI's existing `response_error` path
/// either way.
#[derive(Debug, Default, Clone)]
struct AuditFilters {
    project: Option<u64>,
    branch: Option<String>,
    since: Option<String>,
    until: Option<String>,
    kind: Option<String>,
}

impl AuditFilters {
    /// Appends `&key=value` pairs (percent-encoding reserved characters in
    /// values) to `url`.
    fn append_to(&self, url: &mut String) {
        use std::fmt::Write as _;
        if let Some(project) = self.project {
            let _ = write!(url, "&project_id={project}");
        }
        if let Some(branch) = &self.branch {
            let _ = write!(url, "&branch={}", percent_encode(branch));
        }
        if let Some(since) = &self.since {
            let _ = write!(url, "&since={}", percent_encode(since));
        }
        if let Some(until) = &self.until {
            let _ = write!(url, "&until={}", percent_encode(until));
        }
        if let Some(kind) = &self.kind {
            let _ = write!(url, "&kind={}", percent_encode(kind));
        }
    }
}

/// Minimal query-value percent-encoding — just enough for the handful of
/// reserved characters that show up in RFC3339 timestamps and branch names
/// (`:`, `+`, spaces), without pulling in the `url` crate for it.
fn percent_encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

async fn cmd_audit(
    client: &Client,
    base: &str,
    branch: Option<&str>,
    limit: Option<u64>,
    filters: AuditFilters,
) -> Result<(), CliError> {
    let url = if let Some(branch) = branch {
        let (env_id, _) = resolve_environment(client, base, branch).await?;
        let mut url = format!("{base}/api/v1/environments/{env_id}/audit?");
        filters.append_to(&mut url);
        url
    } else {
        let limit = limit.unwrap_or(50);
        let mut url = format!("{base}/api/v1/audit?limit={limit}");
        filters.append_to(&mut url);
        url
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

/// Handles `oxid context <add|use|list|current|remove>` — purely local,
/// no daemon round-trip, so it's synchronous unlike every other `cmd_*`.
fn cmd_context(action: ContextAction) -> Result<(), CliError> {
    match action {
        ContextAction::Add { name, api, token } => cmd_context_add(&name, api, token),
        ContextAction::Use { name } => cmd_context_use(&name),
        ContextAction::List => cmd_context_list(),
        ContextAction::Current => cmd_context_current(),
        ContextAction::Remove { name, force } => cmd_context_remove(&name, force),
    }
}

fn cmd_context_add(name: &str, api: String, token: Option<String>) -> Result<(), CliError> {
    let mut cfg = config::load()?;
    cfg.contexts
        .insert(name.to_owned(), config::Context { api, token });
    config::save(&cfg)?;
    if !emit_json(&json!({ "context": name, "action": "added" })) {
        ok(format!("Added context `{name}`"));
    }
    Ok(())
}

fn cmd_context_use(name: &str) -> Result<(), CliError> {
    let mut cfg = config::load()?;
    if !cfg.contexts.contains_key(name) {
        return Err(CliError::new(
            format!(
                "no such context `{name}` — add it first with `oxid context add {name} --api <url>`"
            ),
            EXIT_NOT_FOUND,
        ));
    }
    cfg.current_context = Some(name.to_owned());
    config::save(&cfg)?;
    if !emit_json(&json!({ "context": name, "action": "activated" })) {
        ok(format!("Switched to context `{name}`"));
    }
    Ok(())
}

fn cmd_context_list() -> Result<(), CliError> {
    let cfg = config::load()?;
    if emit_json(&json!(
        cfg.contexts
            .iter()
            .map(|(name, ctx)| json!({
                "name": name,
                "api": ctx.api,
                "token": ctx.token.as_deref().map(config::mask_token),
                "current": cfg.current_context.as_deref() == Some(name.as_str()),
            }))
            .collect::<Vec<_>>()
    )) {
        return Ok(());
    }
    if cfg.contexts.is_empty() {
        bg("No contexts configured yet. Add one with `oxid context add <name> --api <url>`.");
        return Ok(());
    }
    println!("{:<4} {:<16} {:<32} TOKEN", "", "NAME", "API");
    for (name, ctx) in &cfg.contexts {
        let marker = if cfg.current_context.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            ""
        };
        let token = ctx
            .token
            .as_deref()
            .map_or("-".to_owned(), config::mask_token);
        println!("{marker:<4} {name:<16} {:<32} {token}", ctx.api);
    }
    Ok(())
}

fn cmd_context_current() -> Result<(), CliError> {
    let cfg = config::load()?;
    match &cfg.current_context {
        Some(name) if cfg.contexts.contains_key(name) => {
            if !emit_json(&json!({ "context": name })) {
                println!("{name}");
            }
            Ok(())
        }
        _ => {
            if !emit_json(&json!({ "context": Value::Null })) {
                bg("No active context (using --api/OXID_API/default).");
            }
            Ok(())
        }
    }
}

fn cmd_context_remove(name: &str, force: bool) -> Result<(), CliError> {
    let mut cfg = config::load()?;
    if !cfg.contexts.contains_key(name) {
        return Err(CliError::new(
            format!("no such context `{name}`"),
            EXIT_NOT_FOUND,
        ));
    }
    let is_active = cfg.current_context.as_deref() == Some(name);
    if is_active && !force {
        return Err(format!(
            "`{name}` is the active context — pass --force to remove it anyway (falls back to --api/OXID_API/default)"
        )
        .into());
    }
    cfg.contexts.remove(name);
    if is_active {
        cfg.current_context = None;
    }
    config::save(&cfg)?;
    if !emit_json(&json!({ "context": name, "action": "removed" })) {
        ok(format!("Removed context `{name}`"));
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

async fn cmd_token_create(
    client: &Client,
    base: &str,
    name: &str,
    projects: &[u64],
) -> Result<(), CliError> {
    let url = format!("{base}/api/v1/tokens");
    let mut payload = json!({ "name": name });
    if !projects.is_empty() {
        payload["projects"] = json!(projects);
    }
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
        return Err(response_error(&body, status, "creating token failed"));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if !emit_json(&value) {
        let token = value["token"].as_str().unwrap_or("?");
        let id = value["id"].as_u64().unwrap_or(0);
        ok(format!("Token `{name}` created (id {id}): {token}"));
        if projects.is_empty() {
            bg("Unscoped: this token has full access. Re-create it with --project to limit it.");
        } else {
            bg(format!(
                "Scoped to project ids {projects:?} — every other project answers 404 to this \
                 token."
            ));
        }
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
    println!(
        "{:<5} {:<24} {:<10} {:<12} CREATED",
        "ID", "NAME", "STATUS", "SCOPES"
    );
    for token in tokens {
        let status = if token["revoked"].as_bool().unwrap_or(false) {
            "revoked"
        } else {
            "active"
        };
        // `null` = unscoped (full access); an array renders as its ids.
        let scopes = match token["scoped_projects"].as_array() {
            Some(ids) => ids
                .iter()
                .map(|v| v.as_u64().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
                .join(","),
            None => "-".to_owned(),
        };
        println!(
            "{:<5} {:<24} {:<10} {:<12} {}",
            token["id"].as_u64().unwrap_or_default(),
            token["name"].as_str().unwrap_or("?"),
            status,
            scopes,
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
/// Compares `x.y.z` version strings, returning `true` when the major
/// component differs — the only mismatch severe enough to warn about, since
/// this workspace doesn't yet promise semver compatibility within a major
/// version but a CLI/daemon pair built from the same release always match
/// exactly anyway.
fn major_version_mismatch(cli_version: &str, daemon_version: &str) -> bool {
    let major = |v: &str| v.split('.').next().unwrap_or(v).to_owned();
    daemon_version != "unknown" && major(cli_version) != major(daemon_version)
}

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
    let daemon_version = health["version"].as_str().unwrap_or("unknown").to_owned();
    let cli_version = env!("CARGO_PKG_VERSION");
    let version_mismatch = major_version_mismatch(cli_version, &daemon_version);

    let (auth_status, auth_ok) = match get_json(client, format!("{base}/api/v1/projects")).await {
        Ok(_) => ("ok", true),
        Err(err) if err.code == EXIT_UNAUTHORIZED => ("unauthorized", false),
        Err(err) => return Err(err),
    };

    // Best-effort: `/api/v1/stats` and `/api/v1/infra/status` need the same
    // auth as everything else, and predate on daemons old enough to 404
    // them, so their failure never fails `doctor` outright — they're
    // capacity/config diagnostics, not a correctness gate.
    let node_stats = if auth_ok {
        match get_json(client, format!("{base}/api/v1/stats")).await {
            Ok(node_stats) => Some(Ok(node_stats)),
            Err(err) => Some(Err(err.message)),
        }
    } else {
        None
    };
    let infra = if auth_ok {
        Some(get_json(client, format!("{base}/api/v1/infra/status")).await)
    } else {
        None
    };

    if !emit_json(&json!({
        "reachable": true,
        "version": daemon_version,
        "cli_version": cli_version,
        "version_mismatch": version_mismatch,
        "latency_ms": latency.as_millis(),
        "auth": auth_status,
        "capacity": node_stats.as_ref().map(|c| match c {
            Ok(node_stats) => node_stats.clone(),
            Err(e) => json!({ "error": e }),
        }),
        "infra": infra.as_ref().map(|r| match r {
            Ok(value) => value.clone(),
            Err(e) => json!({ "error": e.message }),
        }),
    })) {
        print_doctor_report(
            base,
            &daemon_version,
            cli_version,
            version_mismatch,
            latency,
            auth_ok,
            node_stats.as_ref(),
        );
        match infra.as_ref() {
            Some(Ok(value)) => print_infra_report(value),
            Some(Err(err)) if err.code == EXIT_NOT_FOUND => bg(
                "Direct-publish mode: OXID_DOCKER_NETWORK is not configured — scale-to-zero is \
                 DISABLED (no idle auto-pause; environments run until manually paused/destroyed). \
                 Run `oxid infra setup` on the host to enable the supported Traefik topology",
            ),
            Some(Err(err)) => bg(format!(
                "Could not fetch infra status ({}) — the daemon may predate \
                 `/api/v1/infra/status`; upgrade it to enable this check",
                err.message
            )),
            None => {}
        }
    }
    if !auth_ok {
        return Err(CliError::new(
            "authentication check failed",
            EXIT_UNAUTHORIZED,
        ));
    }
    Ok(())
}

fn print_doctor_report(
    base: &str,
    daemon_version: &str,
    cli_version: &str,
    version_mismatch: bool,
    latency: std::time::Duration,
    auth_ok: bool,
    node_stats: Option<&Result<Value, String>>,
) {
    ok(format!(
        "Daemon reachable at {base} (v{daemon_version}, {}ms)",
        latency.as_millis()
    ));
    if auth_ok {
        ok("Control API authenticates correctly");
    } else {
        bg(
            "Control API requires a token that wasn't provided/didn't work — pass --token or set OXID_TOKEN",
        );
    }
    if version_mismatch {
        bg(format!(
            "CLI is v{cli_version} but daemon is v{daemon_version} (major version mismatch) \
             — upgrade whichever is older; a mismatch across major versions may break API compatibility"
        ));
    } else if daemon_version != "unknown" {
        ok(format!(
            "CLI (v{cli_version}) and daemon (v{daemon_version}) versions match"
        ));
    }
    match node_stats {
        Some(Ok(node_stats)) => print_node_stats(node_stats),
        Some(Err(message)) => bg(format!(
            "Could not fetch capacity stats ({message}) — the daemon may predate \
             `/api/v1/stats`; upgrade it to enable this check"
        )),
        None => {}
    }
}

/// Renders one `GET /api/v1/stats` payload — shared by `doctor`'s capacity
/// section and `oxid stats`.
fn print_node_stats(node_stats: &Value) {
    let mem_bytes = node_stats["host_total_memory_bytes"].as_u64().unwrap_or(0);
    let cpus = node_stats["host_cpu_count"].as_u64().unwrap_or(0);
    if mem_bytes == 0 || cpus == 0 {
        bg(
            "Daemon reports 0 host memory/CPUs — its Docker socket may be \
             unreachable; check OXID's container has /var/run/docker.sock mounted \
             and the daemon user can access it",
        );
    } else {
        #[allow(clippy::cast_precision_loss)]
        let mem_gib = mem_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        ok(format!(
            "Docker capacity: {cpus} CPU(s), {mem_gib:.1} GiB memory, {} env(s) running",
            node_stats["environments_running"].as_u64().unwrap_or(0),
        ));
    }
}

/// Read-only: `GET /api/v1/stats` — host capacity and running-environment
/// count, standalone for scripts/monitoring (`--json` emits the raw
/// object). `doctor` runs this same check as one of its diagnostics.
async fn cmd_stats(client: &Client, base: &str) -> Result<(), CliError> {
    let value = get_json(client, format!("{base}/api/v1/stats")).await?;
    if !emit_json(&value) {
        print_node_stats(&value);
    }
    Ok(())
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

/// Read-only: `GET /api/v1/infra/status`. Never creates or changes
/// anything on the daemon.
async fn cmd_infra_status(client: &Client, base: &str) -> Result<(), CliError> {
    let value = get_json(client, format!("{base}/api/v1/infra/status")).await?;
    if !emit_json(&value) {
        print_infra_report(&value);
    }
    Ok(())
}

/// `POST /api/v1/infra/bootstrap` — idempotent: creates the Docker network
/// and/or starts the built-in Traefik container only if either is missing,
/// otherwise reports what was already there. Safe to run repeatedly.
async fn cmd_infra_setup(client: &Client, base: &str) -> Result<(), CliError> {
    let url = format!("{base}/api/v1/infra/bootstrap");
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
        return Err(response_error(&body, status, "infra setup failed"));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if !emit_json(&value) {
        print_infra_report(&value);
    }
    Ok(())
}

/// Shared formatted-text rendering of an `InfraStatus` JSON body, used by
/// both `oxid infra status` and `oxid infra setup` (their responses have
/// the same shape).
fn print_infra_report(value: &Value) {
    let network = value["network"].as_str().unwrap_or("?");
    if value["network_exists"].as_bool().unwrap_or(false) {
        ok(format!("Docker network `{network}` exists"));
    } else {
        bg(format!("Docker network `{network}` does not exist"));
    }

    match value["traefik_status"].as_str().unwrap_or("missing") {
        "running" => ok("Traefik is running"),
        "paused" => bg("Traefik container exists but is paused"),
        "stopped" => bg("Traefik container exists but is stopped"),
        _ => bg("Traefik is not running"),
    }

    match value["self_wiring"]["state"].as_str().unwrap_or("unknown") {
        "not_containerized" => bg("Daemon isn't running inside Docker — self-wiring check skipped"),
        "detected" => {
            let wiring = &value["self_wiring"];
            let joined = wiring["joined_network"].as_bool().unwrap_or(false);
            let labeled = wiring["has_traefik_enable_label"]
                .as_bool()
                .unwrap_or(false);
            let wake = wiring["references_oxid_wake"].as_bool().unwrap_or(false);
            if joined && labeled && wake {
                ok("This daemon's own container is fully wired for wake-on-request");
            } else {
                bg("This daemon's own container is NOT fully wired for wake-on-request");
            }
        }
        _ => bg("Could not determine this daemon's own container wiring"),
    }

    if let Some(steps) = value["next_steps"].as_array() {
        for step in steps {
            if let Some(step) = step.as_str() {
                action(step);
            }
        }
    }
}

async fn cmd_ps(
    client: &Client,
    base: &str,
    sort: Option<SortKey>,
    filter: Option<&str>,
) -> Result<(), CliError> {
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
    let mut projects: Vec<Value> =
        serde_json::from_str(&body).map_err(|e| format!("invalid daemon response: {e}"))?;
    if let Some(filter) = filter {
        projects.retain(|p| row_matches_filter(p["name"].as_str().unwrap_or(""), None, filter));
    }
    // `Ps` lists projects, which have no `state`/`updated` columns — `--sort`
    // only has something to act on via the project name, so every key sorts
    // by name here; it's still accepted (rather than rejected) so scripts
    // that pass `--sort` uniformly across `status`/`ps` don't need a
    // special case.
    if sort.is_some() {
        projects.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
    }
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
            Command::Audit { branch, limit, .. } => {
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
            Command::Audit { branch, limit, .. } => {
                assert_eq!(branch.as_deref(), Some("main"));
                assert_eq!(limit, Some(10));
            }
            other => panic!("expected Audit, got {other:?}"),
        }
    }

    #[test]
    fn parses_audit_with_new_filters() {
        let cli = Cli::try_parse_from([
            "oxid",
            "audit",
            "--project",
            "3",
            "--branch",
            "feat-a",
            "--since",
            "2026-08-01T00:00:00Z",
            "--until",
            "2026-08-20T00:00:00Z",
            "--kind",
            "deploy",
        ])
        .unwrap();
        match cli.command {
            Command::Audit {
                branch,
                project,
                branch_filter,
                since,
                until,
                kind,
                ..
            } => {
                assert_eq!(branch, None);
                assert_eq!(project, Some(3));
                assert_eq!(branch_filter.as_deref(), Some("feat-a"));
                assert_eq!(since.as_deref(), Some("2026-08-01T00:00:00Z"));
                assert_eq!(until.as_deref(), Some("2026-08-20T00:00:00Z"));
                assert_eq!(kind.as_deref(), Some("deploy"));
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
                action: TokenAction::Create { name, project },
            } => {
                assert_eq!(name, "alice");
                assert!(project.is_empty(), "no --project flags means unscoped");
            }
            other => panic!("expected Token::Create, got {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "oxid",
            "token",
            "create",
            "ci-bot",
            "--project",
            "1",
            "--project",
            "3",
        ])
        .unwrap();
        match cli.command {
            Command::Token {
                action: TokenAction::Create { name, project },
            } => {
                assert_eq!(name, "ci-bot");
                assert_eq!(project, vec![1, 3]);
            }
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
    fn parses_context_subcommands() {
        let cli = Cli::try_parse_from([
            "oxid",
            "context",
            "add",
            "staging",
            "--api",
            "http://staging:8080",
            "--token",
            "s3cr3t",
        ])
        .unwrap();
        match cli.command {
            Command::Context {
                action: ContextAction::Add { name, api, token },
            } => {
                assert_eq!(name, "staging");
                assert_eq!(api, "http://staging:8080");
                assert_eq!(token.as_deref(), Some("s3cr3t"));
            }
            other => panic!("expected Context::Add, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["oxid", "context", "use", "staging"]).unwrap();
        match cli.command {
            Command::Context {
                action: ContextAction::Use { name },
            } => assert_eq!(name, "staging"),
            other => panic!("expected Context::Use, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["oxid", "context", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Context {
                action: ContextAction::List
            }
        ));

        let cli = Cli::try_parse_from(["oxid", "context", "current"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Context {
                action: ContextAction::Current
            }
        ));

        let cli = Cli::try_parse_from(["oxid", "context", "remove", "staging", "--force"]).unwrap();
        match cli.command {
            Command::Context {
                action: ContextAction::Remove { name, force },
            } => {
                assert_eq!(name, "staging");
                assert!(force);
            }
            other => panic!("expected Context::Remove, got {other:?}"),
        }
    }

    #[test]
    fn parses_completions_command() {
        let cli = Cli::try_parse_from(["oxid", "completions", "zsh"]).unwrap();
        match cli.command {
            Command::Completions { shell } => assert_eq!(shell, Shell::Zsh),
            other => panic!("expected Completions, got {other:?}"),
        }
    }

    #[test]
    fn parses_infra_status_command() {
        let cli = Cli::try_parse_from(["oxid", "infra", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Infra {
                action: InfraAction::Status
            }
        ));
    }

    #[test]
    fn parses_infra_setup_command() {
        let cli = Cli::try_parse_from(["oxid", "infra", "setup"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Infra {
                action: InfraAction::Setup
            }
        ));
    }

    #[test]
    fn formats_durations_for_humans() {
        assert_eq!(format_duration_ms(850), "850ms");
        assert_eq!(format_duration_ms(9_999), "9999ms");
        assert_eq!(format_duration_ms(10_000), "10.0s");
        assert_eq!(format_duration_ms(41_230), "41.2s");
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
        assert!(matches!(
            cli.command,
            Command::Ps {
                sort: None,
                filter: None
            }
        ));
    }

    #[test]
    fn parses_ps_with_sort_and_filter() {
        let cli =
            Cli::try_parse_from(["oxid", "ps", "--sort", "branch", "--filter", "web"]).unwrap();
        match cli.command {
            Command::Ps { sort, filter } => {
                assert_eq!(sort, Some(SortKey::Branch));
                assert_eq!(filter.as_deref(), Some("web"));
            }
            other => panic!("expected Ps, got {other:?}"),
        }
    }

    #[test]
    fn parses_status_command() {
        let cli = Cli::try_parse_from(["oxid", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Status {
                sort: None,
                filter: None
            }
        ));
    }

    #[test]
    fn parses_status_with_sort_and_filter() {
        let cli = Cli::try_parse_from(["oxid", "status", "--sort", "state", "--filter", "running"])
            .unwrap();
        match cli.command {
            Command::Status { sort, filter } => {
                assert_eq!(sort, Some(SortKey::State));
                assert_eq!(filter.as_deref(), Some("running"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
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
