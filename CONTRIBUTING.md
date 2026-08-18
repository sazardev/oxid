# Contributing to Oxid

Thanks for considering a contribution — Oxid is early-stage and every bit of
help (code, docs, bug reports, design feedback) matters.

## Before you start

- For anything beyond a small fix, open an issue first describing what you
  want to change and why. It saves everyone rework if the approach needs
  discussion.
- Check [`ROADMAP.md`](ROADMAP.md) for a prioritized, granular list of gaps
  between the vision docs (`IDEA.md`, `SPEC.md`, `DESIGN.md`) and what's
  actually implemented — it's the best place to find something to work on.
  Issues labeled [`good first
  issue`](https://github.com/sazardev/oxid/labels/good%20first%20issue) are a
  good place to start if you're new to the codebase.
- Not sure your idea is fully baked yet? Open an
  [Idea](.github/ISSUE_TEMPLATE/idea.md) issue instead of a feature request —
  it's meant for rougher, discussion-stage proposals.
- Found a security vulnerability? Do **not** open a public issue — follow
  [`SECURITY.md`](SECURITY.md) instead.

## Development setup

Requires the Rust toolchain pinned in
[`rust-toolchain.toml`](rust-toolchain.toml) (stable, with `clippy` and
`rustfmt`) and a running Docker daemon (the daemon orchestrates containers via
`bollard`).

```bash
cargo build --workspace
cargo test --workspace
cargo run -p oxid-daemon   # starts the control plane
cargo run -p oxid-cli -- ps
```

## Before opening a PR

```bash
cargo fmt
cargo clippy --workspace --all-targets   # warnings on clippy::all + pedantic are treated as review blockers
cargo test --workspace
```

## Architecture rules

Oxid follows hexagonal architecture (ports & adapters) — see `SPEC.md` §2 and
the crate-level docs in `crates/oxid-core/src/lib.rs` /
`crates/oxid-daemon/src/lib.rs`. When adding a feature:

1. Domain rules and new port traits go in `oxid-core` — it must stay free of
   any I/O, SQL, Docker, or HTTP dependency.
2. Adapter implementations (SQLite, Git, Docker, config parsing) go in
   `oxid-daemon/src/adapter/*`.
3. Orchestration wiring belongs in `oxid-daemon/src/service/control_plane.rs`.
4. HTTP/CLI exposure comes last, once the domain and adapter logic exist.

`unsafe_code` is forbidden workspace-wide (`Cargo.toml` lints) — PRs
introducing `unsafe` will not be merged.

## Commit / PR conventions

- Keep PRs focused; unrelated cleanups belong in a separate PR.
- Write commit messages that explain *why*, not just *what*.
- Fill out the PR template — it's short by design.

## License

By contributing, you agree that your contributions are licensed under the
project's [0BSD license](LICENSE) — the same terms as the rest of the
codebase.
