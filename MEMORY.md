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
- Full test/clippy/fmt pass green (109 daemon/core tests + 28 CLI tests).

## Known gaps (by design, not bugs — see ROADMAP.md P4)

No TUI, no web dashboard, no Tauri desktop app. Single-host only, no HA.
API is open by default if `OXID_API_TOKEN` is unset (warned, not enforced).
Traefik auto-wake-on-request without any manual config still needs the
operator to wire the `errors` middleware themselves (documented, not
automated by Oxid's code — see `control_plane.rs::traefik_labels`).

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
