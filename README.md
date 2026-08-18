<div align="center">
  <img src="assets/logo/oxid-icon.svg" width="96" height="96" alt="Oxid logo" />

  <h1>Oxid</h1>

  <p><strong>Ephemeral environments that breathe. Ferrous performance, invisible footprint.</strong></p>

  <p>
    <a href="LICENSE"><img alt="License: 0BSD" src="https://img.shields.io/badge/license-0BSD-DE5236.svg"></a>
    <img alt="Rust edition 2024" src="https://img.shields.io/badge/rust-edition%202024-DE5236.svg">
    <img alt="Status: alpha" src="https://img.shields.io/badge/status-alpha-262626.svg">
    <a href="CONTRIBUTING.md"><img alt="PRs welcome" src="https://img.shields.io/badge/PRs-welcome-4A9E79.svg"></a>
  </p>

  <p><a href="https://sazardev.github.io/oxid/"><strong>Website →</strong></a></p>
</div>

---

Oxid is a **self-hosted, opinionated control plane for branch-based preview
environments** — the Vercel of your local server, at the resource cost of a
calculator.

Push a branch. Oxid clones it, builds it, deploys it behind a reverse proxy,
and gives it its own URL. Nobody visits it for 30 minutes? Oxid pauses the
container in memory. The next request wakes it in milliseconds. No idle
containers eating RAM, no cloud bill for preview environments nobody is
looking at.

Written entirely in Rust, `unsafe` forbidden workspace-wide.

## Why

Spinning up a feature-branch environment per PR usually means either paying a
cloud platform for staging environments that sleep 90% of the time, or
drowning your local server/NAS in Docker containers nobody remembers to stop.
Oxid detects a push, injects secrets, deploys the container, and scales it to
zero the moment it goes idle — so you get Vercel-style preview URLs without
the bill or the resource sprawl.

## How it works

1. You push a branch. A webhook (HMAC-verified) reaches the Oxid daemon.
2. Oxid clones the repo (cached, hard-linked for speed), builds the image, and
   resolves secrets through a `Global → Project → Branch → Runtime`
   inheritance chain.
3. The container is deployed and routed to `<branch>.<project>.local.dev`.
4. A background scheduler watches activity. Idle past `pause_after`? The
   container is paused in memory. A request comes in for a paused branch?
   Oxid wakes it and serves the request.

See [`IDEA.md`](IDEA.md) for the product philosophy, [`SPEC.md`](SPEC.md) for
the full architecture spec, and [`DESIGN.md`](DESIGN.md) for the visual
language. [`ROADMAP.md`](ROADMAP.md) tracks exactly what's implemented today
versus what those documents describe — Oxid is early and evolving in the
open.

## Project layout

This is a Cargo workspace with a hexagonal (ports & adapters) architecture:

| Crate | Role |
|---|---|
| [`oxid-core`](crates/oxid-core) | Pure domain: entities, state machine, port traits. No I/O. |
| [`oxid-daemon`](crates/oxid-daemon) | The control-plane binary (`oxidd`): SQLite, Git, Docker adapters, the HTTP/webhook API, and the scale-to-zero scheduler. |
| [`oxid-cli`](crates/oxid-cli) | The `oxid` CLI — a thin HTTP client for the daemon. |

## Getting started

Requires the Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml)
(stable, with `clippy` and `rustfmt`) and a running Docker daemon.

```bash
# Build everything
cargo build --workspace

# Run the control-plane daemon (reads OXID_* env vars — see crates/oxid-daemon/src/main.rs)
cargo run -p oxid-daemon

# In another shell, talk to it via the CLI
cargo run -p oxid-cli -- ps
```

Run the test suite and linter before sending a PR:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

## Contributing

Contributions are very welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for
how to get set up, and [`ROADMAP.md`](ROADMAP.md) for a prioritized list of
what's missing. Open an issue before starting on something large so we can
align on approach first.

## Security

Found a vulnerability? Please don't open a public issue — see
[`SECURITY.md`](SECURITY.md) for how to report it privately.

## License

Oxid is released under the [0BSD license](LICENSE) — do whatever you want
with it, commercially or otherwise, with or without attribution.
