#!/usr/bin/env bash
# Shared helpers for .githooks/*. Not a hook itself.

C_RED='\033[0;31m'
C_GREEN='\033[0;32m'
C_YELLOW='\033[0;33m'
C_DIM='\033[2m'
C_RESET='\033[0m'

info()  { printf "%b[~]%b %s\n" "$C_DIM" "$C_RESET" "$1"; }
ok()    { printf "%b[+]%b %s\n" "$C_GREEN" "$C_RESET" "$1"; }
warn()  { printf "%b[!]%b %s\n" "$C_YELLOW" "$C_RESET" "$1"; }
fail()  { printf "%b[x]%b %s\n" "$C_RED" "$C_RESET" "$1"; }

# die MESSAGE — print and exit non-zero (aborts the hook -> aborts the git action).
die() {
    fail "$1"
    echo
    echo "Bypass (not recommended): git commit/push --no-verify"
    exit 1
}

repo_root() {
    git rev-parse --show-toplevel
}

# Rust source/manifest files changed vs. HEAD in the given ref range (or staged, if no args).
rust_files_changed() {
    if [ "$#" -eq 0 ]; then
        git diff --cached --name-only --diff-filter=ACM
    else
        git diff --name-only "$1" "$2" --diff-filter=ACM
    fi | grep -E '(\.rs$|Cargo\.toml$|Cargo\.lock$)' || true
}

# Custom architecture rule: oxid-core is pure domain, no I/O deps allowed
# (CLAUDE.md / SPEC.md §2.1 — "Keep oxid-core free of any I/O, SQL, Docker,
# or HTTP dependency"). Cheap, deterministic, catches an easy-to-miss
# layering violation before it's baked into a PR.
check_hexagonal_boundary() {
    local core_manifest="$1/crates/oxid-core/Cargo.toml"
    [ -f "$core_manifest" ] || return 0

    local forbidden="tokio|sqlx|bollard|axum|reqwest|git2|hyper|tower|tar\\b"
    local hit
    hit=$(grep -inE "^($forbidden)[[:space:]]*(\\.workspace)?[[:space:]]*=" "$core_manifest" || true)
    if [ -n "$hit" ]; then
        fail "oxid-core must stay free of I/O dependencies (hexagonal boundary, see SPEC.md §2.1):"
        echo "$hit"
        return 1
    fi
    return 0
}

# Very small, dependency-free secret scanner over a diff (stdin). Not a
# substitute for gitleaks (used automatically when installed) but a useful
# floor for contributors who don't have it.
scan_diff_for_secrets() {
    local diff_content
    diff_content=$(cat)
    [ -z "$diff_content" ] && return 0

    local patterns=(
        '-----BEGIN [A-Z ]*PRIVATE KEY-----'
        'AKIA[0-9A-Z]{16}'
        'AIza[0-9A-Za-z_-]{35}'
        'ghp_[0-9A-Za-z]{36}'
        'gh[opsu]_[0-9A-Za-z]{36,}'
        'xox[baprs]-[0-9A-Za-z-]{10,}'
        'OXID_MASTER_KEY[[:space:]]*=[[:space:]]*[0-9a-fA-F]{64}'
    )

    local found=0
    for pat in "${patterns[@]}"; do
        if echo "$diff_content" | grep -qE "^\+.*($pat)"; then
            fail "possible secret matching pattern: $pat"
            found=1
        fi
    done
    return $found
}

check_forbidden_paths() {
    local files
    files=$(git diff --cached --name-only --diff-filter=ACM)
    local bad
    bad=$(echo "$files" | grep -E '(^|/)(\.env|secret\.key|id_rsa|id_ed25519|.*\.pem|.*\.p12|.*\.pfx)$' || true)
    if [ -n "$bad" ]; then
        fail "staged file(s) look like local secrets/credentials, not source code:"
        echo "$bad"
        return 1
    fi
    return 0
}

check_merge_conflict_markers() {
    local files
    files=$(git diff --cached --name-only --diff-filter=ACM | grep -E '\.rs$|\.toml$|\.md$' || true)
    [ -z "$files" ] && return 0
    local hit=0
    while IFS= read -r f; do
        [ -f "$f" ] || continue
        if grep -qE '^(<{7}|={7}|>{7})( |$)' "$f"; then
            fail "merge conflict marker left in: $f"
            hit=1
        fi
    done <<< "$files"
    return $hit
}

check_large_files() {
    local max_kb=5120 # 5 MiB
    local files
    files=$(git diff --cached --name-only --diff-filter=ACM)
    [ -z "$files" ] && return 0
    local hit=0
    while IFS= read -r f; do
        [ -f "$f" ] || continue
        local size_kb
        size_kb=$(( $(wc -c < "$f") / 1024 ))
        if [ "$size_kb" -gt "$max_kb" ]; then
            fail "staged file is ${size_kb}KiB, over the ${max_kb}KiB guardrail: $f"
            hit=1
        fi
    done <<< "$files"
    return $hit
}
