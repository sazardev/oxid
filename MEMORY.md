# MEMORY.md — working state as of 2026-08-21

Quick-glance status for whoever (human or Claude) picks this repo back up.
For the granular, tracked-item gap analysis see `ROADMAP.md` — this file is
just "where did we leave off and why."

## What's solid and live-verified

- **One-command DevOps setup (this round), every path tested against the
  real published release:** `install.sh` — binaries-only (release download +
  sha256 verify + PATH install), `--server` (systemd unit + auto-generated
  0600 secrets that re-runs never rotate + health wait + Traefik bootstrap;
  `--root` sandbox mode for testing without touching the host) and
  `--docker` (compose stack from the public ghcr image, `.env` generated,
  image pinned to the release tag, infra verified). Found and fixed live:
  checksum asset name (`.sha256` not `.tar.gz.sha256`), env-var clobbering
  (`BINDIR=""` reset the env), non-root idempotency (grep on a 0600 root
  file silently failed → would have rotated secrets every re-run), raw
  CDN staleness (installer now handles both compose shapes + verifies the
  pin), double-Traefik (compose container now named `oxid-traefik` so
  bootstrap detects it), and **Traefik ≤v3.5 vs Docker ≥29** — the vendored
  docker client negotiates API 1.24, the engine's floor is 1.40,
  `DOCKER_API_VERSION` is ignored, every router silently 404s; compose now
  pins `traefik:latest` (verified routing end-to-end after the bump).
  Validated the full production topology from an absolutely fresh
  `curl | sh`: register via the new `./repos` mount → deploy →
  `Host: main.e2e-go.local.dev` routed through Traefik → container, 200.
- **E2E validation scenario (previous round), still running as a
  playground:** public repo `sazardev/oxid-e2e-go` (5 Go branches, distinct
  features/deps-in-code), lab daemon v0.1.0 musl on 127.0.0.1:18080
  (direct-publish) with shared Postgres/Redis, 5 concurrent devs simulated
  over 4 rounds (CLI + real GitHub webhooks through a cloudflared tunnel),
  zero-downtime proven with request pollers (732/0 and 482/0 during live
  cutovers), per-branch DB/index isolation, dashboard driven via CDP
  (pause/wake from the UI with the port actually dying/coming back),
  rollback with both versions served mid-cutover. Product findings logged
  in the report: oxid.toml frozen at registration, read-only CLI commands
  auto-register cwd, rm-project lacks --project, REDIS_URL index-as-path +
  no sslmode hint undocumented, global deploy lock, containerized daemons
  need the ./repos mount to register (no remote-registration API yet), and
  resource leases are per-daemon-DB (two daemons on one shared
  Postgres/Redis will collide — one daemon per shared instance).
- **v0.1.0 shipped (2026-08-24).** Tag `v0.1.0` → `release.yml` all green:
  6 platform binaries attached to the GitHub Release + `ghcr.io/sazardev/oxid`
  images (`0.1.0`, `0.1`, `latest`). Smoke-tested the released musl binary
  live: the auth gate refuses to start on `0.0.0.0` without
  `OXID_API_TOKEN` (prints the three fixes), the direct-publish topology
  warning prints at startup, and with token+loopback it serves while the
  released `oxid` CLI connects (`ps` → empty list). Found and fixed one real
  pipeline bug on the way: `taiki-e/upload-rust-binary-action` attaches to
  the tag's GitHub Release but never creates it — on a fresh tag every
  matrix job polled "release not found" through its timeout and died at
  upload despite building fine (the earlier `workflow_dispatch` dry-run
  never exercised this path because dry-run skips attaching). Fixed in
  `release.yml` with an idempotent ensure-release step (race-safe across
  parallel jobs); for v0.1.0 itself the release was created manually and
  the failed jobs rerun green.
- **Production-readiness round for the CLI-only v0.1.0 release (this
  round), all covered by tests:** decided the public release posture is
  CLI-first with Traefik mode as *the* supported production topology, then
  closed the three gates that stood between "works on my machine" and
  "shippable":
  1. **Open-API startup gate** — a daemon binding a non-loopback address
     (`OXID_ADDR` default `0.0.0.0:8080`!) now *refuses to start* without
     `OXID_API_TOKEN`, printing three actionable fixes; `OXID_ALLOW_OPEN_API=1`
     is the explicit opt-out and loopback binds stay open for local dev.
     `bind_is_loopback` resolves hostnames via `ToSocketAddrs` and fails
     closed (unresolvable/wildcard = public). Previously it only warned — an
     unauthenticated deploy/destroy/read-secrets API was one forgotten env
     var away.
  2. **Topology honesty** — direct-publish mode now warns at startup that
     scale-to-zero is disabled there (nothing refreshes `last_accessed_at`,
     so the sweep no-ops by design), and `oxid doctor` says the same; Traefik
     mode is documented as the supported path.
  3. **RBAC-lite** (equipo pequeño) — named tokens are project-scopable:
     migration `0010` (`api_tokens.scoped_projects`, NULL = unscoped),
     `oxid token create <name> --project <id>...`, empty scope list rejected,
     scopes normalized (sorted/deduped). Enforcement lives in
     `api/middleware.rs` (`AuthedAs::Operator(OperatorIdentity)`,
     `authorize_project` → **404 outside scope so existence never leaks**,
     `require_unscoped` → 403 on node-wide routes): project list filtered,
     per-project routes (deploy/rollback/environments/secrets/PATCH/DELETE)
     scoped, environment-addressed routes authorized through the owning
     project (`ControlPlane::environment_project_id`), global secrets /
     stats / infra / backup+restore / register-project locked to unscoped
     credentials (backup download previously reachable by *any* named token
     — closed), audit collapses to the scoped projects' merged trail, queue
     filtered per project. Token management stays master-only.
- Core deploy flow: `oxid.toml` explicit, `docker-compose.yml` detection,
  and bare-`Dockerfile` zero-config all work end-to-end (real Docker, real
  git repos, real HTTP responses checked).
- P3 resource pooling (shared Postgres/Redis, per-branch logical DB/index)
  verified live with two branches, real connectivity, and correct cleanup
  (`DROP DATABASE` / index release) on `oxid down`.
- Security/lifecycle gaps from the second audit round are closed: API
  bearer-token auth, `oxid rm-project`, Docker image cleanup on destroy,
  `--purge-secrets`, webhook event-type/branch-deletion handling, and the
  GC-vs-manual-action race (widened `lifecycle_lock`). All re-verified live
  post-merge with a real signed webhook, real `rm-project` cascade, and a
  real Traefik container (routing + heartbeat + `/api/v1/wake` all worked;
  auto-wake via a Traefik `errors` middleware still needs manual operator
  wiring, as already documented — not a bug).
- `oxid logs -f` streams live over SSE instead of polling — confirmed live
  against a container ticking once/second.
- **Production-hardening pass (this round), all live-verified:**
  container memory/CPU limits (`[build] memory_limit_mb`/`cpu_limit_millicores`
  + `OXID_DEFAULT_MEMORY_LIMIT_MB`/`OXID_DEFAULT_CPU_LIMIT_MILLICORES`
  daemon-wide fallback — the fix for the exact class of incident that
  crashed WSL this session, just triggered by user code instead); `oxid
  rollback <branch> [--to <sha>]`; `oxid audit`/`GET /api/v1/audit`
  (`AuditStore` was write-only before); global `--json` flag +
  differentiated exit codes (2 not-found, 3 unreachable, 4 auth, 1 generic);
  `oxid backup`/`oxid restore` (`VACUUM INTO` snapshot, restore is staged
  for the next restart, never a live hot-swap); direct TLS on the daemon
  (`OXID_TLS_CERT`/`OXID_TLS_KEY`, `axum-server`+rustls with the `ring`
  backend specifically, to avoid pulling in `aws-lc-sys`'s cmake build).
- **Lightweight multi-user + ops follow-up pass (this round), all
  live-verified:** database-backed named API tokens (`oxid token
  create/list/revoke`, master-token-only) — the bearer-auth middleware
  accepts the master `OXID_API_TOKEN` or any non-revoked named token,
  attributing `deploy`/`rollback`/`destroy` audit events to the operator's
  name instead of leaving everything anonymous; rate limiting on the
  protected routes only (`OXID_RATE_LIMIT_PER_SECOND`/`_BURST`, a single
  global bucket via `tower_governor` — deliberately not per-client, and
  never applied to `/health`/`/wake`/`/heartbeat`/webhooks); `oxid doctor`
  (reachability/latency/version/auth preflight, 3 distinct exit codes);
  hot master-key rotation (`oxid rotate-key` — `ArcSwap`-wrapped `Cipher`,
  transactional re-encryption of every secret, zero downtime, verified
  surviving a real daemon restart under the new key).
- **Catastrophe-resilience + resource-aware admission control pass (this
  round), all live-verified:** deployed containers carry Docker's
  `unless-stopped` restart policy (config correctness confirmed via `docker
  inspect`; a true crash simulation couldn't be cleanly triggered in this
  sandbox due to Docker/kernel signal-handling quirks, not a code issue);
  startup reconciliation (`ControlPlane::reconcile_startup_state`) diffs the
  database against Docker's actual container state before serving any
  request — live-verified with a real pause/external-unpause/restart cycle;
  graceful shutdown (SIGTERM/Ctrl+C drains in-flight requests) + best-effort
  daemon OOM-score de-prioritization; new resource admission control
  (`OXID_RESERVED_MEMORY_MB`, default 1024) — `docker info` capacity minus
  reserved minus every live environment's committed memory decides whether a
  new deploy proceeds immediately, gets queued (persisted in SQLite,
  `deploy_queue` table, survives a daemon restart), or is rejected outright
  via `CpError::InsufficientCapacity` if it could never fit alone; the GC
  scheduler retries the queue every tick, FIFO first — live-verified
  end-to-end: deployed two branches past a deliberately tight
  `OXID_RESERVED_MEMORY_MB`, confirmed `oxid queue`/`GET /api/v1/queue`
  showed the backlog, destroyed the live one, and watched the scheduler
  auto-promote the queued branch to `Running` within one GC tick with zero
  manual intervention. `POST /api/v1/projects/{id}/deploy` and the GitHub
  webhook both report `202 {"status":"queued","position":N}`; `oxid up`
  prints the queued position instead of a misleading "live at" line; new
  `oxid queue` command lists the backlog.
- **Embedded web dashboard (this round), live-verified in a real browser:**
  SPEC.md §5.3's dashboard, implemented with zero new dependencies — static
  `index.html`/`style.css`/`app.js` + vendored Alpine.js (54KB, no CDN)
  embedded via `include_str!` and served from `/` alongside the API. Shows
  live environments (deduplicated per branch like `oxid status`) with
  DESIGN.md's exact state visualization, pause/wake/destroy actions, the
  deploy queue, recent audit, and a live log viewer streamed over
  `fetch()`+`ReadableStream` (not `EventSource`, which can't send the
  `Authorization` header the API requires). New `GET /api/v1/stats`
  (`ControlPlane::node_stats`) backs the overview cards in one call. Found
  and fixed a real bug live: `x-for` needs `<template x-for>` wrapping in
  Alpine.js, not a bare element — without it every list silently threw and
  rendered empty. Re-verified after the fix: static assets, `/api/v1/stats`,
  wake/pause actions, real nginx log streaming, and the bearer-token 401
  banner → correct-token recovery flow all worked end-to-end.
- **Dashboard rebuilt as a real multi-page SPA (this round, live-verified),
  in response to explicit feedback that the first pass was read-mostly and
  modal-based instead of a real app:** client-side router
  (`/ui/environments`, `/ui/projects/:id`, `/ui/projects/:id/secrets`,
  `/ui/environments/:id?tab=logs|history`, `/ui/queue`, `/ui/audit`,
  `/ui/admin`) with filters as query params, all deep-linkable — the API
  router (`api/mod.rs`, then still part of the single `api.rs`)
  now `.fallback()`s to `index.html` for any unmatched GET so a hard
  refresh on a nested path still works (tested live: reloading mid-route
  and watching the client router take over correctly). Added the
  previously-missing write/process surface: deploy a new branch and roll
  back from a project's own page, delete a project, full secrets CRUD
  (global + project/branch-scoped, values write-only), and an admin page
  (API tokens create/list/revoke, master-key rotation, backup download).
  Every native `confirm()` was replaced with an in-page confirm modal after
  discovering live that a native one freezes the entire tab — including
  CDP-driven browser automation — until a human dismisses it. Also found
  and fixed live: a project's secrets page rendered empty despite the API
  returning real data, because an unhandled rejection inside
  `loadForRoute`'s per-route switch silently skipped the rest of that
  route's loads — now wrapped in try/catch with a visible notice banner,
  plus `cache: "no-store"` on every fetch defensively.
- **Real repo push+webhook playground (`~/oxid-playground/`), set up for the
  user to test hands-on — a bare repo as "origin" with a `post-receive`
  hook that signs and fires the real webhook on every push, so even local
  git exercises the exact same auto-detect path a real GitHub push would.**
  Found and fixed 3 more real bugs from that live use:
  1. `GET /api/v1/stats` gained `traefik_enabled` — without
     `OXID_DOCKER_NETWORK`, an environment's `url` is a
     `branch.base-domain` hostname that only means anything as a Traefik
     `Host()` rule and isn't reachable at all; the dashboard was linking to
     that dead address instead of the project's actual published
     `host:port`. Now links to whichever is actually reachable.
  2. `BuildFailed` audit events were recorded with `detail: None` — the
     real Docker/git error was in hand but silently dropped before
     storage, so the audit trail showed a bare "build_failed" with no way
     to tell what broke or which branch. Now stores the real error string;
     the dashboard's audit page also gained an `envIndex` branch-name
     resolution it had the data for but never actually used (every row
     showed an opaque `#4`).
  3. `serve()`'s plain-HTTP path called
     `axum::serve(...).with_graceful_shutdown(...)` with **no timeout** —
     only the TLS path enforced the documented "drains for up to 10s".
     A real playground daemon instance never exited on `SIGTERM` while a
     browser tab sat open on the dashboard (pooled keep-alive connection),
     needing `SIGKILL`. Now bounded to 10s on both paths.
  - Direct-port-publish mode's "only one live branch per project at a time"
    limitation got called out live as unacceptable ("Oxid debe ser
    dinámico... no es excusa") — and it wasn't a hard limit, just the
    fixed-host-port design. Fixed properly, not papered over: see the next
    item.
- **Dynamic host-port assignment — the actual fix for the item above, not
  just a better error message.** `ContainerPort::run` now always lets
  Docker pick the published host port itself (empty `HostPort` binding) and
  reports back which one it got; `Environment` gained `host_port:
  Option<u16>` to store it (migration `0006`). A busy port can never block a
  deploy again, and — bigger win — multiple branches of the *same* project
  now run simultaneously even without Traefik, each on its own port; the
  dashboard/CLI show each environment's real `host:port` instead of a
  project-wide port or a dead Traefik-style subdomain. Trade-off: the port
  isn't stable across redeploys of the same branch (acceptable for
  ephemeral previews). Live-verified on the exact repro: pushed `dev` while
  `main` was still up (paused, still holding its port) — `dev` deployed on
  a different port, both simultaneously live and independently reachable.
  New `#[ignore]`d Docker-integration test proves two containers on the same
  internal port get distinct host ports.
- **Two more bugs found live immediately after, waking the pre-existing
  `main` on the same playground:**
  1. Its `host_port` was `NULL` (deployed before the column existed) and
     stayed that way forever — `wake` only starts/unpauses the *existing*
     container, it doesn't recreate it through `run()` (the only place that
     used to learn the port). New `ContainerPort::published_port` inspects
     an already-running container to read its bound port back; `wake_env`
     and `reconcile_startup_state` both now backfill a stale `host_port`
     opportunistically instead of leaving it wrong forever.
  2. Waking it also caused an *immediate* re-pause on the next GC tick.
     Root cause: without Traefik, nothing ever calls `touch_by_url`
     (that's the `forwardAuth` heartbeat), so `last_accessed_at` is frozen
     at creation time forever — every direct-mode environment looks
     exactly as idle as a genuinely dead one, and left long enough this
     wouldn't just mis-pause but permanently *destroy* a live environment.
     `sweep` is now a deliberate no-op without `OXID_DOCKER_NETWORK`
     instead of acting on data it can't trust; manual pause/wake/destroy
     still work fine. 6 existing sweep tests updated to configure
     `with_traefik(...)` explicitly (the only mode sweep does anything in
     now); new test proves the no-op directly.
- **Editable project lifetime policy + push-to-deploy duration metric (this
  round), backend/API/CLI verified by tests and the dashboard live-verified
  in a real browser against the playground's real data:** previously
  `pause_after`/`destroy_after` were parsed from `oxid.toml` once at
  `register_project()` and frozen forever short of re-registering — asked
  for explicitly ("Oxid UI pueda configurar esos parametros de tiempo de
  vida"). New `ProjectStore::update`, `ControlPlane::update_project_ttls`
  (partial update, omitted fields keep their value), `PATCH
  /api/v1/projects/{id}`, `oxid configure --pause-after --destroy-after`,
  and a dashboard "Lifetime policy" form on the project page — live-verified
  end-to-end: PATCHed `pause_after` to `2700s` from the actual UI, confirmed
  via a fresh page reload and a direct `GET /api/v1/projects` that it
  persisted server-side (not just an optimistic UI update). Also added a
  client-side-only "deploy time" metric (`Environment.created_at` as the
  push-received proxy vs. the `build_succeeded`/`build_failed` audit event's
  `occurred_at`, via a `toEpochMs` helper that correctly decodes the `time`
  crate's `[year, ordinal_day, ...]` serde array) shown as a new column in
  the audit trail and in each environment's history tab — live-verified
  against the playground's real build events (values like `1.0s`/`0ms`,
  `–` for non-build event kinds).
- **Zero-downtime redeploys (this round), live-verified against real Docker
  with continuous polling during real pushes:** explicit, emphatic ask
  after seeing a redeploy work but wanting "0 caidas 0 defecto siempre
  elvantando algo para no tener fallas." Root cause: `deploy_at` destroyed
  the previous container *before* building/starting the new one — always a
  real gap — and in direct-publish mode the branch's address (`host_port`)
  changes on every redeploy anyway, so even a perfectly-timed swap would
  still break anyone already connected. Fixed at the root, not papered
  over: a redeploy now builds and starts the new instance fully (a
  per-deployment-unique container name — `Environment.container_name`,
  migration `0007` — so old and new coexist briefly without colliding),
  waits for it to actually accept TCP connections
  (`service/proxy.rs::wait_until_ready`, direct-publish mode only — Traefik
  mode already gets this ordering benefit for free since containers
  join/leave behind a stable `Host()` rule), *then* cuts traffic over and
  only *then* removes the previous container. A failed redeploy cleans up
  only the new (half-started) container and leaves the previous one running
  exactly as it was — a bad push can never take an already-live branch down
  with it (new regression test
  `failed_redeploy_leaves_the_previous_instance_untouched`).
  New built-in per-branch TCP reverse proxy (`service/proxy.rs`,
  `ProxyRegistry`) gives every branch a stable `public_port` in
  direct-publish mode — bound once, persisted (`Environment.public_port`),
  reused across every redeploy, with its upstream target swapped atomically
  at cutover — so the branch's *address* no longer changes either, on top
  of there being no gap. Rebuilt on daemon restart from the persisted port
  (`reconcile_startup_state`), released on pause (`mark_unavailable`, fails
  fast instead of hanging), destroy (`remove`), and re-armed on wake.
  Found and fixed a real bug surfaced by this change: `find_environment_by_branch`
  picked the highest-`id` row regardless of state, so a *failed* redeploy
  (which creates a higher-id `Destroyed` row) could shadow an
  actually-still-`Running` older row — silently breaking the webhook
  branch-deletion handler and `deploy_at`'s own "is there a previous live
  instance to protect" check. Now prefers the highest-id *live* row, falling
  back to the highest-id row overall only when nothing is live.
  Live-verified end-to-end on the real playground (real git pushes, real
  Docker, real `nginx` containers): pushed a content change to `main` while
  polling its stable port every 50ms — 200/200 requests returned 200, and
  the served content changed to the new commit, proving a real cutover (not
  just the old container surviving); then pushed a deliberately broken
  Dockerfile — the deploy failed with a real Docker error, `main` stayed at
  100% uptime (0/60 failed polls) serving the last good build the entire
  time, and a follow-up good push redeployed normally with the address
  (`public_port`) unchanged across all three deploys.
- **Private GitHub repo support (this round), live-verified end-to-end
  against a real private repo, a real fine-grained PAT, and a real GitHub
  webhook delivered over the internet:** the daemon clones every project
  into its own git-cache directory, independent of any credential helper
  the operator's shell has — so it could never deploy a private repo at
  all before this. New per-project git token (`PATCH
  /api/v1/projects/{id}`, `oxid configure --git-token`, a write-only
  dashboard field under "Private repo access") stored encrypted at rest
  with the same AES-GCM cipher used for project secrets — `rotate_master_key`
  was extended to also re-encrypt `projects.git_token_enc`, easy to miss
  since it's a separate table from `secrets`. Deliberately **not** a field
  on the `Project` domain struct (which is returned wholesale from `GET
  /api/v1/projects`) — it only ever exists in memory for the instant
  `deploy_at` decrypts it right before the git operation that needs it.
  `GitPort::ensure_repo` gained an `Option<&str>` token param, injected as
  HTTPS userinfo (`x-access-token:<token>@host`) only for the actual
  clone/fetch network call — `cache_dir_name` still hashes the token-free
  URL, `Repository::clone`'s persisted `origin` URL is reset back to the
  token-free one immediately after cloning, and every fetch goes through
  an anonymous (never `.git/config`-persisted) remote instead of
  `find_remote("origin")` + `remote_set_url`, so a token can't linger on
  disk anywhere, ever, even transiently.
  Setup for the live test, for reference: created `sazardev/oxid-private-test`
  (private) via `gh repo create`; installed `cloudflared` (only for this
  local sandbox — real production wouldn't need a tunnel, since the daemon
  would already have a real public address) and exposed a *fresh*
  daemon instance that had `OXID_API_TOKEN` set *before* exposing it
  publicly (never exposed the open-by-default instance); created a
  **fine-grained, read-only, single-repo** GitHub PAT (explicitly not the
  broad `gh auth token` already logged in — reusing that got correctly
  blocked by the safety classifier as credential over-scoping, and the
  literal `--git-token <PAT>` CLI invocation also got blocked for landing
  in shell history/`ps`, so the user ran that one command themselves via
  `!`); created a real webhook via `gh api repos/.../hooks` pointed at the
  tunnel URL. A real `git push` delivered a real GitHub webhook over the
  internet, the daemon decrypted the stored token, cloned the private repo,
  built, and cut over with zero downtime (same `public_port` across both
  deploys, new commit's content actually served).
- **Observability + CLI ops + infra bootstrap + SRP refactor rounds (these
  rounds, all covered by tests):**
  - *Structured tracing & request correlation* (`73c02fc`): `tracing`
    subscriber with `OXID_LOG_FORMAT=json` for machine-parseable logs;
    per-request `request_id` middleware (honors an inbound
    `X-Request-Id`, echoes it on the response) propagated to every
    `ControlPlane` call via a task-local (`request_context.rs`) so daemon
    logs and the audit trail carry the same id (migration `0009` added
    `audit.request_id`); `CatchPanicLayer` turns a panicking handler into a
    JSON 500 tagged with the request id instead of a raw connection reset;
    `oxid audit` gained `--branch/--project/--since/--until/--kind` filters.
  - *CLI ops round* (`663c9fe`, `e6c263c`): named connection contexts
    persisted in a config file (`oxid context add/use/list/current/remove`)
    — `--api`/`--token`/`OXID_API`/`OXID_TOKEN` still override;
    `oxid completions <shell>`; `oxid doctor` extended with an
    `/api/v1/infra/status` check; errors differentiated by cause with
    actionable hints and distinct exit codes (2 not-found, 3 unreachable,
    4 auth); `oxid ps --sort branch|state|updated`.
  - *Traefik/network bootstrap* (`b4b1eb4`): `oxid infra status`
    (read-only: does the `OXID_DOCKER_NETWORK` network exist, is Traefik
    running, is this daemon's own container wired for wake-on-request) and
    `oxid infra setup` (idempotent: creates the network and starts the
    built-in Traefik container if missing). Wiring the daemon's own
    container is deliberately **not** automated — Docker can't relabel a
    running container without recreating it, and recreating the process
    executing the call is unsafe — `InfraStatus::next_steps` prints exactly
    what's left instead. This closes most of what ROADMAP's old "wiring
    pendiente" note described as manual.
  - *SRP refactor* (`f33d547`, `556713f`): no behavior change —
    `control_plane.rs` split into `service/control_plane/` (deploy,
    provision, lifecycle, gc, infra, admission, auth, project, …) and
    `api.rs` into `api/` (router in `mod.rs`, one file per resource under
    `handlers/`, plus `middleware.rs`, `dashboard.rs`, `error.rs`,
    `types.rs`). Docs referencing the old god-file paths were updated in
    the same sweep.
- Full test/clippy/fmt pass green (257 tests: 42 CLI + 44 core + 171
  daemon, 8 `#[ignore]`d Docker-integration ones excluded).

## Known gaps (by design, not bugs — see ROADMAP.md P4)

No TUI, no Tauri desktop app (the web dashboard now covers most of what
SPEC.md asks of both). Single-host only, no HA. Traefik bootstrap is
automated (`oxid infra setup` creates the network and the Traefik
container), but wiring the daemon's *own* container onto that network still
needs the operator (Docker can't relabel a running container; `oxid infra
status` prints the exact remaining steps). Named API tokens now support
project scoping (RBAC-lite) but there are no roles/permission groups beyond
"scoped/unscoped + master" — richer RBAC doesn't exist yet. Production
posture is documented in `PRODUCTION.md` (CLI-first release, Traefik mode
supported).

## Operational lesson learned (read before running heavy builds here)

This machine is shared with ~15-20 other always-on Docker containers from
unrelated projects (gm-erp2, gm-iris, hades, sana-supabase, ...). WSL2's
VM crashed once this session after running `cargo build --release`
(LTO + `codegen-units=1`, cross-compiled to musl) **inside a `docker
build`** while the box was already near its memory ceiling. Root cause
confirmed via `journalctl`: repeated `oom-kill` events, then the boot log
cut off mid-write with no clean shutdown — the Hyper-V VM itself locked up
and had to be restarted.

**Rule going forward:** never run the `release` profile (LTO on) inside a
Docker build for local smoke-testing on this box. Use `cargo build`
(dev profile) and run the binary natively or mount it into a plain base
image — that's what the P3 and SSE live-test rounds did this session, and
the host stayed at 6-8GB free throughout. Reserve the LTO release profile
for the actual CI/release pipeline, which runs on isolated infrastructure.

## Housekeeping

Untracked files `.agents/`, `.claude/`, `nul`, `skills-lock.json` at the
repo root are Claude Code skill-installation artifacts, not part of the
Oxid project — deliberately left uncommitted every round.
