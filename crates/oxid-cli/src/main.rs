//! Oxid command-line interface.
//!
//! Thin front-end over the domain. Command stubs for Phase 1; the actual
//! orchestration is wired up in later phases against `oxid-daemon`.

use clap::{Parser, Subcommand};

/// Ephemeral environments that breathe. Ferrous performance, invisible footprint.
#[derive(Debug, Parser)]
#[command(name = "oxid", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Force a deployment of a branch.
    Up {
        /// Repository or project name.
        repo: String,
        /// Branch to deploy.
        #[arg(long, default_value = "main")]
        branch: String,
    },
    /// Manage environment variables.
    Env {
        /// Variable to set, e.g. `DB_PASSWORD=secret`.
        assignment: String,
        /// Scope of the variable.
        #[arg(long, default_value = "global")]
        scope: String,
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

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Up { repo, branch } => {
            println!("[>] oxid up {repo} --branch {branch} (not implemented yet)");
        }
        Command::Env { assignment, scope } => {
            println!("[>] oxid env set {assignment} --scope {scope} (not implemented yet)");
        }
        Command::Logs { branch, follow } => {
            let follow = if follow { " -f" } else { "" };
            println!("[>] oxid logs {branch}{follow} (not implemented yet)");
        }
    }
}
