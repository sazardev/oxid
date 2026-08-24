# AGENTS.md

Compact repo guide — every line is something an agent would miss without it.

## Build / Test / Lint

```bash
cargo build --workspace              # all crates; default member is oxid-cli (Cargo.toml:4)
cargo build --workspace --all-targets
cargo test --workspace
cargo test -p oxid-daemon <name>     # single test by substring (also -p oxid-core / oxid-cli)
cargo test -p oxid-core -- --nocapture
cargo clippy --workspace --all-targets          # local: all+pedantic = warn (Cargo.toml:42-44)
cargo clippy --workspace --all-targets -- -D warnings  # CI/pre-push gate: deny (ci.yml:41, .githooks/pre-push)
cargo fmt --check                    # pre-commit gate; fix with cargo fmt
cargo fmt --all -- --check
```

Order that matters: `fmt --check` → `clippy -D warnings` → `test` (ci.yml `check` job). `build.rs` wires `.githooks/` as `core.hooksPath` on first `cargo build/test/check` only if not already customized (`crates/oxid-core/build.rs`).

Toolchain pinned in `rust-toolchain.toml` — `stable` + `clippy` + `rustfmt`. No other setup.

## Workspace

- `resolver = "3"`, `edition = "2024"`, 3 members: `oxid-core`, `oxid-daemon`, `oxid-cli` (`Cargo.toml:1-4`).
- Entrypoints: `crates/oxid-core/src/lib.rs` (pure domain), `crates/oxid-daemon/src/main.rs` (binary `oxidd`), `crates/oxid-cli/src/main.rs` (binary `oxid`).
- Migrations: `crates/oxid-daemon/migrations/*.sql` (plain SQL, 9 files) — run at startup via `sqlx` (`crates/oxid-daemon/src/main.rs`).
- Release profile: `opt-level=3 lto=true codegen-units=1 strip=true panic=abort` (`Cargo.toml:53-58`) — `unsafe` forbidden workspace-wide (`Cargo.toml:40`).

## Architecture (hexagonal, SPEC.md §2)

- `oxid-core` — pure domain, zero I/O. Entities `Project/Branch/Environment/ResourcePool/SecretContext`, state machine `Building/Running/Paused/Hibernating/Destroyed/BuildFailed`, port traits in `domain/ports.rs` (all `#[trait_variant::make(Send)]`), services in `domain/services/` (`gc.rs`, `subdomain.rs`, `var_resolution.rs: Global→Project→Branch→Runtime`).
- `oxid-daemon` — binary `oxidd`: `adapter/store.rs` (SQLite+`sqlx`, secrets AES-GCM via `adapter/crypto.rs`), `adapter/git.rs` (`git2` cached clones, optional per-project token for private repos), `adapter/oci.rs` (`bollard` on `/var/run/docker.sock`, network/Traefik bootstrap), `adapter/config.rs` (`oxid.toml`) + `compose.rs` (docker-compose.yml detection) + `postgres_pool.rs` (shared-Postgres per-branch DBs); `service/control_plane/` (`ControlPlane` split SRP: deploy/provision/lifecycle/gc/infra/admission/auth/project), `service/proxy.rs` (built-in per-branch TCP reverse proxy — stable `public_port`, zero-downtime redeploys), `service/scheduler.rs` (tokio GC tick + deploy-queue retry), `api/` (`axum` router in `mod.rs`, one file per resource in `handlers/`, `middleware.rs` auth+rate-limit+request-id, `dashboard.rs` embedded SPA).
- `oxid-cli` — thin `clap` HTTP client, no business logic (multi-context config in `cli/config.rs`).

Flow: `webhook/CLI → api/ → ControlPlane::deploy → GitPort → SecretStore+var_resolution → ContainerPort(build/run/exec on_start) → EnvironmentStore/AuditStore`.

Rule: new capability → domain + port in `oxid-core`, adapter in `oxid-daemon/src/adapter/*`, orchestration in `service/control_plane/`, HTTP/CLI last. Never add `tokio|sqlx|bollard|axum|reqwest|git2|hyper|tower|tar` to `crates/oxid-core/Cargo.toml` — enforced by `.githooks/_lib.sh:check_hexagonal_boundary` (also `ci.yml:35`).

`oxid.toml` schema: `crates/oxid-core/src/domain/project_config.rs` / `IDEA.md` (`[project]/[build]/[routing]/[dependencies]`).

## Runtime

Daemon env (`crates/oxid-daemon/src/main.rs`):
- `OXID_DATA_DIR` default `/data` → `audit.sqlite` (WAL), `git-cache/`, `secret.key` (0600, AES-GCM, `OXID_MASTER_KEY` 64-hex or auto-generated)
- `OXID_ADDR` default `0.0.0.0:8080`, `OXID_GC_INTERVAL_SECS` default `30`
- `OXID_WEBHOOK_SECRET` — HMAC-SHA256 (GitHub/Gitea/Gogs) + token echo (GitLab); webhooks rejected if unset; routes `/api/v1/webhooks/{github,gitlab,gitea,gogs}`
- `OXID_API_TOKEN` — bearer auth; **daemon refuses to start on a non-loopback bind without it** (override `OXID_ALLOW_OPEN_API=1`); named tokens (`oxid token create [--project id]...`, migration `0010`) are project-scopable — scoped tokens get 404 outside their projects and 403 on node-wide routes (see `api/middleware.rs::authorize_project`)
- `OXID_DOCKER_NETWORK` / `OXID_DEFAULT_MEMORY_LIMIT_MB` etc. (see `main.rs`)
- `OXID_RATE_LIMIT_PER_SECOND` + `OXID_RATE_LIMIT_BURST` (both required) — per-client-IP bucket on protected routes
- `OXID_BACKUP_INTERVAL_SECS` (+ `OXID_BACKUP_KEEP`, default 7) — periodic `VACUUM INTO` snapshots to `{data}/backups/`, off by default

CLI: `OXID_API` default `http://127.0.0.1:8080`, `OXID_API_TOKEN` bearer. Run: `cargo run -p oxid-daemon` / `cargo run -p oxid-cli -- <subcommand>` (e.g. `ps`, `up`, `logs -f`). Docker required for daemon (build/run/pause), not for `cargo test` (pure `oxid-core` tests are instant; `oxid-daemon` integration tests use in-memory SQLite unless `#[ignore]` Docker tests).

## Hooks & CI

- `.githooks/pre-commit` (fast): `fmt --check`, merge markers, forbidden paths (`.env`/`secret.key`/`*.pem`), staged secret scan (`gitleaks` or built-in), `cargo check` if Rust changed, hexagonal boundary if `oxid-core/Cargo.toml` staged.
- `.githooks/pre-push` (thorough): + `clippy -D warnings` + `test --workspace` + hexagonal + `cargo audit` + `cargo deny check` + gitleaks history. Install `cargo-audit`, `cargo-deny`, `gitleaks` for full local coverage — otherwise warned/skipped, CI still enforces.
- `ci.yml` (authoritative): same as `pre-push` plus `cargo build --workspace --all-targets` and full-history `gitleaks-action`. `concurrency: cancel-in-progress: true`. Hooks bypassable with `--no-verify`; CI is not.

`deny.toml` (licenses `0BSD/MIT/Apache-2.0/...`, bans wildcards) and `.cargo/audit.toml` (ignored advisories must have comment).

## Docs — read before assuming

`IDEA.md` / `SPEC.md` / `DESIGN.md` are vision; `ROADMAP.md` is the granular gap code-vs-docs (50 tasks, `✅/Parcial/No existe`, wiring notes for Traefik `OXID_DOCKER_NETWORK` + `/api/v1/wake` + `/heartbeat`). Check it before building a feature they describe. `MEMORY.md` is working state, `CONTRIBUTING.md#guardrails` is hook/CI rationale. OpenCode agent squad: `.opencode/agents/*.md` (20 agents), catalog: `.opencode/AGENTS.md`.

## Gotchas

- `cargo test --workspace` also triggers `build.rs` hook wiring — first run sets `core.hooksPath` to `.githooks/` (local per-repo config only).
- `pre-push` and `ci.yml` use `-D warnings` — a `clippy::pedantic` warn that passes locally will still fail push/CI.
- Secrets never in logs/API/audit: encrypted at rest (AES-GCM), `secret.key` must be `0600`, `gitleaks` scans staged diff + push range + full history in CI.
- `release.yml` triggers on `push tags v*.*.*` (`v0.1.0`) + `workflow_dispatch` dry-run: 6 targets (linux gnu/musl x86_64/aarch64 cross, macOS x86_64/aarch64, windows) + `ghcr.io` Docker. See `CONTRIBUTING.md` before large PRs.
