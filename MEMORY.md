# MEMORY.md — working state as of 2026-08-19

Quick-glance status for whoever (human or Claude) picks this repo back up.
For the granular, tracked-item gap analysis see `ROADMAP.md` — this file is
just "where did we leave off and why."

## What's solid and live-verified

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
  `/ui/admin`) with filters as query params, all deep-linkable — `api.rs`
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
- Full test/clippy/fmt pass green (132 daemon/core tests + 32 CLI tests).

## Known gaps (by design, not bugs — see ROADMAP.md P4)

No TUI, no Tauri desktop app (the web dashboard now covers most of what
SPEC.md asks of both). Single-host only, no HA.
API is open by default if `OXID_API_TOKEN` is unset (warned, not enforced).
Traefik auto-wake-on-request without any manual config still needs the
operator to wire the `errors` middleware themselves (documented, not
automated by Oxid's code — see `control_plane.rs::traefik_labels`). Named
API tokens are flat (no roles/permissions beyond "master" vs "operator") —
real RBAC (scoping a token to specific projects) doesn't exist yet.

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
