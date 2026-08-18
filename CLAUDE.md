# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Oxid is a self-hosted control plane for ephemeral, branch-based preview environments ("the Vercel of local servers"). It detects a git push, builds an image, spins up a container, routes it, and scales it to zero (pause) after inactivity, waking it on the next request. Written entirely in Rust.

The product vision, architecture spec, and visual design language live in `IDEA.md`, `SPEC.md`, and `DESIGN.md` respectively — read those for background/rationale, not just this file. `ROADMAP.md` tracks a granular gap analysis between what those documents promise and what's actually implemented; check it before assuming a feature described in SPEC/IDEA/DESIGN exists in code.

## Commands

```bash
cargo build                          # build workspace (default member: oxid-cli)
cargo build --workspace              # build all crates (cli, core, daemon)
cargo test --workspace               # run all tests
cargo test -p oxid-daemon <name>     # run a single test by name substring
cargo clippy --workspace --all-targets   # lint (workspace enforces clippy::all + pedantic as warnings, unsafe_code forbidden)
cargo fmt                            # format

# Run the daemon (control plane API + scheduler)
cargo run -p oxid-daemon
# Run the CLI (thin HTTP client against the daemon)
cargo run -p oxid-cli -- <subcommand>
```

Daemon configuration is via environment variables (see `crates/oxid-daemon/src/main.rs`):
- `OXID_DATA_DIR` (default `/data`) — holds `audit.sqlite`, `git-cache/`, `secret.key`
- `OXID_ADDR` (default `0.0.0.0:8080`)
- `OXID_MASTER_KEY` — 64-hex-char AES-GCM key for secret encryption; auto-generated and persisted to `secret.key` if unset
- `OXID_WEBHOOK_SECRET` — HMAC-SHA256 secret for verifying GitHub push webhooks; webhooks are rejected while unset
- `OXID_GC_INTERVAL_SECS` (default 30) — scheduler tick for scale-to-zero GC

CLI targets the daemon via `OXID_API` (default `http://127.0.0.1:8080`).

Migrations are plain SQL files under `crates/oxid-daemon/migrations/`, run at startup via `sqlx`.

## Architecture

Hexagonal / ports-and-adapters, split across three crates:

- **`oxid-core`** — pure domain logic, zero I/O. Entities (`Project`, `Branch`, `Environment`, `ResourcePool`, `SecretContext`), state machine (`EnvironmentState`: Building/Running/Paused/Hibernating/Destroyed), and the port *traits* other crates implement (`ProjectStore`, `EnvironmentStore`, `AuditStore`, `SecretStore`, `GitPort`, `ContainerPort` — all in `domain/ports.rs`). Domain services (`domain/services/`) hold business rules like GC eligibility (`gc.rs`), subdomain derivation (`subdomain.rs`), and the env-var inheritance resolver (`var_resolution.rs`: `Global -> Project -> Branch -> Runtime`). Every port trait is declared with `#[trait_variant::make(Send)]` so adapters implement a `Send`-safe async variant usable from `axum`/`tokio`.
- **`oxid-daemon`** — the binary (`oxidd`) hosting adapters and the HTTP API:
  - `adapter/store.rs` — SQLite (`sqlx`) implementing all persistence ports; secrets are AES-GCM encrypted at rest via `adapter/crypto.rs`.
  - `adapter/git.rs` — `git2`-based cached clones and detached-head checkouts.
  - `adapter/oci.rs` — Docker orchestration via `bollard` (build/run/pause/unpause/stop/remove/logs/exec).
  - `adapter/config.rs` — parses per-project `oxid.toml`.
  - `service/control_plane.rs` — the `ControlPlane` application service: wires the ports together to implement `deploy()` (resolve secrets, build, run, inject env, run `on_start` hooks) and GC.
  - `service/scheduler.rs` — periodic tokio task driving scale-to-zero GC.
  - `api.rs` — `axum` HTTP surface: webhook ingestion (HMAC-verified) and the REST API the CLI/other clients call.
- **`oxid-cli`** — thin `clap`-based HTTP client (binary `oxid`) that talks to the daemon's REST API; holds no business logic itself.

Data flow for a deploy: webhook/CLI request → `api.rs` → `ControlPlane::deploy` → `GitPort` (clone/checkout) → `SecretStore` + `var_resolution` (compute injected env) → `ContainerPort` (build + run + exec `on_start`) → `EnvironmentStore`/`AuditStore` (persist state + audit trail).

When adding a capability, prefer: domain rules and new port methods in `oxid-core`, adapter implementation in `oxid-daemon/src/adapter/*`, orchestration in `service/control_plane.rs`, and HTTP/CLI exposure last. Keep `oxid-core` free of any I/O, SQL, Docker, or HTTP dependency.

The project (`oxid.toml`) config schema and its `[project]`/`[build]`/`[routing]`/`[dependencies]` sections are specified in `IDEA.md`; `crates/oxid-core/src/domain/project_config.rs` is the domain-side model for it.
