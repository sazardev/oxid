#!/usr/bin/env bash
# -----------------------------------------------------------------------------
# Oxid installer — one command from nothing to a running control plane.
#
#   Binaries only (CLI + daemon, no services):
#     curl -fsSL https://raw.githubusercontent.com/sazardev/oxid/main/install.sh | sh
#
#   Full native server (binaries + data dir + secrets + systemd + Traefik):
#     curl -fsSL .../install.sh | sh -s -- --server
#
#   Full docker stack (compose pulls the published image, Traefik included):
#     curl -fsSL .../install.sh | sh -s -- --docker
#
# Flags:
#   --server        native install: systemd service + auto-generated secrets
#   --docker        compose stack in ./oxid-stack (image: ghcr.io/sazardev/oxid)
#   --version TAG   pin a release (default: latest published)
#   --bindir DIR    binary target dir (default: /usr/local/bin, fallback ~/.local/bin)
#   --root DIR      sandbox prefix for /etc|/var|systemd paths (testing, containers)
#   --no-start      write everything but do not start/enable the service
#
# Re-running is safe: existing secrets are reused, never rotated.
# -----------------------------------------------------------------------------
set -euo pipefail

REPO="sazardev/oxid"
RELEASES="https://github.com/${REPO}/releases"
RAW="https://raw.githubusercontent.com/${REPO}"
MODE="binaries"
VERSION="${VERSION:-}"
BINDIR="${BINDIR:-}"
ROOT=""          # --root sandbox prefix ("" = real paths)
NO_START=0

log()  { printf '[+] %s\n' "$*"; }
warn() { printf '[~] %s\n' "$*"; }
die()  { printf '[!] %s\n' "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

rand_hex() {  # 64 hex chars without depending on openssl being present
  if have openssl; then openssl rand -hex 32
  else head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
  fi
}

as_root() {  # run a command as root only when we are not already root
  if [ "$(id -u)" = "0" ]; then "$@"
  elif have sudo; then sudo "$@"
  else "$@"; fi
}

usage() { sed -n '2,24p' "$0"; exit 0; }

# -----------------------------------------------------------------------------
# Finishing the job
#
# The point of these two: an installer that ends by telling you to run three
# more commands has not finished installing anything. The credentials it just
# generated are printed, not described — a devops who has to `grep` a `.env`
# to find the token the script created two seconds ago is doing the script's
# work for it.
#
# Printing a secret to a terminal is a deliberate trade. It is the operator's
# own machine, they just ran the installer as root, and the alternative is
# every one of them pasting a `docker compose logs | grep` from the README.
# The file it also lives in is written 0600.
# -----------------------------------------------------------------------------

# The address other machines can reach this host on, for the URLs a webhook
# and a teammate's CLI actually need. Falls back to the loopback address
# rather than a literal "DAEMON" nobody can paste anywhere.
reachable_host() {
  _ip="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p' | head -1)"
  [ -n "${_ip}" ] || _ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
  [ -n "${_ip}" ] || _ip="127.0.0.1"
  printf '%s' "${_ip}"
}

# Points the CLI at the daemon it just installed, so `oxid status` works
# without the operator ever seeing the token. Skipped silently when the CLI
# is not on this machine — a docker-only install is a perfectly normal
# choice.
configure_cli() {  # configure_cli API TOKEN
  have oxid || return 0
  oxid context add local --api "$1" --token "$2" >/dev/null 2>&1 || return 0
  # `add` stores the context; it does not select it. Without `use`, the CLI
  # falls back to no credentials and every command 401s against the daemon
  # this installer just set up — a context that exists and is never read.
  oxid context use local >/dev/null 2>&1 || return 0
  log "CLI configured — try: oxid doctor"
}


while [ $# -gt 0 ]; do
  case "$1" in
    --server)  MODE="server" ;;
    --docker)  MODE="docker" ;;
    --version) VERSION="${2:?--version needs a tag}"; shift ;;
    --bindir)  BINDIR="${2:?--bindir needs a dir}"; shift ;;
    --root)    ROOT="${2:?--root needs a dir}"; shift ;;
    --no-start) NO_START=1 ;;
    -h|--help) usage ;;
    *) die "unknown flag: $1 (see --help)" ;;
  esac
  shift
done

[ "$(uname -s)" = "Linux" ] || die "prebuilt binaries cover Linux (musl/gnu, x86_64/aarch64) and macOS — on macOS drop --server/--docker and install with --bindir, or use nix run github:${REPO}"
ARCH="$(uname -m)"
case "${ARCH}-${MODE}" in
  x86_64-*|amd64-*)  TARGET="x86_64-unknown-linux-musl" ;;
  aarch64-*|arm64-*) TARGET="aarch64-unknown-linux-musl" ;;
  *) die "unsupported arch: ${ARCH} (supported: x86_64, aarch64)" ;;
esac

have curl || have wget || die "need curl or wget to download releases"
fetch() {  # fetch URL DEST (DEST "-" = stdout)
  if [ "$2" = "-" ]; then
    if have curl; then curl -fsSL "$1"; else wget -qO- "$1"; fi
  elif have curl; then
    curl -fsSL "$1" -o "$2"
  else
    wget -qO "$2" "$1"
  fi
}

# -----------------------------------------------------------------------------
# resolve version
# -----------------------------------------------------------------------------
if [ -z "${VERSION}" ]; then
  log "resolving latest release…"
  VERSION="$(fetch "${RELEASES}/latest" - 2>/dev/null | grep -oE 'tag/[v0-9.]+' | head -1 | cut -d/ -f2 || true)"
  [ -n "${VERSION}" ] || die "cannot resolve latest release (offline? set --version v0.1.0)"
fi
log "installing Oxid ${VERSION} (${TARGET})"

# -----------------------------------------------------------------------------
# binaries
# -----------------------------------------------------------------------------
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
ASSET="oxid-${VERSION}-${TARGET}"
log "downloading ${ASSET}…"
fetch "${RELEASES}/download/${VERSION}/${ASSET}.tar.gz" "${WORK}/${ASSET}.tar.gz"
# taiki-e/upload-rust-binary-action publishes `<asset>.tar.gz.sha256` content
# under the name `<asset>.sha256` (the file references the tar.gz name).
fetch "${RELEASES}/download/${VERSION}/${ASSET}.sha256" "${WORK}/${ASSET}.sha256" \
  || warn "no checksum published for ${ASSET} — skipping verification"
if [ -f "${WORK}/${ASSET}.sha256" ]; then
  ( cd "${WORK}" && sha256sum -c "${ASSET}.sha256" >/dev/null 2>&1 ) \
    || die "checksum mismatch for ${ASSET}.tar.gz — aborting"
  log "checksum verified"
fi
tar -xzf "${WORK}/${ASSET}.tar.gz" -C "${WORK}"
[ -x "${WORK}/oxid" ] && [ -x "${WORK}/oxidd" ] || die "archive did not contain the expected binaries"

if [ -z "${BINDIR}" ]; then
  BINDIR="/usr/local/bin"
  if [ "$(id -u)" != "0" ] && ! as_root true 2>/dev/null; then BINDIR="${HOME}/.local/bin"; fi
fi
mkdir -p "${BINDIR}"
as_root install -m 0755 "${WORK}/oxid"  "${BINDIR}/oxid"
as_root install -m 0755 "${WORK}/oxidd" "${BINDIR}/oxidd"
log "installed: ${BINDIR}/oxid, ${BINDIR}/oxidd"

if [ "${MODE}" = "binaries" ]; then
  cat <<EOF

[+] Done. Talk to a daemon with:
      oxid context add prod --api http://YOUR-HOST:8080 --token TOKEN
      oxid doctor

    Run a daemon natively (systemd, secrets, Traefik — everything):
      ${0} --server

    Or as a docker stack (pulls ghcr.io/${REPO}):
      ${0} --docker
EOF
  exit 0
fi

# -----------------------------------------------------------------------------
# --server: native systemd service with auto-generated secrets
# -----------------------------------------------------------------------------
ETC="${ROOT}/etc/oxid"
DATA="${ROOT}/var/lib/oxid"
UNIT_DIR="${ROOT}/etc/systemd/system"

if [ "${MODE}" = "server" ]; then
  ENV_FILE="${ETC}/oxidd.env"
  as_root mkdir -p "${ETC}" "${DATA}"

  if as_root grep -q '^OXID_API_TOKEN=' "${ENV_FILE}" 2>/dev/null; then
    warn "reusing existing secrets in ${ENV_FILE} (re-run never rotates them)"
  else
    API_TOKEN="$(rand_hex)"
    WEBHOOK_SECRET="$(rand_hex)"
    as_root tee "${ENV_FILE}" >/dev/null <<EOF
# Generated by oxid install.sh — secrets: 0600, do not commit.
OXID_DATA_DIR=${DATA}
OXID_ADDR=0.0.0.0:8080
OXID_API_TOKEN=${API_TOKEN}
OXID_WEBHOOK_SECRET=${WEBHOOK_SECRET}
# Supported production topology: Traefik routing + scale-to-zero.
OXID_DOCKER_NETWORK=oxid-net
# How Traefik (inside the network) reaches this daemon for /wake + /heartbeat.
OXID_DAEMON_URL=http://172.17.0.1:8080
OXID_BACKUP_INTERVAL_SECS=300
OXID_BACKUP_KEEP=7
OXID_LOG_FORMAT=json
EOF
    log "wrote ${ENV_FILE} (fresh API token + webhook secret)"
  fi
  as_root chmod 0600 "${ENV_FILE}"

  UNIT="${UNIT_DIR}/oxidd.service"
  as_root mkdir -p "$(dirname "${UNIT}")"
  as_root tee "${UNIT}" >/dev/null <<EOF
[Unit]
Description=Oxid control-plane daemon (branch preview environments)
After=network-online.target docker.service
Wants=network-online.target

[Service]
EnvironmentFile=${ENV_FILE}
ExecStart=${BINDIR}/oxidd
Restart=on-failure
RestartSec=2
LimitNOFILE=65535
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF
  log "wrote ${UNIT}"

  if [ "${NO_START}" = "1" ] || [ -n "${ROOT}" ]; then
    warn "--root/testing mode: skipping systemctl (start manually)"
  else
    as_root systemctl daemon-reload
    as_root systemctl enable --now oxidd
    log "waiting for the daemon to come up…"
    API_TOKEN="$(as_root grep '^OXID_API_TOKEN=' "${ENV_FILE}" | cut -d= -f2)"
    for _ in $(seq 1 30); do
      if curl -fsS http://127.0.0.1:8080/api/v1/health >/dev/null 2>&1; then break; fi
      sleep 1
    done
    if curl -fsS http://127.0.0.1:8080/api/v1/health >/dev/null 2>&1; then
      log "daemon is healthy"
      # Bootstrap Traefik + the shared docker network (idempotent).
      BOOTSTRAP_OK=0
      for _ in 1 2 3; do
        if curl -fsS -X POST http://127.0.0.1:8080/api/v1/infra/bootstrap \
             -H "Authorization: Bearer ${API_TOKEN}" >/dev/null 2>&1; then
          BOOTSTRAP_OK=1
          break
        fi
        sleep 3
      done
      [ "${BOOTSTRAP_OK}" = "1" ] \
        && log "Traefik + docker network bootstrapped (scale-to-zero ready)" \
        || warn "infra bootstrap failed — run later: oxid infra setup"
    else
      warn "daemon did not answer /health within 30s — check: journalctl -u oxidd -e"
    fi
  fi

  WEBHOOK_SECRET="$(as_root grep '^OXID_WEBHOOK_SECRET=' "${ENV_FILE}" | cut -d= -f2)"
  HOST_ADDR="$(reachable_host)"
  configure_cli "http://127.0.0.1:8080" "${API_TOKEN}"

  cat <<EOF

[+] Oxid is installed. Everything below is ready to use — nothing else to run.

    Dashboard    http://${HOST_ADDR}:8080/
    API token    ${API_TOKEN}
                 (paste into the dashboard's token box — the CLI on this
                  machine is already configured with it)

    CLI          oxid doctor          (verifies the token) — then oxid ps
    Deploy       oxid up <branch>     (from a git checkout)

    Webhook      http://${HOST_ADDR}:8080/api/v1/webhooks/github
    Secret       ${WEBHOOK_SECRET}

    Credentials live in ${ENV_FILE} (0600).
    Data/backups ${DATA}   (snapshots every 300s in ${DATA}/backups)
    Logs         journalctl -u oxidd -f

    TLS: terminate at Traefik (already running) or set OXID_TLS_CERT/KEY in
    ${ENV_FILE} and 'systemctl restart oxidd'. Full guide: PRODUCTION.md
EOF
  exit 0
fi

# -----------------------------------------------------------------------------
# --docker: compose stack from the published image, nothing cloned
# -----------------------------------------------------------------------------
if [ "${MODE}" = "docker" ]; then
  have docker || die "docker is required for --docker mode"
  docker compose version >/dev/null 2>&1 || die "docker compose plugin is required"

  STACK="${ROOT:-.}/oxid-stack"
  mkdir -p "${STACK}"
  # The compose file comes from main, not the pinned tag: it is the stack
  # recipe the installer understands (tags older than the installer itself
  # shipped a build-from-source compose). The *binaries* stay pinned to
  # VERSION via the published image tag below.
  fetch "${RAW}/main/docker-compose.yml" "${STACK}/docker-compose.yml"
  # ghcr tags are bare semver (metadata-action `{{version}}`): v0.1.0 → 0.1.0.
  # raw.githubusercontent.com caches aggressively, so handle BOTH compose
  # shapes (image-first and older build-from-source) and verify the result.
  IMG_TAG="${VERSION#v}"
  if grep -qE "^    image: ghcr\.io/${REPO}:" "${STACK}/docker-compose.yml"; then
    sed -i "s|^    image: ghcr\.io/${REPO}:.*|    image: ghcr.io/${REPO}:${IMG_TAG}|" "${STACK}/docker-compose.yml"
  else
    sed -i "s|^    build: \.$|    image: ghcr.io/${REPO}:${IMG_TAG}|" "${STACK}/docker-compose.yml"
  fi
  grep -q "image: ghcr.io/${REPO}:${IMG_TAG}" "${STACK}/docker-compose.yml" \
    || die "could not pin ghcr.io/${REPO}:${IMG_TAG} in the compose file"

  if [ -f "${STACK}/.env" ] && grep -q '^OXID_API_TOKEN=' "${STACK}/.env"; then
    warn "reusing existing ${STACK}/.env (re-run never rotates secrets)"
  else
    cat > "${STACK}/.env" <<EOF
# Generated by oxid install.sh — secrets: do not commit.
OXID_API_TOKEN=$(rand_hex)
OXID_WEBHOOK_SECRET=$(rand_hex)
EOF
    chmod 0600 "${STACK}/.env"
    log "wrote ${STACK}/.env (fresh API token + webhook secret)"
  fi

  ( cd "${STACK}" && docker compose up -d )
  log "waiting for the daemon…"
  API_TOKEN="$(grep '^OXID_API_TOKEN=' "${STACK}/.env" | cut -d= -f2)"
  for _ in $(seq 1 60); do
    if curl -fsS http://127.0.0.1:8080/api/v1/health >/dev/null 2>&1; then break; fi
    sleep 1
  done
  if curl -fsS http://127.0.0.1:8080/api/v1/health >/dev/null 2>&1; then
    log "daemon is healthy"
    BOOTSTRAP_OK=0
    for _ in 1 2 3; do
      if curl -fsS -X POST http://127.0.0.1:8080/api/v1/infra/bootstrap \
           -H "Authorization: Bearer ${API_TOKEN}" >/dev/null 2>&1; then
        BOOTSTRAP_OK=1
        break
      fi
      sleep 3
    done
    [ "${BOOTSTRAP_OK}" = "1" ] \
      && log "Traefik + network verified (scale-to-zero ready)" \
      || warn "infra bootstrap failed — run: cd ${STACK} && docker compose restart oxid-daemon"
  else
    warn "daemon not healthy yet — check: cd ${STACK} && docker compose logs oxid-daemon"
  fi

  WEBHOOK_SECRET="$(grep '^OXID_WEBHOOK_SECRET=' "${STACK}/.env" | cut -d= -f2)"
  HOST_ADDR="$(reachable_host)"
  configure_cli "http://127.0.0.1:8080" "${API_TOKEN}"

  # The URLs say `127.0.0.1` because that is where this stack actually
  # listens: the compose file publishes `127.0.0.1:8080:8080`, so nothing
  # off this host can reach the control API. Printing the LAN address would
  # be friendlier and wrong — the commands would not work, and the webhook
  # URL would be one a Git host can never deliver to.
  cat <<EOF

[+] Oxid is up. Everything below is ready to use — nothing else to run.

    Dashboard    http://127.0.0.1:8080/     (on this machine)
    API token    ${API_TOKEN}
                 (paste into the dashboard's token box — the CLI on this
                  machine is already configured with it)

    CLI          oxid doctor          (verifies the token) — then oxid ps
    Deploy       oxid up <branch>     (from a git checkout; its `origin` is
                 what gets registered, since this daemon runs in a container
                 and cannot see your working tree)

    Webhook      secret: ${WEBHOOK_SECRET}
                 The control API is published on 127.0.0.1 only, so a Git
                 host cannot reach it yet. To let it: change the ports line
                 in ${STACK}/docker-compose.yml to "8080:8080", run
                 'docker compose up -d', and point the webhook at
                 http://${HOST_ADDR}:8080/api/v1/webhooks/github

    Credentials live in ${STACK}/.env (0600). Upgrade with:
      cd ${STACK} && docker compose pull && docker compose up -d
EOF
  exit 0
fi
