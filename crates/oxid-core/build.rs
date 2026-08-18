//! Auto-installs the repo's git hooks (`.githooks/`) on first build.
//!
//! This runs as part of every `cargo build`/`test`/`check` in the workspace
//! (every other crate depends on `oxid-core`), so anyone who clones the repo
//! and builds it gets the pre-commit/pre-push guardrails wired up without a
//! manual setup step. It only ever sets local repo config (`core.hooksPath`),
//! never global config, and only if unset — an intentional customization by
//! a contributor is left alone.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Walk up from this crate to find the repo root (has a `.git` dir).
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let mut dir = Path::new(&manifest_dir);
    let repo_root = loop {
        if dir.join(".git").exists() {
            break Some(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break None,
        }
    };

    let Some(repo_root) = repo_root else {
        // Not inside a git checkout (e.g. a packaged/vendored copy) — nothing to do.
        return;
    };

    if !repo_root.join(".githooks").exists() {
        return;
    }

    let current = Command::new("git")
        .args(["config", "--local", "--get", "core.hooksPath"])
        .current_dir(&repo_root)
        .output();

    let already_set = matches!(&current, Ok(out) if out.status.success() && !out.stdout.is_empty());
    if already_set {
        return;
    }

    let result = Command::new("git")
        .args(["config", "--local", "core.hooksPath", ".githooks"])
        .current_dir(&repo_root)
        .status();

    match result {
        Ok(status) if status.success() => {
            println!(
                "cargo:warning=oxid: wired up git hooks (core.hooksPath=.githooks) — see CONTRIBUTING.md#guardrails"
            );
        }
        _ => {
            // Non-fatal: missing `git` binary, read-only checkout, etc. Build must not break over this.
        }
    }
}
