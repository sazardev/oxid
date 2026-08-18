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
| 1.1 | `oxid down <branch>` — destruir entorno | **SPEC §5.1:** _"`$ ctrl up my-repo --branch feature-login` → Fuerza un despliegue manual."_ Se infiere `down` como operación opuesta. | No existe | No existe |
| 1.2 | `oxid status` — listar entornos del proyecto actual | **IDEA §4:** _"Comandos cortos, salida coloreada e intuitiva. Ej: `oxid up feature-login`, `oxid env set`."_ | `oxid ps` lista proyectos, no entornos por proyecto | No existe |
| 1.3 | `oxid logs <branch> -f` — streaming real vía SSE/chunked | **SPEC §5.1:** _"`$ ctrl logs feature-login -f` → Streaming de logs de la rama."_ | Stub: imprime "not implemented yet" | Stub |
| 1.4 | `oxid env set KEY=val --scope global` — inyectar secretos | **SPEC §5.1:** _"`$ ctrl env set my-repo DB_PASSWORD=secret --scope global` → Inyecta variables."_ | Stub: imprime "not implemented yet" | Stub |
| 1.5 | `oxid env list` — ver secretos configurados | **IDEA §4:** La tabla de interfaces muestra `oxid env set` como ejemplo de la CLI. `list` es el complemento natural. | No existe | No existe |
| 1.6 | `oxid pause <branch>` — pausar manualmente | **IDEA §6:** _"Oxid apagó el contenedor, liberando 500MB de RAM."_ El usuario debe poder forzar esta acción. | No existe | No existe |
| 1.7 | `oxid wake <branch>` — despertar manualmente | **IDEA §6:** _"Un tester entra a la URL, Oxid lo nota, hace `unpause` en 200ms."_ El usuario debe poder forzar esta acción. | No existe | No existe |
| 1.8 | Coloreado ANSI con prefijos `[+]` `[~]` `[>]` | **DESIGN §3.3:** _"`[+]` in Patina Green for success. `[~]` in Ash Gray for background tasks. `[>]` in Oxid Orange for actionable prompts."_ | Solo usa `eprintln!` y `println!` sin códigos ANSI | Parcial |
| 1.9 | Flag `--force` para sobreescribir | **DESIGN §5:** _"Offer `[--force]` flags, but clearly state what they overwrite."_ | No existe | No existe |
| 1.10 | Flag `--api` para configurar daemon URL | **SPEC §6:** La sección de configuración sugiere que el sistema debe ser configurable. Actualmente hardcodea `OXID_API`. | Hardcodea `OXID_API` env var | Falta |

---

## 2. Webhook y Seguridad

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 2.1 | Verificación HMAC-SHA256 de webhooks GitHub | **SPEC §4.1:** _"axum recibe un push webhook de GitHub/GitLab. Se verifica el payload criptográficamente."_ | `api.rs:206-243` — acepta cualquier JSON sin verificar firma | No existe |
| 2.2 | Secret configurable (`OXID_WEBHOOK_SECRET`) | **SPEC §4.1:** implícito en _"Se verifica el payload criptográficamente"_. HMAC requiere un shared secret. | No existe | No existe |
| 2.3 | Soporte webhooks GitLab (formato distinto) | **SPEC §4.1:** _"axum recibe un push webhook de GitHub/GitLab."_ Menciona ambos proveedores. | Solo parsea formato GitHub (`ref`, `repository.full_name`) | No existe |
| 2.4 | Rate limiting en la API HTTP | **SPEC §1:** _"Ecosistema Unificado: No requiere herramientas de terceros."_ Implica protección integrada. | No existe | No existe |
| 2.5 | Autenticación API (bearer token mínimo) | **SPEC §6:** La configuración incluye _"tokens"_ en `/data/config.toml`. | No existe | No existe |

---

## 3. Secretos e Inyección de Variables

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 3.1 | Almacenamiento encriptado de secretos (AES-GCM) | **SPEC §4.4:** _"Se compilan los secretos cacheados en disco (encriptados mediante AES-GCM usando una clave maestra local) y se inyectan dinámicamente."_ | `secret_context.rs` — dominio puro, sin persistencia ni encriptación | No existe |
| 3.2 | Clave maestra local para desencriptar | **SPEC §4.4:** _"una clave maestra local"_ — parte de la infraestructura de encriptación AES-GCM. | No existe | No existe |
| 3.3 | Inyección real de variables al contenedor en `deploy()` | **SPEC §4.4:** _"se inyectan dinámicamente"_ al contenedor. **IDEA §6:** _"inyecta variables secretas y despliega."_ | `control_plane.rs:169-172` — solo inyecta `OXID_BRANCH` y `OXID_ENV_URL`, no secretos del usuario | Parcial |
| 3.4 | Resolución de `SecretContext` con herencia `Global→Project→Branch→Runtime` | **SPEC §2.1:** _"Las variables de entorno se calculan por una matriz de herencia: Global → Project → Branch → Runtime."_ | `var_resolution.rs` existe en dominio pero `ControlPlane::deploy` no lo invoca | No conectado |
| 3.5 | Persistencia de secretos en SQLite (tabla `secrets`) | **SPEC §4.4:** _"secretos cacheados en disco"_ — implica persistencia. **IDEA §3:** _"guarda cada variable inyectada."_ | No existe tabla `secrets` en `0001_init.sql` | No existe |

---

## 4. Resource Multiplexing (Bases de Datos)

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 4.1 | Adaptador `ResourcePoolPort` que ejecuta `CREATE DATABASE db_<branch>` en Postgres real | **SPEC §3.1:** _"El sistema mantiene un solo contenedor de base de datos encendido. Cuando se levanta la rama feature-A, el orquestador se conecta al contenedor raíz, crea un schema o base de datos lógica dedicada (db_feature_a)"_ | `resource_pool.rs` — dominio puro con `lease()`/`release()`, sin adaptador real | No existe |
| 4.2 | Adaptador Redis que asigna `REDIS_DB=N` por branch | **SPEC §3.1:** _"Se comparte una única instancia de Redis. El orquestador inyecta una variable de entorno para que cada rama use un índice de base de datos distinto (REDIS_DB=1, REDIS_DB=2)"_ | No existe | No existe |
| 4.3 | Conexión a instancia compartida de Postgres/Redis (configuración en `oxid.toml`) | **IDEA §6 (oxid.toml):** _"`[dependencies.database]` type = "postgres" shared_instance = "local-pg-cluster" inject_url_as = "DATABASE_URL""_ | `project_config.rs` parsea `dependencies` pero no hay adaptador que conecte | No existe |
| 4.4 | Inyección automática de `DATABASE_URL` / `REDIS_URL` al contenedor | **IDEA §6:** _"inyecta esa URL de conexión específica al contenedor de la aplicación como DATABASE_URL"_ | `control_plane.rs` no construye estas variables | No existe |
| 4.5 | Liberación de DB/schema cuando el entorno se destruye | **IDEA §6 (oxid.toml):** _"`destroy_after = "7d"` ... Oxid completamente destruye el contenedor y sus volúmenes efímeros."_ Implica limpieza de recursos. | No existe | No existe |

---

## 5. Scale-to-Zero / Proxy / Wake-on-Request

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 5.1 | Integración con Traefik (labels Docker + configuración) | **SPEC §3.2:** _"Un proxy inverso (Traefik) actúa como entrada."_ **SPEC §4.6:** _"Se levanta el contenedor con etiquetas específicas del proxy. Ejemplo: Subdominio → feat-login.dev.local."_ | `control_plane.rs:180-184` — agrega labels `oxid.project`, `oxid.branch`, `oxid.url` pero no labels de Traefik | No existe |
| 5.2 | Endpoint de wake-on-request que Traefik llama | **SPEC §3.2:** _"Traefik está configurado para redirigir peticiones fallidas (por contenedor pausado) a un endpoint especial del orquestador en Rust."_ | `api.rs:44-45` — endpoint `POST /environments/{id}/wake` existe pero Traefik no lo usa | No conectado |
| 5.3 | Devolver señal de recarga al navegador (302 → wake → retry) | **SPEC §3.2:** _"El orquestador hace docker unpause (latencia ~300ms) y devuelve una señal de recarga al navegador."_ | No existe — el endpoint wake solo retorna `204 No Content` | No existe |
| 5.4 | Monitor de tráfico real (métricas de Traefik para GC) | **SPEC §3.2:** _"Un cron interno de Rust evalúa la actividad de red. Si la rama feature-x no recibe peticiones en 30 minutos, ejecuta docker pause feature-x."_ **IDEA §6:** _"Oxid lo nota, hace unpause"_ | `gc.rs` solo compara `last_accessed_at` de SQLite, no métricas de tráfico HTTP reales | No existe |
| 5.5 | Latencia objetivo < 300ms para unpause | **SPEC §3.2:** _"El orquestador hace docker unpause (latencia ~300ms)"_ — métrica de referencia. | No hay benchmarks ni mediciones | No medido |

---

## 6. Control Plane

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 6.1 | Locking concurrente en SQLite | **SPEC §4.2:** _"Se adquiere un bloqueo transaccional en SQLite para evitar condiciones de carrera si entran múltiples pushes a la misma rama simultáneamente."_ | `control_plane.rs` — no hay locking; `store.rs` usa transacciones pero sin adquirir locks explícitos | No existe |
| 6.2 | Ejecución de `on_start` hooks (migrations, seeds) | **IDEA §6 (oxid.toml):** _"`on_start = ["npm run db:migrate", "npm run db:seed"]` Command injected to run once the container starts"_ **IDEA §6:** _"compila si es necesario, inyecta variables secretas y despliega"_ | `control_plane.rs:168-186` — ejecuta `run` del contenedor pero no ejecuta comandos `on_start` dentro de él | No existe |
| 6.3 | Caché de capas Docker (BuildKit volumes) | **SPEC §4.5:** _"Se aplican técnicas de caché de capas (BuildKit) y volúmenes compartidos para dependencias (ej. ~/.m2, ~/.cargo/registry, node_modules en volúmenes Docker huérfanos mapeados automáticamente)."_ | `oci.rs:61-73` — `build_image` sin volúmenes de caché configurados | No existe |
| 6.4 | WebSocket para notificaciones en vivo | **SPEC §4.7:** _"Registro en la base de datos de eventos ... y emisión por WebSocket hacia los clientes."_ | No existe — solo audit trail en SQLite | No existe |
| 6.5 | Cálculo de métricas de ahorro de RAM | **SPEC §5.2:** _"Visualización de ... uso de CPU/RAM en tiempo real por contenedor."_ **DESIGN §3.4:** _"Bottom pane: System stats (CPU / RAM saved by Oxid)."_ | No existe | No existe |

---

## 7. Persistencia

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 7.1 | Reemplazar `MAX(id)+1` por IDs autoincrementales | **SPEC §1:** _"Eficiencia Absoluta ... Huella de memoria mínima."_ IDs inseguros bajo concurrencia contradicen este principio. | `store.rs` — `SELECT COALESCE(MAX(id), 0) + 1` para asignación de IDs | Deuda técnica |
| 7.2 | Tabla `secrets` para persistir variables de entorno | **SPEC §4.4:** _"secretos cacheados en disco"_ **IDEA §3:** _"guarda ... cada variable inyectada."_ | No existe en `0001_init.sql` | No existe |
| 7.3 | Tabla `resource_pools` para trackear pools | **SPEC §3.1:** _"Resource Pools"_ — el dominio los modela pero no se persisten. | `resource_pool.rs` es solo lógica en memoria | No existe |

---

## 8. CLI Output / Design System

| # | Tarea | Cita del documento | Código actual | Estado |
|---|---|---|---|---|
| 8.1 | Prefijos coloreados: `[+]` verde, `[~]` gris, `[>]` naranja | **DESIGN §3.3:** _"`[+]` in Patina Green for success. `[~]` in Ash Gray for background tasks (e.g., pausing). `[>]` in Oxid Orange for actionable prompts or active builds."_ | `cli/main.rs` — usa `println!` y `eprintln!` sin códigos ANSI | No implementado |
| 8.2 | Mensajes de error estilo Rust compiler | **DESIGN §5:** _"Following Rust's famous compiler errors, Oxid's errors must tell you exactly what went wrong and how to fix it."_ Ejemplo: _"Error reading oxid.toml on line 12: Invalid duration '30'. Did you mean '30m' or '30s'?"_ | `config.rs` devuelve errores como `ConfigError::Validation(String)` sin acción sugerida | Parcial |
| 8.3 | Output de `deploy` con pasos: parse → shared DB → build → live | **DESIGN §3.3:** Ejemplo de output: _"[+] Parsed oxid.toml successfully → [+] Shared Postgres instance detected. Created db_feature_login → [>] Building image (Cache hit: 85%) → [+] Environment live at: https://feature-login.local.dev"_ | `cli/main.rs:86-131` — solo imprime `[>] oxid up` y `[+] Environment live at` | No implementado |

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
| 10.1 | Dashboard web embebido en el binario | **SPEC §5.3:** _"Incluido dentro del mismo binario estático de Rust (archivos precompilados e incrustados)."_ | No existe | No existe |
| 10.2 | Estilo brutalista con bordes duros, sin sombras | **DESIGN §3.2:** _"Use sharp corners (border-radius: 2px or 0px). Use hard 1px solid borders (#333333) instead of drop shadows to separate cards. Layout: Brutalist and data-dense."_ | — | No existe |
| 10.3 | Métricas globales del nodo | **SPEC §5.3:** _"Métricas globales del nodo, auditoría histórica de despliegues y visor de logs estructurados."_ | — | No existe |

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
| 12.1 | Dockerfile para Oxid (el daemon como contenedor) | **SPEC §6:** _"docker run -d --name control-plane -v /var/run/docker.sock:/var/run/docker.sock -v /opt/data:/data -p 8080:8080 ghcr.io/tu-usuario/orchestrator:latest"_ | No existe Dockerfile | No existe |
| 12.2 | `docker run` con montaje de socket + `/data` | **SPEC §6:** _"Todo el sistema es un binario único (estático)."_ _"Estructura del directorio /data: /data/config.toml, /data/audit.sqlite, /data/git-cache/"_ | No existe | No existe |
| 12.3 | Configuración global `config.toml` | **SPEC §6:** _"/data/config.toml (Configuración de dominios, tokens, reglas de recolección de basura)."_ | Solo existe `oxid.toml` por proyecto, no hay config global del daemon | No existe |
| 12.4 | Binario estático cross-compilado | **IDEA §6:** _"El usuario instala Oxid ejecutando un solo binario."_ **SPEC §6:** _"Todo el sistema es un binario único (estático)."_ | `Cargo.toml` usa `edition = "2024"` pero no hay configuración de cross-compilation | No existe |

---

## Priorización

| Prioridad | Categoría | Tareas | Justificación |
|---|---|---|---|
| **P0 — Core funcional** | Control plane, Secretos, Webhook auth | 2.1, 2.2, 3.3, 3.4, 6.1, 6.2 | Sin estos, el sistema no es seguro ni funcional |
| **P1 — UX CLI** | Coloreado, comandos faltantes | 1.1–1.8, 8.1–8.3 | Sin esto, la CLI es inutilizable para el usuario final |
| **P2 — Scale-to-Zero real** | Traefik, wake-on-request | 5.1–5.4 | La feature estrella del producto no funciona end-to-end |
| **P3 — Resource pooling** | Multiplexación DB real | 4.1–4.4 | Diferenciador competitivo vs levantar contenedores por branch |
| **P4 — Interfaces** | TUI, Dashboard, Desktop | 9.x, 10.x, 11.x | Features de producto completo, no MVP |
| **P5 — Ops/Deploy** | Dockerfile, config global | 12.x | Necesario para self-hosting real |

**Total: ~50 tareas granulares.** Las tareas P0 son las que convierten a Oxid de "demo" a "usable". Las P1 lo hacen agradable. Las P2-P3 lo hacen competitivo. Las P4-P5 son features de producto completo.
