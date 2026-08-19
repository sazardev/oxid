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
- Full test/clippy/fmt pass green (128 daemon/core tests + 32 CLI tests).

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
