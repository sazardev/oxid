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

  cat <<EOF

[+] Oxid server is installed.

    Control API : http://127.0.0.1:8080 (locally) — expose http://SERVER-IP:8080
    CLI access  : oxid context add prod --api http://DAEMON:8080 \\
                    --token \$(sudo grep ^OXID_API_TOKEN ${ENV_FILE} | cut -d= -f2)
                  oxid doctor
    Webhooks    : point your Git host at http://DAEMON:8080/api/v1/webhooks/github
                  secret: \$(sudo grep ^OXID_WEBHOOK_SECRET ${ENV_FILE} | cut -d= -f2)
    Data/backups: ${DATA}   (snapshots every 300s in ${DATA}/backups)
    Logs        : journalctl -u oxidd -f

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

  cat <<EOF

[+] Oxid docker stack is up in ${STACK}.

    Dashboard  : http://DAEMON:8080/  — open it and follow the setup wizard
                 (token → infra → first project → webhooks → CLI)
    Token      : if the stack generated one (OXID_AUTO_TOKEN), read it with
                   cd ${STACK} && docker compose logs oxid-daemon | grep -A2 Generated
                 (or your own OXID_API_TOKEN from ${STACK}/.env)
    CLI access : oxid context add prod --api http://DAEMON:8080 \\
                   --token \$(grep ^OXID_API_TOKEN ${STACK}/.env | cut -d= -f2)
                 oxid doctor
    Webhooks   : http://DAEMON:8080/api/v1/webhooks/github
                 secret: \$(grep ^OXID_WEBHOOK_SECRET ${STACK}/.env | cut -d= -f2)
    Upgrade    : cd ${STACK} && docker compose pull && docker compose up -d
EOF
  exit 0
fi
