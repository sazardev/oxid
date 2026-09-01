# Running Oxid in production

The supported way to run Oxid for real work is **CLI-first**: one daemon
(`oxidd`) on a Docker host, operators driving it with `oxid`. The embedded web
dashboard ships in the same binary at no extra cost, but nothing below
requires opening it to the world.

**One control plane, any number of nodes.** Oxid runs environments on more
than one machine (see [Scaling past one machine](#9-scaling-past-one-machine)),
but the control plane itself is single — it does not promise HA. Disaster
recovery is snapshots, not replication (see [Backups & DR](#backups--dr)).

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

## 2. Install — one command

### Docker stack (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/sazardev/oxid/main/install.sh | sh -s -- --docker
```

That single command: verifies the checksum of the released CLI, generates
`OXID_API_TOKEN` + `OXID_WEBHOOK_SECRET` into `./oxid-stack/.env` (0600,
never rotated on re-run), pulls `ghcr.io/sazardev/oxid`, starts daemon +
Traefik, waits for health, and verifies the network/proxy wiring. Re-running
is always safe.

Registering projects on the containerized daemon: the easy path is
registration **by Git URL** — no shared filesystem, the daemon clones into
its own cache (private repos take an encrypted access token):

```bash
curl -X POST http://127.0.0.1:8080/api/v1/projects \
  -H "Authorization: Bearer $(grep ^OXID_API_TOKEN oxid-stack/.env | cut -d= -f2)" \
  -H "Content-Type: application/json" \
  -d '{"repo_url": "https://github.com/you/app.git"}'
# scp-style remotes work too: "repo_url": "git@github.com:you/app.git"
```

Or from a checkout under the mounted `./repos` directory:

```bash
# clone the repo into ./oxid-stack/repos/<name> first, then:
curl -X POST http://127.0.0.1:8080/api/v1/projects \
  ... -d '{"repo_dir": "/repos/<name>"}'
```

The dashboard's setup wizard (`/ui/onboarding`, also linked as *setup* in the
top bar) walks through all of this — token, infra bootstrap, first project +
deploy, webhook URL/secret, CLI snippet — and opens automatically on a fresh
install. After that, webhook pushes deploy like anywhere else. (Shared
Postgres/Redis pooling: add `OXID_POSTGRES_URL`/`OXID_REDIS_URL` to the
stack's compose `environment:` — hostnames must resolve from inside the
`oxid-net` network.)

### Native systemd server

```bash
curl -fsSL https://raw.githubusercontent.com/sazardev/oxid/main/install.sh | sh -s -- --server
```

Installs both binaries to `/usr/local/bin`, writes `/etc/oxid/oxidd.env`
(secrets, 0600) and `/etc/systemd/system/oxidd.service`, starts the service,
waits for `/health`, and bootstraps Traefik + the docker network. Logs:
`journalctl -u oxidd -f`. Data + backups: `/var/lib/oxid`.

### Manual (what the installer automates)

[`docker-compose.yml`](docker-compose.yml) is the reference stack: daemon +
Traefik pre-wired for wake-on-request, socket mounted, `/data` volume,
periodic backups enabled — it ships with `OXID_AUTO_TOKEN=1`, so a plain
`docker compose up -d` works with no `.env` at all (the generated
`OXID_API_TOKEN`/`OXID_WEBHOOK_SECRET` are printed once to the logs and
persisted under `/data`). Pin your own secrets instead by setting both env
vars explicitly (`cp .env.example .env`).

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
- **`OXID_BOOTSTRAP_TOKEN_ACCESS`**: `GET /api/v1/setup/token` hands the
  auto-generated master token to a caller *before* authentication, so the
  onboarding wizard can finish. The shipped compose sets `off`, because it
  publishes the port on every interface — a containerized daemon sees every
  caller arrive from the bridge gateway, so `loopback` would not mean
  loopback there and `any` would hand the master token to whoever reached
  the port first. Leave it `off` unless the port is genuinely private.
- **The port is published on every interface by default** (since v0.4.0), so
  your team's CLIs and your Git host can reach it without editing anything.
  The port was never the boundary — the credential is. Narrow it back to
  `127.0.0.1:8080:8080` if only this host ever talks to the daemon.
- Never put the dashboard/API on the internet without the token; `/health`,
  `/wake`, `/heartbeat` and the webhook routes are the only unauthenticated
  paths, and each is safe-by-design (wake/heartbeat only touch Oxid-managed
  environments).

## 4. Team access: roles, scopes and expiries

Handing the master token to every teammate means anyone can destroy anything.
Instead, issue a credential that says what a person may do, where, and until
when — audit events are attributed to their name automatically:

```bash
oxid token create juan  --project 1 --role developer  --expires-in 90d
oxid token create ana   --project 1 --role viewer
oxid token create ops2               --role admin      # can issue access too
oxid token create ci    --project 1 --role developer   # deploys, no secrets

oxid token list          # role, status, scope, expiry
oxid token suspend 2     # reversible — someone on leave
oxid token resume 2
oxid token revoke 2      # permanent
```

| Role | Can | Cannot |
|---|---|---|
| `viewer` | read projects, environments, logs, history | change anything |
| `developer` | deploy, roll back, pause, wake, destroy environments | secrets, project settings |
| `maintainer` | its projects' secrets, settings, branch rules, deletion | anything node-wide |
| `admin` | the node, and issuing access to others | rotate the master key, read the webhook secret |

Four rules are worth knowing before you hand anything out:

- **Scope beats role, and out-of-scope is a `404`.** Another project's
  endpoints do not confirm they exist, and a scoped credential sees only its
  own projects in `status`/audit/queue listings.
- **A scoped credential is never node-wide, whatever its role.** "Admin of
  project 3" is not an admin of the server, and a *global* secret — injected
  into every project's deploys — counts as node-wide.
- **Omitting `--role` keeps pre-0.4 behaviour** (`maintainer` when scoped,
  `admin` when not), so upgrading removes nobody's permissions. Least
  privilege is something you ask for; `token create` prints what it granted.
- **Rotating the master key and reading the webhook secret stay master-only.**
  An admin is a role, and roles are things admins hand out.

Rule of thumb: developers get `--role developer` scoped to their projects and
an expiry; whoever runs the server gets `admin`; CI gets a scoped
`developer`, since it ships code and does not need to read secrets.

On the receiving end, a teammate runs `oxid login http://DAEMON:8080` (the
token is read from stdin, not left in shell history) and `oxid whoami` to see
what they may do. See `docs/docs/developers.html`.

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

## 9. Scaling past one machine

Oxid is **one control plane over N Docker endpoints**. The daemon keeps the
database, the git cache, the secrets and the audit trail; a node is a Docker
API plus an address, and nothing runs on it but the containers Oxid starts.
There is no agent to install and no cluster to join.

### Register a node

On the node, expose Docker over mTLS (Docker's own
[protect-access](https://docs.docker.com/engine/security/protect-access/)
guide generates the material; the short version):

```bash
# on the node — creates ca.pem, server-cert.pem, server-key.pem,
# cert.pem and key.pem
dockerd --tlsverify \
        --tlscacert=ca.pem \
        --tlscert=server-cert.pem \
        --tlskey=server-key.pem \
        -H tcp://0.0.0.0:2376
```

Copy `ca.pem`, `cert.pem` and `key.pem` to **the control plane's** disk — the
node row stores the *paths*, and the daemon reads them — then:

```bash
oxid node add eu-1 tcp://10.0.0.4:2376 \
  --address 10.0.0.4 \
  --tls-ca /etc/oxid/nodes/eu-1/ca.pem \
  --tls-cert /etc/oxid/nodes/eu-1/cert.pem \
  --tls-key /etc/oxid/nodes/eu-1/key.pem
oxid node ls
```

`--address` is not the same thing as the endpoint, and getting it wrong is
the most common way this goes subtly wrong. The endpoint is where the Docker
API lives; the address is where the control plane's proxy dials the *ports
that node publishes*. They are frequently different interfaces. Omit it and
Oxid dials loopback — its own machine — so the branch deploys successfully
and is unreachable. The post-deploy readiness probe uses the address, so a
mistyped one fails the deploy honestly rather than reporting green.

A remote endpoint with no TLS material is refused. A Docker socket over plain
TCP is root on that machine for anyone who can route to it, and mTLS is the
only thing bounding who that is. `OXID_ALLOW_INSECURE_NODES=1` overrides it,
following the precedent `OXID_ALLOW_OPEN_API` set — use it on a network you
control end to end, and nowhere else.

### Routing across nodes

Traefik's Docker provider only ever sees the socket it is reading, so it
cannot learn about a container on another machine. Point it at the daemon as
well:

```yaml
# docker-compose.yml, traefik service
command:
  # ... everything already there stays ...
  - --providers.http.endpoint=http://oxid-daemon:8080/api/v1/traefik/config
  - --providers.http.headers.Authorization=Bearer ${OXID_API_TOKEN}
  - --providers.http.pollInterval=5s
```

Add it, do not swap it: both providers run together, and the Docker one keeps
routing everything it routes today. The HTTP one supplies what labels
structurally cannot — environments on other nodes, and environments whose
container is stopped. That second one is a bonus: a sleeping branch now has a
router built from its database row, so `oxid-wake-catchall` stops being the
only thing that can wake it. Keep the catch-all anyway; it is still the wake
path in direct-publish mode.

### Placement, draining and failure

- **Placement** is affinity first, then most free memory. A redeploy stays on
  the node it is already on, because images are not distributed — each node
  builds its own — so a branch that moves rebuilds from scratch.
- **Draining** (`oxid node drain eu-1`) stops new environments landing there
  and touches nothing already running. `--evacuate` additionally moves every
  live branch off, one redeploy each through the ordinary zero-downtime path
  (build, wait for ready, cut over, then remove). It rebuilds each branch at
  **the commit it is running**, never at its current head.
- **A node that stops answering** is marked `down` by the health probe and
  receives no new work. Its environments are left exactly as they are —
  nothing is moved, marked destroyed or rebuilt. A network partition is
  indistinguishable from a dead machine from the control plane, and acting on
  one is how two live copies of a branch end up fighting over a URL. A node
  that answers again rejoins on its own, with no restart.
- **Removing a node** is refused while any environment still points at it,
  destroyed ones included: the audit trail hangs off environment rows, so
  freeing the node would delete that history as a side effect.

### The two costs, stated plainly

1. **The control plane is in the data path.** Traffic for a branch on a remote
   node goes Traefik → the daemon's per-branch proxy → the node. Restarting
   `oxidd` therefore cuts in-flight connections to *remote* environments and
   stalls new ones until the accept loops rebind at startup. On a single node
   this is unchanged: Traefik reaches the container directly. If control-plane
   bandwidth or its restart window becomes your limit, that is the point at
   which a per-node agent would earn its complexity — the design leaves room
   for one, and does not ship it before it is needed.
2. **The control plane is a single point of failure.** Running environments
   keep serving (containers carry `unless-stopped`, and Traefik is its own
   container). What stops is deploying, waking, the GC, the API, and — per
   the point above — cross-node traffic. Recovery is restore-and-restart; see
   [Backups & DR](#backups--dr).

Backups snapshot the SQLite file. That brings back the node **rows** and not
the certificate files they name — those stay your responsibility, alongside
`secret.key`.

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
- [ ] Multi-node only: every node registered with `--address` **and** TLS paths
- [ ] Multi-node only: Traefik's `--providers.http.*` wired (`oxid infra status` confirms)
- [ ] Multi-node only: node certificate files backed up separately from the database
