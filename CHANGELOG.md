# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/1.1.0/);
versioning is [SemVer](https://semver.org/) — on the `0.x` line the **minor**
is the breaking position.

## [Unreleased]

### Changed

- **Deploys no longer serialize node-wide.** One mutex covered every deploy,
  pause, wake and destroy on the daemon, so a team's pushes finished one
  after another however many cores the host had — fifteen branches took as
  long as fifteen builds run back to back. Locking is now keyed by what is
  actually shared: a branch (its rows, its container, its cutover), a
  project's git cache (held only until the build context is captured, never
  across the build), the admission check, and a shared resource pool's slot
  table. Sibling branches build and start at the same time.

  Fifteen branches pushed simultaneously — real signed webhooks, real Docker
  and Traefik — settle in **7.1 s instead of 27.3 s**, or 4.2 s with
  `OXID_DEPLOY_CONCURRENCY=16`. See `BENCHMARKS.md`.
- The deploy queue drains in waves of `OXID_DEPLOY_CONCURRENCY` (default 4)
  rather than one entry at a time. Order is still respected between waves,
  and a wave that reports the host is full still stops the drain, so a large
  deploy is not starved by a stream of small ones behind it.

### Security

- **The daemon handed its master token to anyone who asked.**
  `GET /api/v1/setup/token` is pre-auth by design, and its safety rested on
  the shipped compose publishing the port on `127.0.0.1` — but `OXID_ADDR`
  defaults to `0.0.0.0`, and the startup guard only refuses an open API when
  *no* token exists. `OXID_AUTO_TOKEN=1` therefore started a daemon that
  answered this to the whole network. Verified against a LAN address: the
  token came back, and with it `GET /api/v1/backup` (the database and the
  AES master key used to encrypt every secret) and the webhook secret —
  enough to deploy arbitrary code. Who the endpoint answers is now
  `OXID_BOOTSTRAP_TOKEN_ACCESS`, and it defaults to withholding.

  Making it a setting rather than a rule is the point: a containerized
  daemon is always bound to `0.0.0.0`, and Docker's forwarding makes the
  operator on the host and a stranger on the network arrive from the same
  bridge address — what separates them is whether the port was published on
  `127.0.0.1`, which only the operator knows. `loopback` (the default)
  serves callers on the daemon's own host, judged on the real peer address
  and never on `X-Forwarded-For`, denying when no peer address is available
  at all; `any` serves anyone who can reach the port, which the shipped
  compose sets because it publishes on loopback; `off` disables it.
- **The pre-auth onboarding routes are rate-limited.** They sat outside the
  limiter, leaving the two endpoints an unauthenticated caller can reach by
  design — one of which hands over a credential — as the only ones on the
  daemon nothing throttled.

### Added

- **A repository no longer needs a Dockerfile.** Oxid reads what a project
  already says about itself — `package.json`, `go.mod`, `pyproject.toml`,
  `Cargo.toml` — and generates one. Recognised today: NestJS, Next.js, Vite
  and Create React App and Angular (as SPAs), Express and other plain Node
  servers, Go with Fiber, Gin or Echo, FastAPI, Flask, Django, Axum, Actix,
  and a plain directory of static files. The package manager comes from the
  lockfile and the runtime version from `.nvmrc`, `engines.node`, `go.mod`
  or `requires-python`.

  Every generated Dockerfile is multi-stage, and that is the point on a host
  meant to run many environments at once: measured on a Nest service, 215MB
  against the 1.63GB of the single-stage Dockerfile written by hand. It also
  installs dependencies before copying the source, so a commit that does not
  touch the lockfile reuses the install layer — on a branch rebuilt every
  push, that layer is most of the build.

  It only ever fills a gap. `oxid.toml`, a Compose file or a committed
  `Dockerfile` all win, the generated file is written into the private build
  context and never the checkout, and a repository Oxid cannot identify gets
  the same "write a Dockerfile" error as before rather than a build that
  fails halfway through.
  Verified by building and serving one real repository per stack through
  Docker. That is how three defects surfaced that no assertion on generated
  text could have: `npm ci` refuses to run without a lockfile, `go build -o
  app ./...` breaks on a module with more than one `main`, and the Rust
  stage copied a binary path Cargo names after the package. Generated image
  sizes: Go 24.5MB, SPA and static 94.5MB, Rust 136MB, FastAPI 209MB, Nest
  215MB, Express 246MB.
- **The detected stack is tracked and shown**, as a dashboard tag with the
  evidence behind it (`Detected from package.json, @nestjs/core`), a column
  in `oxid ps`, and a `detected_stack` field on the project in the API.
- **The dashboard is a PWA and works on a phone.** A manifest, a service
  worker and SVG icons ship in the binary like the rest of the panel, so it
  installs to a home screen and opens with the daemon unreachable — verified
  by stopping the daemon and hard-navigating to a nested route. The worker
  never caches `/api/`: a cached environment list is a lie about live
  cluster state, and an error is better than a lie.
- **Layout works from 320px to ultrawide.** Below 760px each table row
  becomes a labelled card instead of something to scroll sideways, and the
  nav becomes a thumb-scrollable strip rather than a column that pushed the
  fleet off the first screen. The page container went from a fixed 1400px
  cap — which left a third of a modern monitor empty — to a fluid width.
- **`DELETE /api/v1/queue/{id}` cancels a queued deploy**, with a button in
  the dashboard's queue view. The drain stops at the first entry that does
  not fit, so a large deploy is never starved by small ones behind it — and
  the other side of that is one entry which can never fit holding up
  everything after it. Until now the cures were making it fit or restarting
  into an empty database. Scoped like the listing: an operator who cannot
  see an entry gets the same `404` for cancelling it.
- **A diagnostics page** — everything `oxid doctor` reports (reachable,
  version, latency, whether the token authenticates and how far it reaches)
  next to what only the daemon knows: capacity, queue depth, routing mode,
  and the infrastructure wiring. "Something is wrong, where do I look" had
  no single answer in the dashboard before.
- **The dashboard can register a project.** It could only ever be done
  inside the onboarding wizard, which runs once and then gets out of the
  way — so a team's second repository had no route into the dashboard at
  all and had to go through the CLI.
- **An infrastructure panel**, showing the Docker network, Traefik and the
  wake-on-request wiring, with the repair button and the exact next steps
  for whatever cannot be automated. Also wizard-only until now, though "is
  Traefik still wired?" is a daily question, not a first-run one.
- **Restore from the dashboard.** Downloading a snapshot was already there;
  putting one back meant the CLI. The confirmation says what an operator
  most often misses — that it replaces every secret, and that nothing
  happens until the daemon restarts.
- **Bulk actions on environments**, with a select-all and a selection that
  follows the filter, so what a bulk button acts on is always what the table
  shows as ticked. Single-environment buttons stop scaling at about the size
  this product is for: fifteen branches rendered thirty-two loose
  `wake`/`destroy` buttons, and putting the fleet to sleep was fifteen
  clicks and fifteen confirmations.

### Fixed

- **The onboarding wizard always said wake-on-request was unwired.** It read
  `self_wiring_ok`, a field no version of `GET /api/v1/infra/status` ever
  sent, so the indicator was `undefined` — permanently red on daemons that
  were perfectly wired. The verdict the domain already computes
  (`SelfWiringStatus::is_fully_wired`) is now serialized alongside the
  evidence.
- **Wide tables scrolled the whole page sideways.** An eight-column table
  does not fit a tablet, and letting it widen the document put a horizontal
  scrollbar under the nav and the stat strip too. Each table now scrolls
  inside its own box; verified at 1440, 1024, 768 and 480 px.
- **Nothing had a focus style.** A keyboard user got the browser default,
  which on this near-black surface is close to invisible. Every control now
  has a `:focus-visible` ring, and there is a skip link past the nav.
- **Controls were below the size a finger can hit.** `.btn-sm` measured
  about 22px tall. Where the pointer is coarse they are 44px, links clear
  the 24px WCAG floor, and inputs are 16px so iOS stops zooming the page on
  focus. Measured across every view at 390px: 18 undersized targets before,
  none after.
- **Muted text failed contrast.** `--ash-gray` measured 4.2:1 against the
  page — below AA — on every label and timestamp in the panel. Now 6.1:1.
- **Every failed read showed a bare status code.** The API answers errors as
  a message written to be acted on — "set `OXID_DOCKER_NETWORK` first, then
  restart" — and in the caller's own language. Reads threw the status code
  instead and discarded the body, so the dashboard displayed `404` where the
  daemon had explained exactly what to do. Both halves of the API surface now
  fail the same way.
- **`OXID_DOCKER_NETWORK is not set` was hardcoded English.** The dashboard
  sends `Accept-Language`, so a Spanish panel showed an English answer — the
  exact gap the daemon's catalog exists to close.
- **The infrastructure panel flickered against its own refresh.** The page
  re-reads every few seconds and each cycle blanked the result before
  fetching, so "Checking…" sat next to the answer it was supposedly
  replacing. A refresh now either replaces the answer or reports why it
  could not, and a failed check keeps a way to retry it — the retry button
  used to live inside the block that renders only on success.
- **Registering a project from the projects page said it was deploying.** It
  reused the wizard's message, where registering *is* followed by a deploy.
  Nothing deploys from that page, and saying so sent people looking for a
  build that never started.
- **Counted messages read wrong in the singular** — "1 environments", "1
  entornos". Both languages inflect the noun, so the counted strings carry
  a `.one` and an `.other` form, with a test for the keys that are built at
  runtime and therefore invisible to the existing catalog check.
- **A restore could destroy the database it was meant to rescue.**
  `apply_staged_restore` wrote whatever bytes the uploaded archive held over
  the live `audit.sqlite` and deleted the marker before finding out whether
  the result opened. A truncated or wrong-format upload took out the last
  good copy and left a daemon that could no longer start — on the path that
  runs precisely when the operator has nothing else left. The archive is now
  unpacked beside the real files and checked before anything is swapped in,
  what it replaces is kept as `audit.sqlite.pre-restore`, the write-ahead
  log of the replaced database is cleared, and a rejected archive is set
  aside as `.restore-failed.tar` while the daemon starts normally on the
  database it already had. The startup half of restore had no test coverage
  at all; it now has six.
- **A deploy interrupted by a restart stayed `Building` forever.** Startup
  reconciliation skipped those rows, and admission counts `building` as
  memory the host has promised — so every daemon killed mid-deploy leaked a
  reservation nothing was using, until enough accumulated to refuse deploys
  the node had room for. The same failure `Paused` used to cause, in a state
  nobody swept. They are now recorded as `BuildFailed`, which is what
  actually happened to them.
- **Every deploy did its own `git fetch`, one at a time.** A fetch brings
  down every branch of a repository, so the first of a burst had already
  retrieved what the rest were about to ask for — and they repeated it
  anyway, serialized behind the lock that protects the shared checkout, one
  network round-trip each. On fifteen branches that was three quarters of
  the wall-clock. Concurrent deploys of one project now share a fetch,
  decided on when each caller *asked* rather than on the age of the result,
  so nobody is served data older than their own request.
- **A burst of pushes waited out a scheduler tick doing nothing.** Webhooks
  arrive faster than a drain can read the queue, so most of them landed after
  its snapshot and were answered "a drain is already running" — true, but
  that drain had already read past them, and nobody looked again until the
  next tick. On fifteen simultaneous pushes that idle wait was the majority
  of the wall-clock. A drain now re-reads the queue before finishing.
- **Concurrent deploys of sibling branches could build each other's code.**
  `checkout_commit` force-rewrites one on-disk working directory shared by
  every branch of a project; the build read straight from it. Each deploy now
  takes a private copy of the build context (symlinks preserved) before the
  git lock is released.
- **Admission could over-commit under concurrency.** Two deploys arriving at
  once each saw the same free memory and each claimed it. The check is
  serialized, counts `building` alongside `running`, and excludes the row the
  asking deploy just created.
- **Two branches could be handed the same Redis database.** The lowest free
  slot is chosen by reading which are taken and then claiming one, and a
  lease is unique per branch, not per slot — so nothing caught the
  collision. The read and the claim are now held under one lock per pool.
- A branch with no `oxid.toml` of its own silently replaced its project's
  registered configuration with zero-config defaults inferred from its
  Dockerfile.

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
