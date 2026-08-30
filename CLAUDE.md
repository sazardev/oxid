# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Oxid is a self-hosted control plane for ephemeral, branch-based preview environments ("the Vercel of local servers"). It detects a git push, builds an image, spins up a container, routes it, and scales it to zero (pause) after inactivity, waking it on the next request. Written entirely in Rust, `unsafe` forbidden workspace-wide.

The product vision, architecture spec, and visual design language live in `IDEA.md`, `SPEC.md`, and `DESIGN.md` respectively — read those for background/rationale, not just this file. `ROADMAP.md` tracks a granular gap analysis between what those documents promise and what's actually implemented; check it before assuming a feature described in SPEC/IDEA/DESIGN exists in code. `AGENTS.md` is a denser, more exhaustive agent-oriented reference (exact line numbers, hook/CI internals, gotchas) — consult it when this file isn't specific enough.

## Commands

```bash
cargo build                          # build workspace (default member: oxid-cli)
cargo build --workspace              # build all crates (cli, core, daemon)
cargo build --workspace --all-targets
cargo test --workspace               # run all tests
cargo test -p oxid-daemon <name>     # single test by substring (also -p oxid-core / oxid-cli)
cargo test -p oxid-core -- --nocapture
cargo clippy --workspace --all-targets            # local: all+pedantic = warn
cargo clippy --workspace --all-targets -- -D warnings   # CI/pre-push gate: deny
cargo fmt                            # format
cargo fmt --all -- --check           # pre-commit gate
```

Gate order that matters, mirrored between `.githooks/pre-push` and CI (`ci.yml` `check` job): `fmt --check` → `clippy -D warnings` → `test --workspace`. A `clippy::pedantic` warning that's silent locally (warn) will still fail push/CI (deny with `-D warnings`).

Toolchain is pinned in `rust-toolchain.toml` (`stable` + `clippy` + `rustfmt`); no other setup needed. `build.rs` (`crates/oxid-core/build.rs`) wires `.githooks/` as `core.hooksPath` automatically on the first `cargo build/test/check`, but only if hooksPath isn't already customized.

Daemon configuration is via environment variables (see `crates/oxid-daemon/src/main.rs`):
- `OXID_DATA_DIR` (default `/data`) — holds `audit.sqlite` (WAL), `git-cache/`, `secret.key` (0600, AES-GCM)
- `OXID_ADDR` (default `0.0.0.0:8080`)
- `OXID_MASTER_KEY` — 64-hex-char AES-GCM key for secret encryption; auto-generated and persisted to `secret.key` if unset
- `OXID_WEBHOOK_SECRET` — HMAC-SHA256 secret verifying GitHub/Gitea/Gogs push webhooks (+ token echo for GitLab); webhooks are rejected while unset. Routes: `/api/v1/webhooks/{github,gitlab,gitea,gogs}`. A push is matched to a project by exact repository path (`repo_matches`), never by substring, and answered `202 queued`; the pushing user is recorded as the audit `operator`.
- `OXID_API_TOKEN` — bearer auth; the daemon refuses to start on a non-loopback bind without it (override with `OXID_ALLOW_OPEN_API=1`). Named, project-scopable tokens via `oxid token create [--project id]`; scoped tokens get 404 outside their project and 403 on node-wide routes (`api/middleware.rs::authorize_project`)
- `OXID_DEPLOY_CONCURRENCY` (default 4) — how many queued deploys one drain pass runs at once. A build is mostly waiting on Docker, so overlapping them costs little and is what keeps a burst of pushes from finishing one after another
- `OXID_GC_INTERVAL_SECS` (default 30) — scheduler tick for scale-to-zero GC
- `OXID_RATE_LIMIT_PER_SECOND` + `OXID_RATE_LIMIT_BURST` (both required together) — per-client-IP bucket on protected routes
- `OXID_BACKUP_INTERVAL_SECS` (+ `OXID_BACKUP_KEEP`, default 7) — periodic `VACUUM INTO` snapshots to `{data}/backups/`, off by default
- `OXID_DB_MAX_CONNECTIONS` (default 8) — SQLite pool size. WAL lets readers and the writer proceed in parallel, so the read-heavy hot paths (the `forwardAuth` heartbeat on every request, dashboard polling, `oxid status`) overlap instead of queueing. Writes still serialize — that is SQLite — so `busy_timeout`/`acquire_timeout` are set explicitly
- `OXID_TRAEFIK_HTTP_PORT` (default 80) — host port `oxid infra setup` publishes the built-in Traefik on. Traefik always listens on 80 *inside* its container; only the host side is configurable, so a machine whose 80 is already taken can still bootstrap
- `OXID_DOCKER_NETWORK` / `OXID_DEFAULT_MEMORY_LIMIT_MB` etc. — see `main.rs`

CLI targets the daemon via `OXID_API` (default `http://127.0.0.1:8080`) and `OXID_API_TOKEN`. Docker is required for the daemon (build/run/pause) but not for `cargo test` — `oxid-core` tests are pure and instant; `oxid-daemon` integration tests use in-memory SQLite unless marked `#[ignore]` (Docker-dependent).

Migrations are plain SQL files under `crates/oxid-daemon/migrations/`, run at startup via `sqlx`.

## Architecture

Hexagonal / ports-and-adapters, split across three crates:

- **`oxid-core`** — pure domain logic, zero I/O. Entities (`Project`, `Branch`, `Environment`, `ResourcePool`, `SecretContext`), state machine (`EnvironmentState`: Building/Running/Paused/Hibernating/Destroyed/BuildFailed), and the port *traits* other crates implement (`ProjectStore`, `EnvironmentStore`, `AuditStore`, `SecretStore`, `GitPort`, `ContainerPort` — all in `domain/ports.rs`). Domain services (`domain/services/`) hold business rules like GC eligibility (`gc.rs`), subdomain derivation (`subdomain.rs`), and the env-var inheritance resolver (`var_resolution.rs`: `Global -> Project -> Branch -> Runtime`). Every port trait is declared with `#[trait_variant::make(Send)]` so adapters implement a `Send`-safe async variant usable from `axum`/`tokio`.
- **`oxid-daemon`** — the binary (`oxidd`) hosting adapters and the HTTP API:
  - `adapter/store.rs` — SQLite (`sqlx`) implementing all persistence ports; secrets are AES-GCM encrypted at rest via `adapter/crypto.rs`.
  - `adapter/git.rs` — `git2`-based cached clones and detached-head checkouts (optional per-project token for private repos).
  - `adapter/oci.rs` — Docker orchestration via `bollard` (build/run/pause/unpause/stop/remove/logs/exec) plus network/Traefik bootstrap.
  - `adapter/config.rs` — parses per-project `oxid.toml`; `compose.rs` detects `docker-compose.yml`; `postgres_pool.rs` implements shared-Postgres per-branch databases.
  - `service/control_plane/` — the `ControlPlane` application service, split SRP (one module per concern): `deploy.rs`, `provision.rs`, `lifecycle.rs`, `gc.rs`, `infra.rs`, `admission.rs`, `auth.rs`, `project.rs`.
  - `service/proxy.rs` — built-in per-branch TCP reverse proxy (stable `public_port`, zero-downtime redeploys); `service/scheduler.rs` — periodic tokio task driving scale-to-zero GC + deploy-queue retry.
  - `api/` — `axum` HTTP surface: router in `mod.rs`, one handler file per resource under `handlers/`, `middleware.rs` (auth + rate-limit + request-id), `dashboard.rs` (embedded SPA).
- **`oxid-cli`** — thin `clap`-based HTTP client (binary `oxid`) that talks to the daemon's REST API; holds no business logic itself (multi-context config in `cli/config.rs`).

Data flow for a deploy: webhook/CLI request → `api/handlers/*` → `ControlPlane::deploy` → `GitPort` (clone/checkout) → `SecretStore` + `var_resolution` (compute injected env) → `ContainerPort` (build + run + exec `on_start`) → `EnvironmentStore`/`AuditStore` (persist state + audit trail).

Four invariants in that pipeline are load-bearing and easy to undo by accident:

- **Webhooks are accepted, not served.** `handle_push` answers `202 queued` and the deploy runs on the persisted queue (`enqueue_push` → `retry_queued_deploys`, single-flighted by `deploy_drain_lock`). Providers abandon a delivery in seconds — GitHub at 10, with no retry for push events — while a real first build takes far longer. Never move the deploy back inside the request.
- **Scale-to-zero stops containers, it does not pause them.** Traefik's Docker provider only publishes routers for `running` containers and ignores pause/unpause events, so a `docker pause`d environment loses its route permanently and 404s instead of waking. `ControlPlane::pause` and the GC both use `stop`; waking dispatches on `container_status`, never on the stored state.
- **Wake-on-request needs the daemon's catch-all router.** A stopped environment has no router of its own, so the lowest-priority `oxid-wake-catchall` router on the daemon's container is what catches the request and rewrites it to `/api/v1/wake`. It ships in `docker-compose.yml` and `oxid infra status` reports it missing.
- **The environment row is created before the image build.** It is what gives a failure somewhere to be recorded; every failure path funnels through `record_deploy_failure` so a broken Dockerfile leaves an `EnvironmentState::BuildFailed` row, an audit event and an ERROR line instead of nothing at all. `BuildFailed` is a real state, distinct from `Destroyed`: it means "someone's push is broken", it can only transition onward to `Destroy`/`TtlExpired`, and it is deliberately excluded from `find_environment_by_branch`'s notion of *live* so a failure never hides the instance still serving the branch.
- **Admission is decided once, after the checkout.** `check_admission` runs inside `deploy_at` with the branch's own effective config, because that is the first point the real memory request is known. `AdmissionMode` says what to do when it doesn't fit — enqueue, report (the queue drain already holds the entry), or bypass (rollback). Deciding earlier means weighing a number the deploy won't use.

Per-deploy config comes from the commit: `branch_config` re-reads `oxid.toml` from the checkout for `[build]`, `[routing].port` and `[dependencies]`. `base_domain` and the idle/lifetime policy stay with the project, because those are operator decisions owned by `oxid configure`. Containers are injected `OXID_BRANCH`, `OXID_ENV_URL` and `OXID_COMMIT`.

Redeploys are zero-downtime: the new container is built and started before traffic cuts over through the reverse proxy, so a broken push never takes the previous build down.

When adding a capability, prefer: domain rules and new port methods in `oxid-core`, adapter implementation in `oxid-daemon/src/adapter/*`, orchestration in `oxid-daemon/src/service/control_plane/`, and HTTP/CLI exposure last. Keep `oxid-core` free of any I/O, SQL, Docker, or HTTP dependency — `crates/oxid-core/Cargo.toml` must never gain `tokio|sqlx|bollard|axum|reqwest|git2|hyper|tower|tar`; this is enforced both by `.githooks/_lib.sh:check_hexagonal_boundary` and in CI (`ci.yml`).

The project (`oxid.toml`) config schema and its `[project]`/`[build]`/`[routing]`/`[dependencies]` sections are specified in `IDEA.md`; `crates/oxid-core/src/domain/project_config.rs` is the domain-side model for it.

## Database

One SQLite file, WAL, opened as a pool (`OXID_DB_MAX_CONNECTIONS`, default 8). Three things are load-bearing and easy to undo:

- **`open_in_memory` keeps exactly one connection.** Every connection to `:memory:` gets its own empty database, so a pool there hands each caller a different, migration-less copy — tests fail in ways that look like data loss.
- **`rotate_master_key` uses `BEGIN IMMEDIATE`.** Exclusion from concurrent secret writes used to come free from a single-connection pool; a deferred transaction takes the write lock only at its first write, leaving a window where a secret written under the *old* key would survive the swap and become undecryptable.
- **`touch_by_url` coalesces.** Traefik calls the heartbeat on every request to every environment and it is deliberately unauthenticated, so a write per call is both waste and an amplifier. The timestamp only feeds idle detection, whose threshold is minutes.

Measured on 12k environments / 60k audit events: heartbeat throughput went from flat at ~180 req/s (1→64 concurrent, p50 6ms→321ms) to 948–5108 req/s with p50 under 12ms. `environments(url)` is indexed — it is the column the busiest query in the system filters on.

## Internationalisation

Spanish and English, in three places that each resolve the language their own way:

- **Dashboard** — `crates/oxid-daemon/web/i18n.js` holds the catalog; `t()` on the Alpine component reads the active locale on every call, so the switcher re-renders every binding without a reload. Language: previous choice (`localStorage`), else `navigator.languages`, else English.
- **CLI** — `crates/oxid-cli/src/i18n.rs`. Language: `--lang`, else `OXID_LANG`, else `LC_ALL`/`LC_MESSAGES`/`LANG`, else English.
- **Daemon** — `crates/oxid-daemon/src/i18n.rs`, for messages the API returns to a person. The locale comes from `Accept-Language` and travels as a `tokio::task_local!` set in `request_id_middleware`, exactly like the request id, so a message built deep in `ControlPlane` needs no extra parameter.

Deliberately **not** translated, and each for a reason worth keeping: `--json` output and API field names (scripts parse them); log lines (aggregators match on their text); and anything wrapping a `git2`/`bollard`/`sqlx` error (those strings come from those libraries and are what an operator searches for).

Every catalog is guarded by tests that fail on a missing key or a placeholder a translation dropped or invented — `every_dashboard_string_exists_in_every_language` also checks the reverse, that no key the UI asks for is undefined.

## Hooks & CI

- `.githooks/pre-commit` (fast): `fmt --check`, merge-marker check, forbidden paths (`.env`/`secret.key`/`*.pem`), staged secret scan (`gitleaks` if installed, else built-in), `cargo check` if Rust changed, hexagonal-boundary check if `oxid-core/Cargo.toml` is staged.
- `.githooks/pre-push` (thorough): everything above plus `clippy -D warnings`, `test --workspace`, `cargo audit`, `cargo deny check`, full-history `gitleaks`. Install `cargo-audit`, `cargo-deny`, `gitleaks` locally for full coverage — otherwise those steps warn/skip locally, but CI still enforces them.
- Hooks are bypassable with `--no-verify`; CI (`ci.yml`) is not, and additionally runs `cargo build --workspace --all-targets`.
- `deny.toml` allows licenses `0BSD/MIT/Apache-2.0/...` and bans wildcard deps; `.cargo/audit.toml` requires a comment on any ignored advisory.
