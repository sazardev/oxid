# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/1.1.0/);
versioning is [SemVer](https://semver.org/) — on the `0.x` line the **minor**
is the breaking position.

## [0.2.0] - 2026-08-29

Scale-to-zero worked in every unit test and in no real deployment. Simulating
a ten- then fifteen-developer team against a real GitHub repository — twelve
branches, signed push webhooks, the full Docker + Traefik stack — surfaced
seventeen defects, of which two made the flagship feature unusable and one
deadlocked a busy node outright.

### Fixed

- **Waking a scaled-to-zero branch never worked end to end.** Traefik's Docker
  provider only publishes routers for `running` containers and ignores
  pause/unpause events, so a `docker pause`d environment lost its route
  permanently and answered `404` — the `errors` middleware that powers waking
  only fires on a router that still exists. Nine of eleven paused environments
  were unreachable. Suspension now uses `stop`, the daemon carries a
  lowest-priority catch-all router so a stopped branch has something to answer
  it, and waking dispatches on the container's actual state.
- **A sleeping branch reserved memory it was not using**, deadlocking the node.
  Admission counted `Paused` environments, which was right while suspension
  froze a container and kept its resident set. Once suspension started
  stopping containers, eleven stopped environments reserved 1408 MB while
  consuming none, and four deploys queued behind them forever.
- **An application's own 5xx was replaced by the wake page.** The middleware
  caught all of `500-599`, so a branch whose code threw showed its developer
  "Waking up…" reloading every two seconds instead of the stack trace.
  Narrowed to `502-504` — the codes Traefik itself emits when it cannot reach
  a backend.
- **A failed build left no trace at all**: no environment row, no audit event,
  no ERROR line, only a `500` on a webhook nobody watches. The row is now
  created before the build and every failure path records against it.
- **Webhook deliveries ran the whole deploy inline**, taking 54 s on a cold
  build against GitHub's 10 s delivery timeout — reported as failed while the
  environment came up fine, and deployed twice if redelivered by hand.
- **A branch's own `oxid.toml` was ignored.** A branch declaring a Postgres
  dependency deployed "successfully" with no database and no `DATABASE_URL`.
- **Repository matching was a substring test**, so a push from an unregistered
  repository whose name prefixed a registered one deployed that project.
- **Two branches normalising to one subdomain both claimed it**, leaving one
  permanently unreachable with nothing saying why.
- **A dead row could shadow the live environment on the same URL**, and a
  failed deploy could hide the instance still serving a branch.
- **`POST /api/v1/tokens` ignored unknown fields**, so a misspelled `projects`
  key silently minted a full-access token.
- **The Traefik `oxid infra setup` starts routed nothing** on Docker Engine
  ≥ 29 and lacked the timeouts waking depends on. Its published port also
  conflated the container and host sides, so any value but 80 bound a port
  nothing listened on.
- Transient failures (DNS, network) lost the push entirely; they are now
  retried from the persisted queue, while permanent ones are not.
- Tearing down an environment required a container that may never have
  existed.

### Added

- `EnvironmentState::BuildFailed` — a real state, distinct from `Destroyed`,
  because "someone's push is broken" and "this was torn down" are not the same
  thing to whoever is reading `oxid status`. Surfaced in the CLI, the
  dashboard and `/api/v1/stats`.
- `GET /api/v1/environments/{id}`, which only accepted `DELETE` before.
- `OXID_TRAEFIK_HTTP_PORT` — host port for the built-in Traefik, so a machine
  whose 80 is taken can still run `oxid infra setup`.
- `OXID_COMMIT` injected into every container alongside `OXID_BRANCH` and
  `OXID_ENV_URL`.
- Audit coverage for manual pause and wake, and the pushing user recorded as
  the `operator` on everything arriving by webhook.

### Changed

- **Webhooks answer `202 {"status":"queued"}`** and deploy off a persisted
  queue that survives a restart, instead of deploying inside the request.
- **API timestamps are RFC 3339.** They were `time`'s positional array
  (`[2026, 241, 15, …]`), which no consumer can read without reimplementing
  the calendar — it had already leaked into the CLI as `2026-day241 15:20:45`.
- **Admission is decided once**, after the checkout, against the branch's real
  request rather than config the deploy would not use.
- Traefik's `providersThrottleDuration` and dial timeout are tuned in the
  shipped compose and the built-in bootstrap: waking went from 2 280 ms to
  285–850 ms, almost all of it reclaimed here rather than in this daemon.

### Migration

- Database migration `0011_deploy_queue_attempts.sql` runs automatically at
  startup.
- Consumers of `/api/v1` must expect RFC 3339 strings where they previously
  parsed arrays, and a `build_failed` environment state.
- Anything asserting on a webhook's `"deployed"` response should expect
  `"queued"`.
- Existing deployments should adopt the new `docker-compose.yml` Traefik
  labels and flags; `oxid infra status` reports what is missing.

## [0.1.1] - 2026-08-25

### Fixed

- Wake-on-request was unreachable and the rate limit misconfigured.
- Eviction is state-aware; already-paused containers are no longer re-paused
  on every sweep.
- The build-context tar no longer fails on a dangling symlink.

### Added

- One-command DevOps installer (`install.sh`), polished landing and
  onboarding, self-serve bootstrap token from the CLI and the setup wizard.

## [0.1.0] - 2026-08-24

First tagged release: the control plane, CLI, daemon and dashboard.

[0.2.0]: https://github.com/sazardev/oxid/releases/tag/v0.2.0
[0.1.1]: https://github.com/sazardev/oxid/releases/tag/v0.1.1
[0.1.0]: https://github.com/sazardev/oxid/releases/tag/v0.1.0
