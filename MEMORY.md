# MEMORY.md — working state as of 2026-08-18

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
  GC-vs-manual-action race (widened `lifecycle_lock`).
- `oxid logs -f` now streams live over SSE (`GET .../logs/stream`,
  `ContainerPort::stream_logs`) instead of polling every 2s — confirmed
  live against a container ticking once/second.
- Full test/clippy/fmt pass green (97 daemon/core tests + 20 CLI tests),
  binary size and CLI-startup perf pass done (see `ROADMAP.md` §"tercera
  ronda").

## What's NOT retested since the last merge

The `origin/main` history briefly diverged (another session had pushed
Docker/Nix/CI scaffolding commits) and was merged back in at `7333662`.
That merge only touched imports/formatting in the Rust files it conflicted
on — but the following were verified *before* that merge, not after, and
would benefit from one more live pass if you want maximum confidence:
webhook-triggered deploy/destroy with a real signed payload, `rm-project`
cascade, `--purge-secrets`, and Traefik wake-on-request through a real
Traefik container.

## Known gaps (by design, not bugs — see ROADMAP.md P4)

No TUI, no web dashboard, no Tauri desktop app. Single-host only, no HA.
API is open by default if `OXID_API_TOKEN` is unset (warned, not enforced).

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
