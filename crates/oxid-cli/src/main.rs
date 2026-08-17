//! Oxid command-line interface.
//!
//! Thin client over the daemon's HTTP API (SPEC.md §5.1). Point it at a running
//! daemon with `OXID_API` (default `http://127.0.0.1:8080`).

use clap::{Parser, Subcommand};
use reqwest::Client;
use serde_json::Value;

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
    /// Manage environment variables (not implemented yet).
    Env {
        /// Variable to set, e.g. `DB_PASSWORD=secret`.
        assignment: String,
        /// Scope of the variable.
        #[arg(long, default_value = "global")]
        scope: String,
    },
    /// Stream logs of a branch environment (not implemented yet).
    Logs {
        /// Branch whose logs to follow.
        branch: String,
        /// Follow output as it is written.
        #[arg(long, short)]
        follow: bool,
    },
}

fn api_base() -> String {
    std::env::var("OXID_API").unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned())
}

fn report_error(body: &str, status: reqwest::StatusCode) {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.to_owned());
    eprintln!("[!] {status}: {message}");
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = Client::new();
    let base = api_base();

    let result = match cli.command {
        Command::Up { branch } => cmd_up(&client, &base, &branch).await,
        Command::Ps => cmd_ps(&client, &base).await,
        Command::Env { assignment, scope } => {
            println!("[>] oxid env set {assignment} --scope {scope} (not implemented yet)");
            Ok(())
        }
        Command::Logs { branch, follow } => {
            let follow = if follow { " -f" } else { "" };
            println!("[>] oxid logs {branch}{follow} (not implemented yet)");
            Ok(())
        }
    };

    if let Err(message) = result {
        eprintln!("[!] {message}");
        std::process::exit(1);
    }
}

async fn cmd_up(client: &Client, base: &str, branch: &str) -> Result<(), String> {
    let repo_dir = std::env::current_dir().map_err(|e| format!("cannot resolve cwd: {e}"))?;

    println!("[>] oxid up {branch} ({})", repo_dir.display());
    let response = client
        .post(format!("{base}/api/v1/projects"))
        .json(&serde_json::json!({ "repo_dir": repo_dir.display().to_string() }))
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
    let project_id = project["id"]
        .as_u64()
        .ok_or_else(|| "daemon response missing project id".to_owned())?;
    println!(
        "[+] Project `{}` registered (id {project_id})",
        project["name"]
    );

    let response = client
        .post(format!("{base}/api/v1/projects/{project_id}/deploy"))
        .json(&serde_json::json!({ "branch": branch }))
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
    println!("[+] Environment live at: {url}");
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
        println!("[~] No projects registered yet.");
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
}
