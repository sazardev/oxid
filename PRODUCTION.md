# Running Oxid in production

The supported way to run Oxid for real work is **CLI-first**: one daemon
(`oxidd`) on a Docker host, operators driving it with `oxid`. The embedded web
dashboard ships in the same binary at no extra cost, but nothing below
requires opening it to the world.

Single-node by design — Oxid does not promise HA. Disaster recovery is
snapshots, not replication (see [Backups & DR](#backups--dr)).

---

## 1. Pick your topology

| | **Traefik mode** (supported) | Direct-publish mode |
|---|---|---|
| How it works | Containers join `OXID_DOCKER_NETWORK`, Traefik routes `<branch>.<base-domain>` | Each container publishes a random host port behind Oxid's built-in TCP proxy |
| Address per branch | Stable subdomain | Stable local `public_port` (not routable off-host) |
| Scale-to-zero | **Works** (`forwardAuth` heartbeat feeds idle detection) | **Disabled** — nothing refreshes `last_accessed_at`, so the GC sweep deliberately no-ops |
| Multiple branches live at once | Yes | Yes |

Without `OXID_DOCKER_NETWORK`, environments run until someone manually pauses
or destroys them. That is fine for a laptop; for a server, use Traefik mode.
The daemon prints a warning at startup in direct-publish mode, and
`oxid doctor` reports which mode you're in.

## 2. Install (Docker Compose)

[`docker-compose.yml`](docker-compose.yml) is the reference stack: daemon +
Traefik pre-wired for wake-on-request, socket mounted, `/data` volume,
periodic backups enabled.

```bash
export OXID_WEBHOOK_SECRET=$(openssl rand -hex 32)   # verifies webhook signatures
export OXID_API_TOKEN=$(openssl rand -hex 32)        # authenticates every control request

docker compose up -d
oxid context add prod --api http://your-host:8080 --token "$OXID_API_TOKEN"
oxid context use prod
oxid doctor        # reachability, auth, version match, infra checks
oxid infra status  # what's still missing for Traefik routing
```

If you start Traefik yourself instead of via compose, `oxid infra setup`
idempotently creates the network/container. One step always stays manual:
wiring the *daemon's own* container onto the network (Docker cannot relabel a
running container) — `oxid infra status` prints exactly what's left.

### Bare-metal binary instead

The release artifacts are static musl binaries. Run `oxidd` under systemd with
the same environment variables as the compose service, plus
`OXID_TLS_CERT`/`OXID_TLS_KEY` if you want TLS termination in-process rather
than behind a proxy:

```ini
# /etc/systemd/system/oxidd.service (excerpt)
[Service]
Environment=OXID_DATA_DIR=/var/lib/oxid
Environment=OXID_ADDR=0.0.0.0:8080
EnvironmentFile=/etc/oxid/oxidd.env   # OXID_API_TOKEN, OXID_WEBHOOK_SECRET, ...
ExecStart=/usr/local/bin/oxidd
Restart=on-failure
```

## 3. Security baseline (non-negotiables)

- **`OXID_API_TOKEN` must be set** whenever the daemon can be reached beyond
  loopback. Since v0.1.0 the daemon *refuses to start* otherwise — an open API
  can deploy, destroy and read secret names. Loopback binds stay open for
  local development; `OXID_ALLOW_OPEN_API=1` restores the old behavior
  explicitly if you really mean it.
- **TLS**: terminate at Traefik (compose default) or serve HTTPS directly with
  `OXID_TLS_CERT`/`OXID_TLS_KEY`.
- **Rate limiting**: set `OXID_RATE_LIMIT_PER_SECOND`/`OXID_RATE_LIMIT_BURST`
  on any daemon reachable by more than one client (per-IP token bucket).
- **`OXID_WEBHOOK_SECRET`**: without it every webhook is rejected. Use one
  secret across providers; GitHub/Gitea/Gogs verify HMAC-SHA256 signatures,
  GitLab compares its plain token constant-time.
- Never put the dashboard/API on the internet without the token; `/health`,
  `/wake`, `/heartbeat` and the webhook routes are the only unauthenticated
  paths, and each is safe-by-design (wake/heartbeat only touch Oxid-managed
  environments).

## 4. Team access: named tokens and project scoping

Handing the master token to every teammate means anyone can destroy anything.
Instead, mint named tokens — audit events are attributed to the operator's
name automatically:

```bash
# Full-access operator (same reach as master, but attributable):
oxid token create alice

# Scoped operator: may only act on projects 1 and 3:
oxid token create bob --project 1 --project 3
```

A scoped token gets `404` on any other project's endpoints (no existence
leak), sees only its projects in `oxid status`/audit/queue listings, and is
rejected from node-wide operations (registering/deleting projects, global
secrets, stats, infra, backups, token management). `oxid token list` shows
each token's scopes; revoke takes effect immediately.

Rule of thumb: humans get scoped tokens for their projects; CI gets either a
scoped token or the master token locked down at the network layer.

## 5. Deploying projects

```bash
oxid up main                      # deploy/register from the current directory
oxid status                       # branch / state / URL table
oxid logs feature-login -f        # live SSE log stream
oxid rollback main [--to <sha>]   # zero-downtime roll back
oxid pause/wake/down <branch>     # manual lifecycle control
oxid queue                        # deploys waiting on host capacity
```

Secrets resolve `Global → Project → Branch → Runtime` and are stored
AES-GCM-encrypted:

```bash
oxid env set DATABASE_URL=... --scope global
oxid env set STRIPE_KEY=sk_...   --scope project --project 1
```

Private repos: `oxid configure --git-token <PAT>` stores a per-project,
read-only token encrypted at rest; it is used only for the clone/fetch call
and never persisted anywhere else.

## 6. Backups & DR

- Periodic consistent snapshots (`VACUUM INTO`) into `{data}/backups/`:
  `OXID_BACKUP_INTERVAL_SECS` + `OXID_BACKUP_KEEP` (compose enables 300s/7).
- On-demand download: `oxid backup > oxid.tar`; restore stages the archive
  and applies it on the next daemon restart (`OXID_ALLOW_RESTORE=1` required).
- For surviving host loss entirely, run the Litestream sidecar documented in
  the compose file — SQLite streams WAL to S3-compatible storage.
- Restore drill: `docker compose down`, restore `audit.sqlite` + `secret.key`
  into the volume, `up -d`. Startup reconciliation diffs the database against
  real Docker state before serving requests, so containers that drifted while
  the daemon was gone are adopted or marked accordingly.

## 7. Upgrades

1. `oxid doctor` before and after — it flags CLI↔daemon major-version skew.
2. Swap the binary/image and restart. In-flight requests drain for up to 10s;
   deployed containers keep running (they carry `unless-stopped` restart
   policy) and are reconciled on startup.
3. Migrations apply automatically at startup; downgrades across migrations are
   not supported — snapshot first (`oxid backup`).

## 8. Health & observability

- Liveness/readiness probe: `GET /api/v1/health` (unauthenticated).
- Logs: `RUST_LOG=info` default; set `OXID_LOG_FORMAT=json` in production so a
  log aggregator can parse fields directly. Every request carries an
  `X-Request-Id`, echoed in responses, structured logs, and the audit trail.
- History: `oxid audit [--branch|--project|--since|--kind]`.
- Capacity: `oxid stats` (host memory/CPU vs committed environments).

## Production checklist

- [ ] `OXID_DOCKER_NETWORK` set (Traefik mode) — scale-to-zero actually works
- [ ] `OXID_API_TOKEN` set (daemon refuses to start without it off-loopback)
- [ ] `OXID_WEBHOOK_SECRET` set and configured in your Git host
- [ ] TLS terminated (Traefik or `OXID_TLS_CERT`/`OXID_TLS_KEY`)
- [ ] Rate limiting configured
- [ ] Operators on scoped named tokens; master token in a secret manager
- [ ] `OXID_BACKUP_INTERVAL_SECS` on (or Litestream sidecar running)
- [ ] Restore drill performed once before go-live
- [ ] `oxid doctor` green from an operator machine
