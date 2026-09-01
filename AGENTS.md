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
- Migrations: `crates/oxid-daemon/migrations/*.sql` (plain SQL, 20 files) — run at startup via `sqlx` (`crates/oxid-daemon/src/main.rs`).
- Release profile: `opt-level=3 lto=true codegen-units=1 strip=true panic=abort` (`Cargo.toml:53-58`) — `unsafe` forbidden workspace-wide (`Cargo.toml:40`).

## Architecture (hexagonal, SPEC.md §2)

- `oxid-core` — pure domain, zero I/O. Entities `Project/Branch/Environment/ResourcePool/SecretContext`, state machine `Building/Running/Paused/Hibernating/Destroyed/BuildFailed`, port traits in `domain/ports.rs` (all `#[trait_variant::make(Send)]`), services in `domain/services/` (`gc.rs`, `subdomain.rs`, `var_resolution.rs: Global→Project→Branch→Runtime`, `stack/` detection, `branch_filter.rs`, `access.rs`, `placement.rs`, `routing.rs`, `tls.rs`).
- `oxid-daemon` — binary `oxidd`: `adapter/store.rs` (SQLite+`sqlx`, secrets AES-GCM via `adapter/crypto.rs`), `adapter/git.rs` (`git2` cached clones, optional per-project token for private repos), `adapter/oci.rs` (`bollard` on `/var/run/docker.sock`, network/Traefik bootstrap), `adapter/config.rs` (`oxid.toml`) + `compose.rs` (docker-compose.yml detection) + `postgres_pool.rs` (shared-Postgres per-branch DBs); `service/control_plane/` (`ControlPlane` split SRP: deploy/provision/lifecycle/gc/infra/admission/auth/project/node), `service/fleet.rs` (`Fleet<O>`: the nodes this daemon holds a Docker client for), `service/proxy.rs` (built-in per-branch TCP reverse proxy — stable `public_port`, zero-downtime redeploys; dials a `Target { host, port }` so it also bridges to another node), `service/scheduler.rs` (tokio tick: node health probe → GC → deploy-queue retry), `api/` (`axum` router in `mod.rs`, one file per resource in `handlers/`, `middleware.rs` auth+rate-limit+request-id, `dashboard.rs` embedded SPA).
- `oxid-cli` — thin `clap` HTTP client, no business logic (multi-context config in `cli/config.rs`).

Flow: `webhook/CLI → api/ → ControlPlane::deploy → GitPort → SecretStore+var_resolution → ContainerPort(build/run/exec on_start) → EnvironmentStore/AuditStore`.

Rule: new capability → domain + port in `oxid-core`, adapter in `oxid-daemon/src/adapter/*`, orchestration in `service/control_plane/`, HTTP/CLI last. Never add `tokio|sqlx|bollard|axum|reqwest|git2|hyper|tower|tar` to `crates/oxid-core/Cargo.toml` — enforced by `.githooks/_lib.sh:check_hexagonal_boundary` (also `ci.yml:35`).

`oxid.toml` schema: `crates/oxid-core/src/domain/project_config.rs` / `IDEA.md` (`[project]/[build]/[routing]/[dependencies]`).

## Runtime

Daemon env (`crates/oxid-daemon/src/main.rs`):
- `OXID_DATA_DIR` default `/data` → `audit.sqlite` (WAL), `git-cache/`, `secret.key` (0600, AES-GCM, `OXID_MASTER_KEY` 64-hex or auto-generated)
- `OXID_ADDR` default `0.0.0.0:8080`, `OXID_GC_INTERVAL_SECS` default `30`, `OXID_DEPLOY_CONCURRENCY` default `4` (queued deploys per drain wave; see `service/control_plane/mod.rs::default_deploy_concurrency`). Per-project `git fetch` is coalesced in `service/refresh_coalescer.rs` — sharing is decided on when a caller *asked*, never on the age of the result, so it is coalescing rather than caching and has no staleness window
- `OXID_WEBHOOK_SECRET` — HMAC-SHA256 (GitHub/Gitea/Gogs) + token echo (GitLab); webhooks rejected if unset; routes `/api/v1/webhooks/{github,gitlab,gitea,gogs}`
- `OXID_API_TOKEN` — bearer auth; **daemon refuses to start on a non-loopback bind without it** (override `OXID_ALLOW_OPEN_API=1`). Named credentials (`oxid token create <name> [--project id] [--role R] [--expires-in 90d]`, migrations `0010`/`0017`) carry a role, a scope and an expiry; `oxid_core::services::access` holds the rules and `api/middleware.rs::authorize(&authed, Capability::X, project)` is the only way a route asks. Out-of-scope → 404 (no existence leak), everything else → 403 with the reason. A scoped credential is refused **any** action with no project id, whatever the capability — that is what stops it writing a *global* secret. `require_master` survives for exactly two things an `admin` must not have: `rotate-key` and `setup/webhook-secret`
- `OXID_BOOTSTRAP_TOKEN_ACCESS` (`loopback`/`any`/`off`) — who `GET /api/v1/setup/token` answers pre-auth. The shipped compose sets `off` **and** publishes `8080:8080`; the two go together, and re-enabling `any` with the port open hands out the master token
- `GET /api/v1/me` (behind `oxid whoami`) reports the caller's own grant; its capability list is derived from the role, never written out separately
- `OXID_DOCKER_NETWORK` / `OXID_DEFAULT_MEMORY_LIMIT_MB` etc. (see `main.rs`)
- `OXID_RATE_LIMIT_PER_SECOND` + `OXID_RATE_LIMIT_BURST` (both required) — per-client-IP bucket on protected routes
- `OXID_BACKUP_INTERVAL_SECS` (+ `OXID_BACKUP_KEEP`, default 7) — periodic `VACUUM INTO` snapshots to `{data}/backups/`, off by default
- `OXID_ALLOW_INSECURE_NODES=1` — lets a remote node register with no TLS material. Default refuses: a Docker socket over TCP is root on that box for anyone who can route to it
- `OXID_TRAEFIK_POLL_INTERVAL` default `5s` — Traefik's re-poll of `/api/v1/traefik/config`. Not the wake latency (a sleeping branch keeps its router), only the delay before a *new* branch is routable

CLI: `OXID_API` default `http://127.0.0.1:8080`, `OXID_API_TOKEN` bearer. `oxid login/logout/connect/server/whoami` wrap the context config (`cli/config.rs`); `login` reads the token from stdin and verifies it against `/api/v1/me` before saving, `logout` clears the credential but keeps the address. Run: `cargo run -p oxid-daemon` / `cargo run -p oxid-cli -- <subcommand>` (e.g. `ps`, `up`, `logs -f`). Docker required for daemon (build/run/pause), not for `cargo test` (pure `oxid-core` tests are instant; `oxid-daemon` integration tests use in-memory SQLite unless `#[ignore]` Docker tests).

## Hooks & CI

- `.githooks/pre-commit` (fast): `fmt --check`, merge markers, forbidden paths (`.env`/`secret.key`/`*.pem`), staged secret scan (`gitleaks` or built-in), `cargo check` if Rust changed, hexagonal boundary if `oxid-core/Cargo.toml` staged.
- `.githooks/pre-push` (thorough): + `clippy -D warnings` + `test --workspace` + hexagonal + `cargo audit` + `cargo deny check` + gitleaks history. Install `cargo-audit`, `cargo-deny`, `gitleaks` for full local coverage — otherwise warned/skipped, CI still enforces.
- `ci.yml` (authoritative): same as `pre-push` plus `cargo build --workspace --all-targets` and full-history `gitleaks-action`. `concurrency: cancel-in-progress: true`. Hooks bypassable with `--no-verify`; CI is not.

`deny.toml` (licenses `0BSD/MIT/Apache-2.0/...`, bans wildcards) and `.cargo/audit.toml` (ignored advisories must have comment).

## Docs — read before assuming

`IDEA.md` / `SPEC.md` / `DESIGN.md` are vision; `ROADMAP.md` is the granular gap code-vs-docs (50 tasks, `✅/Parcial/No existe`, wiring notes for Traefik `OXID_DOCKER_NETWORK` + `/api/v1/wake` + `/heartbeat`). Check it before building a feature they describe. `MEMORY.md` is working state, `CONTRIBUTING.md#guardrails` is hook/CI rationale. OpenCode agent squad: `.opencode/agents/*.md` (20 agents), catalog: `.opencode/AGENTS.md`.

## Multi-node (`MULTINODE.md`, all four stages delivered)

One control plane, N Docker endpoints — no agent. `ContainerPort` unchanged; `DockerClient::connect_to(&Node)` dispatches to `connect_with_defaults`/`connect_with_ssl`/`connect_with_http` (bollard's `ssl` feature already arrives via `buildkit`). Migration `0020` adds `nodes`, seeds node 1 (`local`) and backfills `environments.node_id` in the same file. `Environment::new` defaults to `NodeId::LOCAL`, which is what keeps every construction site compiling *and* meaning the right thing.

- `Fleet<O>` is `Arc<ArcSwap<HashMap<…>>>` — `Arc` **around** the swap, since `ControlPlane` derives `Clone` and axum clones per handler.
- `ControlPlane::new` keeps its signature and registers the given `oci` as node 1; `with_node_connector` supplies the constructor (`main.rs` passes `DockerClient::connect_to`) because building a `ContainerPort` is adapter knowledge.
- `local_oci()` = control-plane infrastructure (Traefik, network, ACME volume). `oci_for(env.node_id)?` = anything acting on an environment; it errors rather than falling back to local.
- `main.rs` calls `reload_fleet()` **before** `reconcile_startup_state()`.
- `place_deploy` replaced `check_admission`: with a fleet, "does it fit" and "where" are one decision. Capacity is read live per node (the `nodes` row is the probe's cache and admission cannot use a stale number). `LockKey::Admission(NodeId)`; `committed_memory_mb` carries `AND COALESCE(node_id,1) = ?`.
- `GET /api/v1/traefik/config` (authenticated, ETagged) serves routers from rows; services always point at `127.0.0.1:{public_port}`, never `node.address:host_port` — `host_port` changes per redeploy and the HTTP provider polls, which would reopen the gap migration `0007` closed.

## Gotchas

- **Docs are part of the change.** `docs/` is a hand-written GitHub Pages site (`docs/docs/*.html`, sidebar duplicated per page) that documents what `install.sh` prints, what the stack table detects, and what each role may do. Change any of those and the page is stale.
- **A node this daemon cannot reach never has its environments rewritten.** `reconcile_startup_state` resolves the node before deciding anything and pushes an unreachable one into `errors`; `oci.rs::container_status` answers `Missing` only on a real 404. Both halves are needed — the `Missing` branch marks rows `Destroyed`. Nothing (probe, drain, `node rm`) writes an environment row because of a node's state.
- **`entryPoints` in `routing.rs` carries `#[serde(rename)]`.** Traefik silently ignores keys it does not know, so `entry_points` produced routers that existed, answered nothing and logged nothing. Pinned by the serialisation test.
- **`delete_node` refuses even destroyed environments** — `audit_events` cascades from `environments`, so freeing a node would delete that branch's history as a side effect.
- **`oxid node drain --evacuate` sets `draining` first, then redeploys.** Placement refuses a draining node even to a branch already on it, and that is what makes each redeploy *leave*. It re-reads where a branch landed **by branch, not by environment id** — a redeploy creates a new row.
- **Two things must not be undone in `access.rs`:** an out-of-scope denial answers 404 (403 would confirm the project exists), and a scoped credential is refused any action with `project: None` — the version that also asked whether the capability *looked* project-local let a scoped maintainer write a global secret.
- **`[deploy]` lives on the project row (migration `0016`), not in the per-commit config**, because the branch filter has to answer before the checkout — reading it from the pushed commit would do the fetch the filter exists to avoid. Only webhooks are filtered; `oxid up <branch>` never is.
- **Image tags are lowercased in full** (`helpers.rs::image_name`). Docker refuses any other reference, so `JIRA-123` failed every deploy before that.
- `cargo test --workspace` also triggers `build.rs` hook wiring — first run sets `core.hooksPath` to `.githooks/` (local per-repo config only).
- `pre-push` and `ci.yml` use `-D warnings` — a `clippy::pedantic` warn that passes locally will still fail push/CI.
- Secrets never in logs/API/audit: encrypted at rest (AES-GCM), `secret.key` must be `0600`, `gitleaks` scans staged diff + push range + full history in CI.
- `release.yml` triggers on `push tags v*.*.*` (`v0.1.0`) + `workflow_dispatch` dry-run: 6 targets (linux gnu/musl x86_64/aarch64 cross, macOS x86_64/aarch64, windows) + `ghcr.io` Docker. See `CONTRIBUTING.md` before large PRs.
