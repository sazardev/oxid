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
- `OXID_DEPLOY_CONCURRENCY` (default 4) — how many queued deploys one drain pass runs at once. A build is mostly waiting on Docker, so overlapping them costs little and is what keeps a burst of pushes from finishing one after another. Raising it only pays off now that the per-project `git fetch` is coalesced (`service/refresh_coalescer.rs`); before that, the serialized fetch masked it entirely. Measured on 15 simultaneous pushes: 7.1s at 4, 4.2s at 16
- `OXID_BOOTSTRAP_TOKEN_ACCESS` (`loopback` default / `any` / `off`) — who `GET /api/v1/setup/token` hands the auto-generated master token to, pre-auth. The daemon cannot tell a safe deployment from an exposed one — containerized, it is always bound to `0.0.0.0` and every caller arrives from the bridge gateway — so the operator decides and the default withholds. The shipped `docker-compose.yml` sets `any`, which is only correct because it publishes on `127.0.0.1`
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

## Stack detection

A repository with no `Dockerfile` used to be refused outright, which asked
every team wanting preview environments to become Docker authors first.
`oxid_core::services::stack` reads what a repository already says about
itself — `package.json`, `go.mod`, `pyproject.toml`, `Cargo.toml` — and
generates a Dockerfile for it. Measured on a Nest service: the generated
multi-stage image is 215MB against 1.63GB for the single-stage Dockerfile
someone writes by hand.

Load-bearing, in order of how easy each is to undo:

- **Detection runs last.** `adapter::config::parse_project` tries
  `oxid.toml`, then Compose, then a committed `Dockerfile`, and only then
  detects. A Dockerfile someone wrote is a decision, never second-guessed.
- **The generated file goes in the private build-context copy**, never the
  checkout — the developer's `git status` stays clean, and committing their
  own Dockerfile takes over with no state to clear.
- **`detect` returns `None` rather than guessing.** An unrecognised
  repository gets the same "write a Dockerfile" error it always did; a
  generated build that dies halfway through is worse than an honest refusal.
- **The wire name equals the display name.** A derived `rename_all` turns
  `NestJs` into `nest-js`, giving the panel one spelling and the logs
  another; every variant is renamed explicitly and a test pins it.
- **The domain is pure.** `detect` takes a `RepoManifest` (which paths
  exist, plus the contents of the few manifests it asks for), so every rule
  is testable without a filesystem, a network or Docker, and the adapter
  only reads files.

Verified by deploying one real repository per stack through Docker, not by
asserting on generated text — which is how three defects surfaced that the
unit tests could not see: `npm ci` refuses to run without a lockfile (and
plenty of repositories do not commit one), `go build -o app ./...` breaks on
any module with more than one `main`, and the Rust stage copied
`target/release/app` for a binary Cargo names after the package. Sizes of
the generated images: Go 24.5MB, SPA/static 94.5MB, Rust 136MB, FastAPI
209MB, Nest 215MB, Express 246MB.

The result is stored on the project (`detected_stack`, migration `0013`) and
shown as a tag in the dashboard and an `oxid ps` column. Null is the normal
case: the project answered for itself.

### Build speed

Every generated Dockerfile uses `RUN --mount=type=cache` for its
ecosystem's download cache — npm/pnpm/yarn/bun stores, `GOMODCACHE` and
`GOCACHE`, cargo's registry and `target`, pip's cache. BuildKit was already
requested by `adapter::oci` (`BuilderVersion::BuilderBuildKit`); the
generated files simply were not exploiting it. The layer cache dies the
moment a lockfile changes; a cache mount is not part of a layer and is
shared between branches of the same project. Measured on a one-line change
to an Axum service: **17s to 2s**.

Two things are easy to get wrong here:

- **Cargo's `target` is a cache mount, so `COPY --from=build /src/target/...`
  finds nothing** — the mount is not part of any layer. The binary is copied
  out inside the same `RUN`. It is also `sharing=locked`, because cargo takes
  its own lock on a target directory and concurrent branch builds would
  queue anyway.
- **`pip install --no-cache-dir` is the right advice without BuildKit and
  wrong with it**: the cache is no longer in a layer, so the flag only
  discards work between builds.

`Stack::base_images` names what a build will pull, and
`ControlPlane::prewarm_base_images` fetches them in a detached task at
registration — minutes to hours before the first push, so the first deploy
does not open with a download. Best-effort by contract: only images a
*detected* stack named are fetched (a project with its own Dockerfile could
be built on anything), and a failure is logged at debug since the build
pulls the image itself.

### Monorepos

`detect_monorepo` recognises pnpm workspaces, a `workspaces` array in the
root `package.json` (npm/yarn/bun) and `lerna.json`, reporting Turborepo or
Nx on top when present. Members with their own `package.json` are listed;
one counts as *deployable* if it has a recognised framework, depends on
something that listens (Express, Fastify, Hono, …) or declares a `start`
script — otherwise it is a library other packages import, and listing it
would send an operator to register something with nothing to serve.

Three things are load-bearing:

- **A workspace member builds from the repository root**, not its own
  directory. Its dependencies include siblings, and the lockfile resolving
  them is at the root; `deploy_at` switches the Docker context to `.` when
  `[build].context` names a member, and generates a Dockerfile that installs
  at the root filtered to that package.
- **The runtime stage carries the whole built tree.** Copying
  `node_modules` plus the one package looks tighter and produces an image
  that starts and dies on `MODULE_NOT_FOUND`: a workspace links siblings in
  as symlinks pointing back at `packages/*`. Found by deploying one.
- **Zero-config points at the first deployable service**, since a monorepo
  root usually builds nothing. The dashboard lists every service with the
  active one marked, so changing it is a `[build].context` edit rather than
  a guess.

Workspace globs are deliberately not parsed: "has a `package.json`" reaches
the same answer without a glob language, and `adapter::config::read_repo_manifest`
already bounds the walk to the root plus one level inside `apps/`,
`packages/`, `services/` and `libs/`.

## Dashboard

Embedded in the binary via `include_str!` — no build step, no bundler, and
no request that leaves the host: Alpine is vendored, there is no webfont and
no icon font, and the whole shell is ~232KB uncompressed (Alpine is 44KB of
it; the panel's own CSS/JS/HTML is ~120KB).

It is a PWA: `manifest.webmanifest`, `sw.js` and two SVG icons are served
from the same binary (`api/dashboard.rs`). The service worker caches the
shell so the panel opens with the daemon down, and **never caches `/api/`** —
a cached environment list is a lie about live cluster state, and the tests
in `api/tests.rs` assert that exclusion rather than trusting the comment.
Registration is best-effort: it needs a secure context, which a daemon on a
plain-HTTP LAN address does not have.

Layout is fluid from 320px to ultrawide and verified there, not assumed:
below 760px the tables become cards (each `<td>` carries `:data-label` with
its column header, so the labels have one source), the nav becomes a
thumb-scrollable strip rather than stacking, and `@media (pointer: coarse)`
raises controls to a 44px target — a checkbox needs a wrapping `label.check`
for that, since padding does not enlarge a replaced element.

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
