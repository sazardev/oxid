# ROADMAP.md: Gap Analysis — Código vs Documentos

> Análisis granular de lo que existe en código vs lo que prometen
> `IDEA.md`, `SPEC.md` y `DESIGN.md`. Cada tarea incluye una cita
> literal del documento fuente que la sustenta.

---

## Mapa de Referencias de Documentos

| Documento | Secciones clave |
|---|---|
| **IDEA.md** | §2 La Gran Idea · §3 Filosofía · §4 Interfaces · §5 Marketing · §6 Golden Path · oxid.toml spec |
| **SPEC.md** | §1 Principios · §2 Arquitectura · §3 Ahorro de recursos · §4 Pipeline · §5 Interfaces · §6 Self-Hosting |
| **DESIGN.md** | §1 Colores · §2 Tipografía · §3.1 Estados · §3.2 Dashboard · §3.3 CLI · §3.4 TUI · §4 Iconografía · §5 Tone of Voice |

---

## 1. CLI (`oxid-cli`)

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 1.1 | `oxid down <branch>` — destruir entorno | **SPEC §5.1:** _"`$ ctrl up my-repo --branch feature-login` → Fuerza un despliegue manual."_ Se infiere `down` como operación opuesta. | `cli/main.rs` — `oxid down <branch> [--force]`, resuelve rama → entorno y llama `DELETE /environments/{id}` | ✅ Done |
| 1.2 | `oxid status` — listar entornos del proyecto actual | **IDEA §4:** _"Comandos cortos, salida coloreada e intuitiva. Ej: `oxid up feature-login`, `oxid env set`."_ | `cli/main.rs` — `oxid status` lista rama/estado/URL coloreado por estado (DESIGN §3.1) | ✅ Done |
| 1.3 | `oxid logs <branch> -f` — streaming real vía SSE/chunked | **SPEC §5.1:** _"`$ ctrl logs feature-login -f` → Streaming de logs de la rama."_ | `api/handlers/lifecycle.rs` expone `/api/v1/environments/{id}/logs/stream` (SSE real) y `cli/main.rs::cmd_logs -f` lo consume vía `bytes_stream`; verificado en vivo contra un contenedor que emite 1 línea/seg | ✅ Done |
| 1.4 | `oxid env set KEY=val --scope global` — inyectar secretos | **SPEC §5.1:** _"`$ ctrl env set my-repo DB_PASSWORD=secret --scope global` → Inyecta variables."_ | `cli/main.rs` — `oxid env set KEY=VAL --scope <global\|project\|branch> [--project ID] [--branch B]` | ✅ Done (a0c064d) |
| 1.5 | `oxid env list` — ver secretos configurados | **IDEA §4:** La tabla de interfaces muestra `oxid env set` como ejemplo de la CLI. `list` es el complemento natural. | `cli/main.rs` — `oxid env list [--scope ...]` (valores nunca expuestos) | ✅ Done (a0c064d) |
| 1.6 | `oxid pause <branch>` — pausar manualmente | **IDEA §6:** _"Oxid apagó el contenedor, liberando 500MB de RAM."_ El usuario debe poder forzar esta acción. | `cli/main.rs` — `oxid pause <branch>` | ✅ Done |
| 1.7 | `oxid wake <branch>` — despertar manualmente | **IDEA §6:** _"Un tester entra a la URL, Oxid lo nota, hace `unpause` en 200ms."_ El usuario debe poder forzar esta acción. | `cli/main.rs` — `oxid wake <branch>` | ✅ Done |
| 1.8 | Coloreado ANSI con prefijos `[+]` `[~]` `[>]` | **DESIGN §3.3:** _"`[+]` in Patina Green for success. `[~]` in Ash Gray for background tasks. `[>]` in Oxid Orange for actionable prompts."_ | `cli/main.rs` — helpers `ok`/`bg`/`action`/`error` con ANSI | ✅ Done (a0c064d) |
| 1.9 | Flag `--force` para sobreescribir | **DESIGN §5:** _"Offer `[--force]` flags, but clearly state what they overwrite."_ | `cli/main.rs` — `oxid down --force` salta la confirmación interactiva de destrucción | ✅ Done |
| 1.10 | Flag `--api` para configurar daemon URL | **SPEC §6:** La sección de configuración sugiere que el sistema debe ser configurable. Actualmente hardcodea `OXID_API`. | `cli/main.rs` — flag global `--api <url>`, tiene prioridad sobre `OXID_API` | ✅ Done |

---

## 2. Webhook y Seguridad

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 2.1 | Verificación HMAC-SHA256 de webhooks GitHub | **SPEC §4.1:** _"axum recibe un push webhook de GitHub/GitLab. Se verifica el payload criptográficamente."_ | `api/handlers/webhook.rs` — `verify_hmac` + `X-Hub-Signature-256` (comparación constante vía `hmac`) | ✅ Done (a0c064d) |
| 2.2 | Secret configurable (`OXID_WEBHOOK_SECRET`) | **SPEC §4.1:** implícito en _"Se verifica el payload criptográficamente"_. HMAC requiere un shared secret. | `main.rs` lee `OXID_WEBHOOK_SECRET`; webhooks rechazados si no está configurado | ✅ Done (a0c064d) |
| 2.3 | Soporte webhooks GitLab (formato distinto) | **SPEC §4.1:** _"axum recibe un push webhook de GitHub/GitLab."_ Menciona ambos proveedores. | `api/handlers/webhook.rs` — núcleo común `handle_push` + tres handlers: `/webhooks/github` (HMAC `X-Hub-Signature-256`), `/webhooks/gitlab` (token plano `X-Gitlab-Token` comparado constant-time, branch borrada = `after` null-SHA, otros `object_kind` ignorados), `/webhooks/gitea` + `/webhooks/gogs` (HMAC hex sin prefijo). Un solo `OXID_WEBHOOK_SECRET`; 13 tests de webhook | ✅ Done |
| 2.4 | Rate limiting en la API HTTP | **SPEC §1:** _"Ecosistema Unificado: No requiere herramientas de terceros."_ Implica protección integrada. | Ya existía (`OXID_RATE_LIMIT_PER_SECOND`/`BURST`, `GovernorLayer` sobre rutas protegidas, test de burst) — el ROADMAP estaba desactualizado. Ahora con bucket **por client IP**: nuevo `ClientIpKeyExtractor` (fallback a bucket compartido cuando no hay `ConnectInfo`, p.ej. tests) y `into_make_service_with_connect_info` cableado en ambos serve paths (HTTP y TLS) de `main.rs`. Tras un reverse proxy único degrada a global (dirección segura; XFF no se confía por spoofable) | ✅ Done (mejorado a per-IP) |
| 2.5 | Autenticación API (bearer token mínimo) | **SPEC §6:** La configuración incluye _"tokens"_ en `/data/config.toml`. | `OXID_API_TOKEN` + middleware `Authorization: Bearer` (comparación en tiempo constante) sobre todo `/api/v1/*` salvo `/health`, `/webhooks/*`, `/wake`, `/heartbeat`. CLI: `--token`/`OXID_TOKEN` | ✅ Done |

---

## 3. Secretos e Inyección de Variables

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 3.1 | Almacenamiento encriptado de secretos (AES-GCM) | **SPEC §4.4:** _"Se compilan los secretos cacheados en disco (encriptados mediante AES-GCM usando una clave maestra local) y se inyectan dinámicamente."_ | `adapter/crypto.rs` (`Cipher` AES-GCM) + `SecretStore` en `SqliteStore` — todo valor cifrado en reposo; rotación en caliente vía `rotate_master_key` | ✅ Done (a0c064d) |
| 3.2 | Clave maestra local para desencriptar | **SPEC §4.4:** _"una clave maestra local"_ — parte de la infraestructura de encriptación AES-GCM. | `{data}/secret.key` (`0600`, autogenerado) o `OXID_MASTER_KEY` (64-hex); `oxid rotate-key` re-cifra sin downtime | ✅ Done (a0c064d) |
| 3.3 | Inyección real de variables al contenedor en `deploy()` | **SPEC §4.4:** _"se inyectan dinámicamente"_ al contenedor. **IDEA §6:** _"inyecta variables secretas y despliega."_ | `service/control_plane/provision.rs` — resuelve `VarSources` (Global→Project→Branch→Runtime) e inyecta el mapa resultante en `ContainerSpec.env` | ✅ Done (a0c064d) |
| 3.4 | Resolución de `SecretContext` con herencia `Global→Project→Branch→Runtime` | **SPEC §2.1:** _"Las variables de entorno se calculan por una matriz de herencia: Global → Project → Branch → Runtime."_ | `var_resolution.rs` conectado a `ControlPlane::deploy`; runtime gana sobre todo | ✅ Done (a0c064d) |
| 3.5 | Persistencia de secretos en SQLite (tabla `secrets`) | **SPEC §4.4:** _"secretos cacheados en disco"_ — implica persistencia. **IDEA §3:** _"guarda cada variable inyectada."_ | Tabla `secrets` en `0001_init.sql` + `SecretStore` en `SqliteStore` (valores cifrados AES-GCM) | ✅ Done (a0c064d) |

---

## 4. Resource Multiplexing (Bases de Datos)

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 4.1 | Adaptador `ResourcePoolPort` que ejecuta `CREATE DATABASE db_<branch>` en Postgres real | **SPEC §3.1:** _"El sistema mantiene un solo contenedor de base de datos encendido. Cuando se levanta la rama feature-A, el orquestador se conecta al contenedor raíz, crea un schema o base de datos lógica dedicada (db_feature_a)"_ | `adapter/postgres_pool.rs::PostgresPool` — `ensure_database`/`drop_database` reales vía `sqlx::PgPool`. Verificado en vivo contra `postgres:16-alpine`: cada rama obtiene su propia DB real, consultable desde el contenedor de la app | ✅ Done |
| 4.2 | Adaptador Redis que asigna `REDIS_DB=N` por branch | **SPEC §3.1:** _"Se comparte una única instancia de Redis. El orquestador inyecta una variable de entorno para que cada rama use un índice de base de datos distinto (REDIS_DB=1, REDIS_DB=2)"_ | `service/control_plane/provision.rs::provision_dependency` — asigna el índice libre más bajo (tabla `resource_leases`, sin necesitar un cliente Redis real ya que es pura contabilidad). Verificado en vivo: dos ramas obtienen `REDIS_DB=0` y `REDIS_DB=1` reales contra `redis:7-alpine` | ✅ Done |
| 4.3 | Conexión a instancia compartida de Postgres/Redis (configuración en `oxid.toml`) | **IDEA §6 (oxid.toml):** _"`[dependencies.database]` type = "postgres" shared_instance = "local-pg-cluster" inject_url_as = "DATABASE_URL""_ | `OXID_POSTGRES_URL`/`OXID_REDIS_URL` (daemon) + `[dependencies.*]` de `oxid.toml` (ya parseado) conectados end-to-end vía `ControlPlane::with_resource_pools` | ✅ Done |
| 4.4 | Inyección automática de `DATABASE_URL` / `REDIS_URL` al contenedor | **IDEA §6:** _"inyecta esa URL de conexión específica al contenedor de la aplicación como DATABASE_URL"_ | `service/control_plane/provision.rs::run_and_activate` inyecta `dependency.inject_url_as` como variable `Runtime` (gana sobre cualquier secreto del mismo nombre) | ✅ Done |
| 4.5 | Liberación de DB/schema cuando el entorno se destruye | **IDEA §6 (oxid.toml):** _"`destroy_after = "7d"` ... Oxid completamente destruye el contenedor y sus volúmenes efímeros."_ Implica limpieza de recursos. | `service/control_plane/provision.rs::release_dependencies` — `DROP DATABASE` real (Postgres) + libera el índice (Redis), en `destroy()` manual y en el `Destroy` del GC. Verificado: `DROP DATABASE` confirmado con `\l` en Postgres real tras `oxid down` | ✅ Done |

---

## 5. Scale-to-Zero / Proxy / Wake-on-Request

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 5.1 | Integración con Traefik (labels Docker + configuración) | **SPEC §3.2:** _"Un proxy inverso (Traefik) actúa como entrada."_ **SPEC §4.6:** _"Se levanta el contenedor con etiquetas específicas del proxy. Ejemplo: Subdominio → feat-login.dev.local."_ | `service/control_plane/infra.rs::traefik_labels` — `deploy()` agrega labels `traefik.enable`, router/service/middleware por rama, y une el contenedor a `OXID_DOCKER_NETWORK` (sin publicar host port) cuando está configurado; fallback a host-port si no. Bootstrap automatizado vía `oxid infra status`/`setup` (ver nota abajo) | ✅ Done |
| 5.2 | Endpoint de wake-on-request que Traefik llama | **SPEC §3.2:** _"Traefik está configurado para redirigir peticiones fallidas (por contenedor pausado) a un endpoint especial del orquestador en Rust."_ | `api/handlers/lifecycle.rs` — `POST /api/v1/wake` lee `Host`/`X-Forwarded-Host`, resuelve el entorno y lo despierta (`wake_by_url`); pensado como target de la `errors` middleware de Traefik | ✅ Done |
| 5.3 | Devolver señal de recarga al navegador (302 → wake → retry) | **SPEC §3.2:** _"El orquestador hace docker unpause (latencia ~300ms) y devuelve una señal de recarga al navegador."_ | `api/handlers/lifecycle.rs::wake_page_html` — página HTML con `meta refresh` estilo DESIGN (Carbon Black/Oxid Orange) | ✅ Done |
| 5.4 | Monitor de tráfico real (métricas de Traefik para GC) | **SPEC §3.2:** _"Un cron interno de Rust evalúa la actividad de red. Si la rama feature-x no recibe peticiones en 30 minutos, ejecuta docker pause feature-x."_ **IDEA §6:** _"Oxid lo nota, hace unpause"_ | `api/handlers/lifecycle.rs::heartbeat_by_host` + `service/control_plane/lifecycle.rs::touch_by_url` — endpoint `GET/POST /api/v1/heartbeat` pensado como `forwardAuth` middleware de Traefik en cada request; refresca `last_accessed_at` real en vez de solo tocarlo al desplegar | ✅ Done (requiere wiring de Traefik, ver nota abajo) |
| 5.5 | Latencia objetivo < 300ms para unpause | **SPEC §3.2:** _"El orquestador hace docker unpause (latencia ~300ms)"_ — métrica de referencia. | Benchmark reproducible: test `#[ignore]` `pause_wake_latency_stays_under_the_300ms_target` (`adapter/oci.rs`) — mide solo la operación wake (re-arm del estado fuera del cronómetro) e imprime p50/p95/p99. Medido en vivo contra Docker 29.6.1: **unpause p50=22.7ms p95=28.2ms p99=28.9ms** (target 300ms, ~10x margen); start de contenedor hibernado p50=262.3ms p95=274.2ms (cold start estructuralmente más lento, barra propia de 1000ms) | ✅ Done (medido) |

---

## 6. Control Plane

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 6.1 | Locking concurrente en SQLite | **SPEC §4.2:** _"Se adquiere un bloqueo transaccional en SQLite para evitar condiciones de carrera si entran múltiples pushes a la misma rama simultáneamente."_ | IDs `AUTOINCREMENT` + `RETURNING id` (asignación atómica); pool con `max_connections(1)` serializa escrituras | ✅ Done (a0c064d) |
| 6.2 | Ejecución de `on_start` hooks (migrations, seeds) | **IDEA §6 (oxid.toml):** _"`on_start = ["npm run db:migrate", "npm run db:seed"]` Command injected to run once the container starts"_ **IDEA §6:** _"compila si es necesario, inyecta variables secretas y despliega"_ | `ContainerPort::exec` (`docker exec`) invocado tras `run` en `deploy()` | ✅ Done (a0c064d) |
| 6.3 | Caché de capas Docker (BuildKit volumes) | **SPEC §4.5:** _"Se aplican técnicas de caché de capas (BuildKit) y volúmenes compartidos para dependencias (ej. ~/.m2, ~/.cargo/registry, node_modules en volúmenes Docker huérfanos mapeados automáticamente)."_ | `adapter/oci.rs::build` — builds vía `BuilderVersion::BuilderBuildKit` + sesión gRPC por build (feature `buildkit` de bollard; el builder V1 clásico está deprecado upstream). Los Dockerfiles con `# syntax=docker/dockerfile:1` + `RUN --mount=type=cache,...` ganan cachés de dependencias persistentes entre redeploys sin configuración extra. Verificado en vivo: test e2e `buildkit_cache_mounts_persist_across_builds` prueba que un marker escrito en el cache mount del build #1 sobrevive al build #2. Además `build()` ya no traga el mensaje real de error de build (`DockerStreamError` se desenvuelve). Costo medido: binario release 17MB → 19MB (~12%) | ✅ Done |
| 6.4 | WebSocket para notificaciones en vivo | **SPEC §4.7:** _"Registro en la base de datos de eventos ... y emisión por WebSocket hacia los clientes."_ | No existe — solo audit trail en SQLite | No existe |
| 6.5 | Cálculo de métricas de ahorro de RAM | **SPEC §5.2:** _"Visualización de ... uso de CPU/RAM en tiempo real por contenedor."_ **DESIGN §3.4:** _"Bottom pane: System stats (CPU / RAM saved by Oxid)."_ | No existe | No existe |

---

## 7. Persistencia

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 7.1 | Reemplazar `MAX(id)+1` por IDs autoincrementales | **SPEC §1:** _"Eficiencia Absoluta ... Huella de memoria mínima."_ IDs inseguros bajo concurrencia contradicen este principio. | `store.rs` — `AUTOINCREMENT` + `RETURNING id`; `next_*_id` eliminados | ✅ Done (a0c064d) |
| 7.2 | Tabla `secrets` para persistir variables de entorno | **SPEC §4.4:** _"secretos cacheados en disco"_ **IDEA §3:** _"guarda ... cada variable inyectada."_ | Tabla `secrets` en `0001_init.sql` con índice UNIQUE por scope | ✅ Done (a0c064d) |
| 7.3 | Tabla `resource_pools` para trackear pools | **SPEC §3.1:** _"Resource Pools"_ — el dominio los modela pero no se persisten. | Migración `0002_resource_leases.sql` — tabla `resource_leases` (project_id, branch, kind, shared_instance, resource_name), única por (proyecto, rama, kind, instancia), cascada al borrar el proyecto | ✅ Done |

---

## 8. CLI Output / Design System

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 8.1 | Prefijos coloreados: `[+]` verde, `[~]` gris, `[>]` naranja | **DESIGN §3.3:** _"`[+]` in Patina Green for success. `[~]` in Ash Gray for background tasks (e.g., pausing). `[>]` in Oxid Orange for actionable prompts or active builds."_ | `cli/main.rs` — helpers `ok`/`bg`/`action`/`error` con ANSI | ✅ Done (a0c064d) |
| 8.2 | Mensajes de error estilo Rust compiler | **DESIGN §5:** _"Following Rust's famous compiler errors, Oxid's errors must tell you exactly what went wrong and how to fix it."_ Ejemplo: _"Error reading oxid.toml on line 12: Invalid duration '30'. Did you mean '30m' or '30s'?"_ | CLI: errores de conexión diferenciados (timeout/DNS/conexión) con hint y exit codes por clase. Duraciones (`Ttl::parse`) ya sugerían unidades; `required()` ahora añade ejemplo accionable por campo (`missing \`[routing] port\` — add it under \`[routing]\`, e.g. \`port = 8080\` — the container port...`) y `PoolKind` lista los válidos | ✅ Done |
| 8.3 | Output de `deploy` con pasos: parse → shared DB → build → live | **DESIGN §3.3:** Ejemplo de output: _"[+] Parsed oxid.toml successfully → [+] Shared Postgres instance detected. Created db_feature_login → [>] Building image (Cache hit: 85%) → [+] Environment live at: https://feature-login.local.dev"_ | El daemon devuelve el deploy enriquecido: `ContainerPort::build` reporta `BuildReport {duration_ms, steps_total, steps_cached}` parseado del stream estructurado BuildKit (vertexes; fallback de texto para builders clásicos), y `run_and_activate` recopila líneas de dependencias ("created postgres database \`db_x\` (shared \`local-pg\)"). Handler las fusiona como `"build"`/`"dependencies"` junto al environment; `oxid up`/`rollback` imprimen "Image built (cache hit: N%, Xs)" + una línea por dependencia. Deploys *queued* reintentados server-side no llevan report (no hay caller) | ✅ Done |

---

## 9. TUI (Terminal User Interface)

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 9.1 | Crate `oxid-tui` con `ratatui` | **DESIGN §3.4:** _"Built with libraries like ratatui (Rust), the TUI is for power users."_ | No existe crate `oxid-tui` | No existe |
| 9.2 | Panel izquierdo: árbol de ramas | **DESIGN §3.4:** _"Left pane: Tree view of Git branches."_ | — | No existe |
| 9.3 | Panel derecho: logs en vivo | **DESIGN §3.4:** _"Right pane: Live container logs."_ | — | No existe |
| 9.4 | Panel inferior: stats CPU/RAM | **DESIGN §3.4:** _"Bottom pane: System stats (CPU / RAM saved by Oxid)."_ | — | No existe |
| 9.5 | Navegación vim-style (`j/k`, `Enter`, `/`) | **DESIGN §3.4:** _"Navigation: Vim-style bindings (j/k to move up/down, Enter to wake/sleep, / to search branches)."_ | — | No existe |

---

## 10. Web Dashboard

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 10.1 | Dashboard web embebido en el binario | **SPEC §5.3:** _"Incluido dentro del mismo binario estático de Rust (archivos precompilados e incrustados)."_ | `crates/oxid-daemon/web/*` embebido vía `include_str!` en `api/dashboard.rs`, servido en `/` (SPA multi-página con router client-side bajo `/ui/*`), sin build step ni dependencias nuevas (Alpine.js vendorizado, 54KB) | Hecho |
| 10.2 | Estilo brutalista con bordes duros, sin sombras | **DESIGN §3.2:** _"Use sharp corners (border-radius: 2px or 0px). Use hard 1px solid borders (#333333) instead of drop shadows to separate cards. Layout: Brutalist and data-dense."_ | `web/style.css` implementa la paleta y tipografía completas de DESIGN.md §1-3 | Hecho |
| 10.3 | Métricas globales del nodo | **SPEC §5.3:** _"Métricas globales del nodo, auditoría histórica de despliegues y visor de logs estructurados."_ | `GET /api/v1/stats` (`ControlPlane::node_stats`) + panel de auditoría/cola + visor de logs en vivo (streaming real vía `fetch`+`ReadableStream`, no `EventSource`, para poder enviar el header `Authorization`) | Hecho |

---

## 11. Desktop App (Tauri)

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 11.1 | App Tauri + React | **SPEC §5.4:** _"Aplicación de escritorio opcional, construida sobre Tauri, que se conecta al daemon de Rust en red local."_ | No existe | No existe |
| 11.2 | Barra de tareas con estados Verde/Gris/Rojo | **IDEA §4:** _"ver estados (Verde/Gris/Rojo) y compartir accesos."_ **DESIGN §3.1:** _"Running: Steel White + Patina Green. Paused: Ash Gray. Building: Oxid Orange."_ | — | No existe |
| 11.3 | Un clic para abrir URL efímera | **IDEA §4:** _"Un clic para abrir la URL de una rama"_ | — | No existe |

---

## 12. Infraestructura / Deployment

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 12.1 | Dockerfile para Oxid (el daemon como contenedor) | **SPEC §6:** _"docker run -d --name control-plane -v /var/run/docker.sock:/var/run/docker.sock -v /opt/data:/data -p 8080:8080 ghcr.io/tu-usuario/orchestrator:latest"_ | `Dockerfile` — build musl estático + runtime `distroless/static` (~29MB), validado con `docker build`/`docker run` reales contra un daemon vivo | ✅ Done |
| 12.2 | `docker run` con montaje de socket + `/data` | **SPEC §6:** _"Todo el sistema es un binario único (estático)."_ _"Estructura del directorio /data: /data/config.toml, /data/audit.sqlite, /data/git-cache/"_ | `docker-compose.yml` — daemon + Traefik, socket + volumen `/data`, labels `oxid-wake` documentadas; ejemplo probado con `docker compose config` | ✅ Done |
| 12.3 | Configuración global `config.toml` | **SPEC §6:** _"/data/config.toml (Configuración de dominios, tokens, reglas de recolección de basura)."_ | Superseded: dominios/tokens/GC ya se resuelven vía `OXID_*` env vars (`OXID_API_TOKEN`, `OXID_DOCKER_NETWORK`, `OXID_DAEMON_URL`, `OXID_GC_INTERVAL_SECS` — ver `main.rs`), consistente con el resto del diseño 12-factor del daemon. Un `config.toml` sería una segunda fuente de verdad redundante | Superseded por env vars |
| 12.4 | Binario estático cross-compilado | **IDEA §6:** _"El usuario instala Oxid ejecutando un solo binario."_ **SPEC §6:** _"Todo el sistema es un binario único (estático)."_ | `.github/workflows/release.yml` — build+publish de `oxid`/`oxidd` para linux-gnu, linux-musl (x86_64 y aarch64, estático), macOS (x86_64 + Apple Silicon) y Windows en cada tag `vX.Y.Z`. `flake.nix` (validado con `nix build`) para instalación nativa en NixOS/Nix | ✅ Done |
| 12.5 | Postura DR del nodo único (SQLite sin HA) | **SPEC §6:** Self-hosting de un binario único implica un solo estado persistente; el producto no promete HA multi-nodo. Decisión explícita: single-node es diseño, no omisión — DR vía snapshots, no replicación. | Backups periódicos automáticos: `service/backup.rs` — cada `OXID_BACKUP_INTERVAL_SECS` snapshot consistente (`VACUUM INTO`, seguro contra el pool vivo) en `{data}/backups/`, rotación por retención (`OXID_BACKUP_KEEP`, default 7; lógica de rotación unitestada, archivos ajenos al esquema nunca se tocan). Restore existente end-to-end (`oxid backup`/`restore` staged-on-restart). Ejemplo comentado de sidecar Litestream para replicación streaming off-site en `docker-compose.yml`. Default off: sin datos no hay nada que respaldar | ✅ Done |

---

## Multi-nodo

Fuera de este análisis por una razón que conviene dejar escrita: los
documentos de visión nunca lo prometieron (`SPEC.md` habla del **nodo**, en
singular), así que no es una brecha entre código y documentos sino una
expansión de alcance.

El plan completo — inventario de lo que asume un solo nodo, decisión de
arquitectura con sus alternativas descartadas, esquema, enrutado, modos de
fallo y etapas con estimaciones — vive en
[`MULTINODE.md`](MULTINODE.md). Su etapa 0 (cola de deploys reclamable de
forma atómica) ya está entregada, porque arreglaba un fallo que muerde con
un solo servidor.

## Priorización

> **Última actualización:** P1 (UX CLI), P2 (Scale-to-Zero real) y P5 (Dockerfile, self-hosting,
> release binaries) entregados ✅. Verificado con un
> daemon y CLI reales (binarios compilados, Docker real, repo git real con ramas de verdad,
> webhook firmado con HMAC real) en vez de solo `cargo test` — ver hallazgos abajo.
>
> **Quinta ronda: auditoría de producción del CLI** — cerrados los 2 Parcial (8.2/8.3) y el
> hardening restante:
> - **Token del CLI en disco a `0600`**: `~/.config/oxid/config.toml` guarda bearer tokens y
>   nacía world-readable por umask; `save` fuerza 0600 y `load` avisa+corrige archivos laxos
>   existentes (parity con `secret.key` del daemon).
> - **Timeout de conexión (10s)** en el cliente HTTP: un daemon colgado ya no cuelga al CLI
>   indefinidamente (sin timeout total: `logs -f`/`up`/`backup` son lentos por diseño).
> - **`oxid stats`** standalone (antes solo dentro de `doctor`); hint de Ctrl+C en `oxid up`
>   ("el deploy sigue server-side", exit 130).
> - **Deploy report end-to-end**: `ContainerPort::build → BuildReport` parseando los eventos
>   estructurados BuildKit (`aux` vertexes, excluyendo `[internal]`; fallback de texto clásico),
>   resumen de leases de dependencias en `run_and_activate`, fusionados como
>   `"build"`/`"dependencies"` en las respuestas de deploy/rollback e impresos por la CLI.
>
> **Cuarta ronda: cerrados los 5 gaps de producción de la auditoría previa**
> (rate-limit per-IP, webhooks GitLab/Gitea/Gogs, caché BuildKit, backups automáticos DR,
> benchmark unpause):
> - **Rate limiting por client IP:** ya existía bucket global (`OXID_RATE_LIMIT_PER_SECOND`/`BURST`
>   + test) — el ROADMAP estaba desactualizado. Mejorado: nuevo `ClientIpKeyExtractor` con
>   fallback a bucket compartido cuando no hay `ConnectInfo` (tests), y
>   `into_make_service_with_connect_info` cableado en ambos serve paths (HTTP y TLS). Tras un
>   reverse proxy único degrada a global — XFF deliberadamente no confiado (spoofable).
> - **Webhooks multi-proveedor (2.3):** refactor a núcleo común `handle_push` + GitLab
>   (`X-Gitlab-Token` constant-time, branch borrada = null-SHA en `after`, `object_kind` filter)
>   y Gitea/Gogs (HMAC hex sin prefijo, payload tipo GitHub). 13 tests.
> - **BuildKit (6.3):** builds vía `BuilderVersion::BuilderBuildKit` + sesión gRPC (feature
>   `buildkit` de bollard). Cache mounts (`RUN --mount=type=cache`) verificados e2e con marker
>   persistente entre builds. Bonus: `build()` ya reporta el error real del stream en vez de un
>   genérico "Docker stream error".
> - **Backups automáticos DR (12.5 nuevo):** snapshots periódicos `VACUUM INTO` con rotación por
>   retención (`service/backup.rs`, rotación unitestada), default off, Litestream documentado en
>   el compose. Postura explícita: single-node es diseño; DR ≠ HA.
> - **Benchmark unpause (5.5):** test reproducible que cronometra solo la operación wake. Medido:
>   unpause p50=22.7ms / p95=28.2ms (target 300ms); hibernating-start p50=262.3ms / p95=274.2ms.

> **Segunda pasada de verificación agresiva (multi-proyecto, multi-rama, concurrencia,
> seguridad, rendimiento) encontró y corrigió 9 bugs reales más:**
> - **Fuga de secretos entre ramas (seguridad, crítico):** el filtro SQL que resuelve "secretos
>   visibles para este deploy" (`SECRET_CONTEXT_FILTER`) tenía una condición redundante que hacía
>   que coincidiera con *cualquier* fila del proyecto sin importar la rama. Dos ramas con un secreto
>   `branch`-scoped del mismo nombre podían recibir el valor de la otra. Confirmado desplegando
>   `main` y `feature-cart` con `DB_PASS` distinto por rama y viendo el valor cruzado. Corregido el
>   filtro SQL además de un segundo bug de precedencia (`secrets_for` no aplicaba
>   `Global→Project→Branch` correctamente, solo tomaba la última fila devuelta por SQLite).
> - **Deploy fallido bloquea la rama para siempre (confiabilidad, crítico):** si `run()`/`exec()`
>   fallaba después de crear la fila `Environment` (`Building`), esta quedaba atascada — `Building`
>   no puede transicionar a `Destroy`, así que todo `oxid up` posterior de esa rama fallaba con
>   `transition 'Destroy' is not allowed from 'Building'`. Corregido: un fallo ahí ahora transiciona
>   a `BuildFailed`. De paso se hizo el deploy resiliente a contenedores huérfanos (sin fila en la
>   DB) removiéndolos defensivamente antes de correr uno nuevo.
> - **Hooks `on_start` fallidos se ignoraban silenciosamente (correctness, crítico):** `exec()`
>   nunca revisaba el exit code del comando — una migración rota se reportaba como deploy exitoso.
>   Corregido inspeccionando `ExecInspectResponse.exit_code` vía bollard; ahora un hook que falla
>   aborta el deploy con el mensaje de error real (stdout/stderr capturado).
> - **Condición de carrera en deploys concurrentes (crítico):** desplegar la misma rama (o incluso
>   ramas distintas del mismo proyecto) en paralelo corrompía el checkout de git compartido
>   (`tar_context` fallando a mitad de camino) y podía dejar `status`/`down`/`pause`/`wake`
>   apuntando a la fila `Destroyed` de un deploy perdedor mientras el contenedor del ganador seguía
>   vivo. Confirmado disparando 10 `oxid up` simultáneos a la misma rama nueva. Corregido con un
>   lock async que serializa `deploy()` de punta a punta.
> - **`register_project` no era realmente idempotente bajo concurrencia:** el
>   check-then-act entre "¿existe ya?" y `INSERT` no es atómico; 10 registros concurrentes del
>   mismo proyecto nuevo dejaban 9 con `409 UNIQUE constraint failed` en vez de devolver el
>   proyecto ya creado. Corregido con fallback: ante conflicto, relee y devuelve el existente.
> - **Clave maestra AES-GCM legible por cualquier usuario (seguridad):** `secret.key` se creaba con
>   permisos `0644` (umask por defecto) en vez de `0600` — en una máquina compartida, cualquier
>   usuario local podía leer la clave que desencripta todos los secretos. Corregido forzando `0600`.
> - **`docker stop` con timeout de gracia de 10s por defecto** retrasaba cada `Hibernate`/`Destroy`
>   del GC en hasta 10 segundos extra, en contra de la propuesta de "Scale-to-Zero" ágil. Reducido
>   a un timeout de 2s (contenedores efímeros de desarrollo, no producción crítica).
>
> Todo verificado en vivo con Docker real, no solo con `cargo test`: fuga de secretos reproducida
> y corregida con dos ramas reales, 10 deploys/10 registros concurrentes reales (100% éxito tras el
> fix, antes fallaban en cascada), ciclo de vida GC completo (`Running→Paused→Hibernating→Destroyed`)
> observado con timestamps reales, latencia real de `pause`/`wake` medida en 25-36ms (SPEC pedía
> ~300ms), RSS del daemon 3.8-9MB con hasta 6 branches corriendo (cumple "<15MB en reposo").
>
> **Tercera ronda: cerrados todos los gaps identificados en la auditoría anterior + implementado
> P3 (resource pooling) completo.** Nuevo, en esta ronda:
> - **Auth de la API** (`OXID_API_TOKEN` + `Authorization: Bearer`, comparación en tiempo
>   constante, abierta por defecto con warning al arrancar) — el hueco de seguridad más grande
>   que quedaba (cualquiera con acceso de red podía desplegar/destruir/manipular secretos).
> - **`oxid rm-project`** — borra proyecto completo (entornos, imágenes, git-cache, secretos vía
>   cascada). Antes un proyecto registrado era permanente.
> - **Limpieza de imágenes Docker** al destruir un entorno (manual o GC) — antes solo se limpiaba
>   el contenedor, las imágenes `oxid/<project>/<branch>` se acumulaban para siempre.
> - **`oxid down --purge-secrets`** — opt-in, ya que por defecto los secretos de una rama
>   sobreviven a su destrucción/redeploy (conveniencia para ramas recurrentes).
> - **Webhook:** ignora eventos no-`push` (`ping` ya no rompe con "missing ref") y una
>   push con `"deleted": true` destruye el entorno en vez de intentar desplegar una rama que ya
>   no existe.
> - **`lifecycle_lock`** ampliado de solo-`deploy` a `pause`/`wake`/`destroy`/cada acción del GC —
>   cerraba una carrera real entre un sweep automático y una acción manual sobre el mismo entorno.
> - **P3 completo:** `PostgresPool` (adaptador real vía `sqlx`, `CREATE`/`DROP DATABASE`) +
>   asignación de índice Redis (tabla `resource_leases`, sin necesitar cliente Redis ya que es
>   pura contabilidad). Inyecta `DATABASE_URL`/`REDIS_URL` reales, reutiliza el lease entre
>   redeploys, libera al destruir. **Verificado en vivo de punta a punta:** Traefik real (no
>   simulado) enrutando a dos ramas con Postgres+Redis reales, cada una con su propia DB
>   (`db_full_app_main`, `db_full_app_feature_x`) e índice Redis (`0`, `1`) confirmados desde
>   dentro del contenedor de la app; `DROP DATABASE` confirmado tras `oxid down`.
> - También verificado en vivo: `oxid down` sin `--force` respondiendo "n" (aborta
>   correctamente), `--api`/`--token` con dos daemons reales corriendo simultáneamente
>   (override correcto sobre `OXID_API`/`OXID_TOKEN`), `oxid logs -f` en 0.0% CPU en reposo
>   (confirma que el poll duerme 2s, no hace busy-wait).
>
> **Bugs reales encontrados y corregidos en la primera pasada (no solo los dos de P2):**
> - Cada branch publicaba el mismo `host_port`, así que dos ramas del mismo proyecto no podían
>   correr a la vez. Se resuelve al unir el contenedor a `OXID_DOCKER_NETWORK` (Traefik) en vez de
>   publicar puerto de host. **Confirmado en vivo:** `main` y `feature-one` del mismo proyecto
>   corriendo simultáneamente sin colisión, alcanzables por nombre DNS en la red compartida.
> - `wake()` siempre llamaba `docker unpause`, que no hace nada útil sobre un contenedor
>   `Hibernating` (fue `stop`peado, no `pause`ado). Ahora se añadió `ContainerPort::start` y
>   `wake` distingue `Paused` (`unpause`) de `Hibernating` (`start`).
> - **Crítico:** desplegar una rama que no es la default (ej. `feature-login`) fallaba siempre en
>   el primer deploy de un proyecto con `branch 'X' not found` — `git2` solo materializa
>   `refs/heads/<default>` tras un clone, el resto queda en `refs/remotes/origin/*`. Rompía
>   literalmente el caso de uso central del producto. Corregido en `adapter/git.rs`
>   (`sync_resolve_branch_head` ahora resuelve contra `refs/remotes/origin/<branch>` primero) y de
>   paso se agregó `fetch` en cada `ensure_repo` con caché ya existente (antes nunca se actualizaba
>   tras el primer clone, así que un redeploy vía webhook desplegaba código congelado del primer
>   clone, no el commit nuevo).
> - **Crítico:** redeploy de una rama ya viva (ej. webhook en un segundo push) fallaba con
>   `409 Conflict` de Docker por nombre de contenedor duplicado — `deploy()` nunca tiraba el
>   contenedor anterior. Corregido en `service/control_plane/deploy.rs::deploy_at` (tira el contenedor previo y
>   marca su fila `Destroyed` antes de crear el nuevo). Confirmado en vivo con un webhook real
>   simulando un segundo push a la misma rama.
> - Una rama redeployada tras `oxid down` dejaba dos filas de `Environment` con la misma URL/nombre
>   de contenedor (una `Destroyed`, otra viva); toda resolución "rama → entorno" (CLI
>   `down`/`pause`/`wake`/`logs`, y `find_by_url` para wake-on-request) tomaba la primera en vez de
>   la más reciente. Causaba que `pause`/`wake` fallaran en la API pero igual actuaran sobre el
>   contenedor real (mismo nombre), dejando el estado en SQLite inconsistente con Docker.
>   Corregido: la API y `find_environment_by_branch`/`find_by_url` ahora resuelven siempre a la
>   fila más reciente por rama/URL; `oxid status` deduplica mostrando solo la más reciente.
> - Cosmético: `oxid up`/`status` imprimían el nombre del proyecto con comillas literales
>   (`` `"e2e-app"` ``) por interpolar un `serde_json::Value` directo en vez de `.as_str()`.
>
> **Nota de wiring Traefik (actualizada):** las labels de Traefik (5.1) y los endpoints
> `/api/v1/wake` y `/api/v1/heartbeat` (5.2/5.4) están implementados y probados. Desde
> `b4b1eb4`, el bootstrap de la infraestructura está automatizado: `oxid infra status`
> reporta read-only si existen la red Docker, el contenedor Traefik y el wiring del propio
> daemon (`GET /api/v1/infra/status`), y `oxid infra setup` crea de forma idempotente la red
> y levanta el Traefik built-in si faltan (`POST /api/v1/infra/bootstrap`). El único paso que
> sigue siendo manual es conectar el propio contenedor del daemon a la red/labels (Docker no
> puede relabelar un contenedor corriendo sin recrearlo, y recrear el proceso que ejecuta la
> llamada no es seguro de automatizar) — `InfraStatus::next_steps` imprime exactamente qué
> falta. Sin Traefik, el sistema funciona en modo "publicación directa de puertos": desde el
> backfill de `host_port` dinámico y el reverse-proxy TCP built-in por rama
> (`service/proxy.rs`), ese modo ya permite múltiples ramas vivas simultáneas del mismo
> proyecto, cada una con su `public_port` estable y redeploys sin downtime.

| Prioridad | Categoría | Tareas | Justificación |
|---|---|---|---|
| **P0 — Core funcional** | Control plane, Secretos, Webhook auth | 2.1, 2.2, 3.3, 3.4, 3.5, 6.1, 6.2, 7.1, 7.2 ✅ | Sin estos, el sistema no es seguro ni funcional |
| **P1 — UX CLI** | Coloreado, comandos faltantes | 1.1–1.10 ✅ (1.3 parcial: polling, no SSE), 8.1 ✅ | Sin esto, la CLI es inutilizable para el usuario final |
| **P2 — Scale-to-Zero real** | Traefik, wake-on-request | 5.1–5.4 ✅ (requiere wiring de infraestructura, ver nota arriba) | La feature estrella del producto ahora emite lo necesario end-to-end |
| **P3 — Resource pooling** | Multiplexación DB real | 4.1–4.5, 7.3 ✅ | Diferenciador competitivo vs levantar contenedores por branch — verificado en vivo con Postgres+Redis reales |
| **P4 — Interfaces** | TUI, Dashboard, Desktop | 9.x, 10.x ✅, 11.x | Features de producto completo, no MVP |
| **P5 — Ops/Deploy** | Dockerfile, release binaries | 12.1, 12.2, 12.4 ✅ · 12.3 superseded | Necesario para self-hosting real |

**Total: 57 tareas granulares** (43 ✅ Done + 3 Hecho · 0 Parcial · 10 No existe · 0 No medido · 1 Superseded). Las tareas P0 son las que convierten a Oxid de "demo" a "usable". Las P1 lo hacen agradable. Las P2-P3 lo hacen competitivo. Las P4-P5 son features de producto completo.
