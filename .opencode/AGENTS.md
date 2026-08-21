# AGENTS.md — Oxid Agent Squad

> 20 agentes (13 primary + 7 subagent) + 5 built-in. Fuente de verdad: `.opencode/agents/*.md:1`. Tras editar, **reinicia opencode**.

## Primary (Tab para ciclar)

| Agente | Descripción | Cuándo usar | Invocación |
|--------|-------------|-------------|------------|
| `audit` | Forense exhaustivo — errores, fugas, nulos, perf, dead files, fugas negocio | Antes de merge, release, refactor grande | `@audit revisa PR #42` |
| `optimizer` | SRP, partir archivos >400L, nombres, DRY, incongruencias | Archivo hinchado, `store.rs` 800L | `@optimizer parte adapter/store.rs` |
| `ideator` | Ideas, diferenciadores, `oxid share`, `tunnel`, multiplex S3 | Brainstorming, roadmap H1/H2/H3 | `@ideator 10 ideas para potenciar Oxid` |
| `architect` | Hexagonal, `domain/ports.rs` traits, escala 100 ramas | Nueva feature, split dominio | `@architect diseña ResourcePool S3` |
| `designer` | `DESIGN.md` Industrial Elegance, paleta `#DE5236`, CLI `[+]/[~]/[>]` | Nueva UI, auditar `web/style.css` | `@designer audita dashboard` |
| `strategist` | `ROADMAP.md` RICE, H1/H2/H3, costo oportunidad | Qué construir este sprint | `@strategist prioriza que hacer ahora` |
| `sentinel` | HMAC, AES-GCM 0600, races, threat model | Exponer endpoint, tocar secretos | `@sentinel threat model /wake` |
| `ferrous` | `<15MB` RSS, wake <300ms, clones/allocs, `lto=true` | Perf, memoria, latencia GC | `@ferrous optimiza var_resolution.rs` |
| `reviewer` | PR review caveman 1 línea por comentario, `APPROVE/REQUEST CHANGES` | Cada PR, antes de merge | `@reviewer revisa diff main...HEAD` |
| `publisher` | Docker `ghcr.io`, 6 targets `release.yml`, `upload-rust-binary` | Shippear `vX.Y.Z` | `@publisher dry-run` / `@publisher release minor` |
| `automator` | `ci.yml`/`release.yml`, `.githooks`, `scheduler` GC, workflows | Fix CI rojo, nuevo workflow | `@automator crea preview.yml` |
| `documenter` | `README`/`ROADMAP`/`SPEC`/`DESIGN`/`docs/`/`MEMORY.md` al día | Tras feature/fix/release | `@documenter drift check` |
| `flow` | Orquesta `commit→pr→release→hotfix→docs` con `task` + `todowrite` | Shippear sin olvidar pasos | `@flow release minor --dry-run` |
| `tester` | `cargo test` workspace, `oxid-core` puro, boundary `grep tokio` en core | Coverage, verde total | `@tester coverage de gc.rs` |

## Subagentes ( @ para ahorrar tokens, 12-20 steps)

| Agente | Scope ultra | Ahorro | Invocación |
|--------|-------------|--------|------------|
| `crypt` | Solo `crypto.rs`, `secret.key 0600`, `OXID_MASTER_KEY` | <30 líneas, nunca leak | `@crypt verifica nonce crypto.rs:12` |
| `saver` | Caveman 65% menos tokens | <15 líneas | `@saver scan unwrap en api/handlers` |
| `lint` | `fmt --check` + `clippy -D warnings` + `grep unwrap` | 3 bash max | `@lint check` |
| `probe` | `grep`/`glob`/`read` solo lectura | <15 líneas, 4 calls max | `@probe donde está SecretStore` |
| `patcher` | 1 archivo / 1 bug / 1 fix | 3 calls, <15 líneas | `@patcher fix clone api/handlers/project.rs:42` |
| `versioner` | `Cargo.toml:6` bump + `Cargo.lock` + `CHANGELOG` + `tag vX.Y.Z` | 6 pasos | `@versioner bump patch` |

## Flujos recomendados

```bash
# Commit local
@flow commit  # probe→lint→tester→patcher→commit

# PR
@flow pr  # lint→tester→reviewer→audit→documenter

# Release (10 min end-to-end)
@flow release minor --dry-run  # todo verde?
@flow release minor --push     # versioner→publisher→documenter→automator

# Hotfix
@flow hotfix crates/oxid-daemon/src/api/handlers/webhook.rs "fix: HMAC bypass"

# Docs al máximo
@flow docs  # documenter drift → reviewer
@documenter  # solo docs

# Perf
@ferrous → @optimizer → @tester → @lint

# Seguridad
@sentinel → @crypt → @audit → @reviewer

# Ideación → estrategia → arquitectura
@ideator → @strategist → @architect → @designer
```

## Reglas globales

- Cada agente cita `file:line` y `SPEC.md:§`/`ROADMAP.md:#`/`DESIGN.md:§`/`Cargo.toml:6`/`release.yml:1`.
- `audit`/`reviewer`/`sentinel` nunca `edit` — solo reportan. `optimizer`/`publisher`/`automator`/`documenter`/`tester`/`flow` sí editan.
- `oxid-core` puro: `grep tokio|sqlx|bollard|axum` en `crates/oxid-core` debe 0 (ver `ci.yml:36` y `.githooks/_lib.sh`).
- Release profile `Cargo.toml:53-57` (`opt-level=3 lto=true codegen-units=1 strip panic=abort`) se respeta.
- Tras cualquier `edit`, `cargo fmt && cargo clippy -- -D warnings && cargo test --workspace`.

## Dónde viven

```
.opencode/agents/*.md      # 20 agentes (13 primary + 7 subagent en este proyecto)
~/.config/opencode/agents/ # opcional global (copia con cp .opencode/agents/* ~/.config/opencode/agents/)
.opencode/AGENTS.md        # este archivo
```

## Verificación

```bash
opencode agent list | grep -E "^\w+ \((primary|subagent)\)" | sort
# 20 agentes: 15 primary (13 custom + build/plan + compaction/summary/title), 10 subagent (5 custom + explore/general)
ls -1 .opencode/agents/ | wc -l  # 20
wc -l .opencode/agents/*.md | tail -1  # ~1880 líneas
```
