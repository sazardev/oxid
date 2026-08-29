// Translation catalog and locale plumbing for the dashboard.
//
// Served as its own asset (see `api/dashboard.rs`) rather than inlined into
// `app.js`: the catalog is the part a translator touches, and keeping it in
// one file means adding a language never means reading application code.
//
// Adding a locale: add an entry to `CATALOG` with the same keys as `en`, and
// a label in `LOCALES`. `missingKeys()` in the browser console reports any
// key a locale is missing; anything absent falls back to English rather than
// rendering an empty element.
(function () {
  "use strict";

  const LOCALES = [
    { code: "en", label: "EN", name: "English" },
    { code: "es", label: "ES", name: "Español" },
  ];

  const CATALOG = {
    en: {
      // ---- chrome -------------------------------------------------------
      "app.tagline": "control plane",
      "app.title": "Oxid — Control Plane",
      "nav.environments": "environments",
      "nav.projects": "projects",
      "nav.queue": "queue",
      "nav.audit": "audit",
      "nav.secrets": "secrets",
      "nav.admin": "admin",
      "nav.setup": "setup",
      "nav.setupTitle": "First-run setup wizard",
      "conn.connected": "Connected",
      "conn.unreachable": "Unreachable",
      "conn.tokenPlaceholder": "Bearer token (if configured)",
      "conn.refresh": "Refresh now",
      "conn.language": "Language",
      "banner.authError": "[!] 401/403 from the daemon — check the bearer token above.",
      "banner.connError": "[!] Cannot reach the daemon at",
      "banner.dismiss": "[click to dismiss]",
      "footer.tagline": "Ephemeral environments that breathe.",
      "footer.autoRefresh": "auto-refresh {secs}s",

      // ---- stats --------------------------------------------------------
      "stats.projects": "Projects",
      "stats.running": "Running",
      "stats.paused": "Paused",
      "stats.building": "Building",
      "stats.hibernating": "Hibernating",
      "stats.failed": "Failed",
      "stats.queued": "Queued",
      "stats.hostMemory": "Host memory",

      // ---- environment states (also the filter options) -----------------
      "state.running": "running",
      "state.paused": "paused",
      "state.building": "building",
      "state.hibernating": "hibernating",
      "state.build_failed": "build failed",
      "state.destroyed": "destroyed",

      // ---- environments list --------------------------------------------
      "env.title": "Environments",
      "env.noTraefik":
        "[~] No Traefik configured ({var} unset) — URLs below link to each environment's own dynamically-assigned host port instead of a subdomain. Docker picks a free port per deploy, so multiple branches of the same project can run simultaneously even without Traefik — the port just isn't stable across redeploys.",
      "env.allProjects": "all projects",
      "env.allStates": "all states",
      "env.searchBranch": "search branch...",
      "env.empty": "No environments match. Run {cmd}, or clear the filters.",
      "env.col.project": "Project",
      "env.col.branch": "Branch",
      "env.col.state": "State",
      "env.col.url": "URL",
      "env.col.created": "Created",
      "env.col.lastSeen": "Last seen",
      "env.col.updated": "Updated",

      // ---- environment detail --------------------------------------------
      "env.notFound": "Environment not found (destroyed, or the id is wrong).",
      "env.field.id": "id",
      "env.field.url": "url",
      "env.field.note": "note",
      "env.field.commit": "commit sha",
      "env.field.created": "created",
      "env.field.lastSeen": "last seen",
      "env.field.updated": "updated",
      "env.directNote":
        "no Traefik configured — this is Oxid's own stable proxy port for this branch, not {url}. It stays the same across redeploys: a push swaps traffic to the new container with zero downtime instead of changing the address.",
      "env.tab.overview": "overview",
      "env.tab.logs": "logs",
      "env.tab.history": "history",
      "env.noAudit": "No audit events.",

      // ---- projects -------------------------------------------------------
      "project.title": "Projects",
      "project.col.name": "Name",
      "project.col.repo": "Repo",
      "project.col.port": "Port",
      "project.col.environments": "Environments",
      "project.notFound": "Project not found.",
      "project.deleteProject": "delete project",
      "project.field.containerPort": "container port",
      "project.field.containerPortTitle":
        "What the app listens on inside the container — the host-side published port is chosen dynamically per environment, see each branch's URL below",
      "project.field.dockerfile": "dockerfile",
      "project.field.context": "context",
      "project.field.memoryLimit": "memory limit",
      "project.field.cpuLimit": "cpu limit",
      "project.field.dependencies": "dependencies",
      "project.lifetime.title": "Lifetime policy",
      "project.lifetime.help":
        "Idle/scale-to-zero timers — {file} only ever seeds these once, at first registration. Changes here take effect on the next GC sweep, no redeploy.",
      "project.lifetime.pausePlaceholder": "e.g. 30m",
      "project.lifetime.destroyPlaceholder": "e.g. 7d",
      "project.git.title": "Private repo access",
      "project.git.help":
        "Only needed for a private repository — a GitHub personal access token (or equivalent) letting this daemon clone/fetch it on its own. Write-only: never shown back once saved.",
      "project.git.placeholder": "e.g. ghp_..., leave blank to keep unchanged",
      "project.deployPlaceholder": "branch to deploy",
      "project.environments": "Environments",
      "project.noBranches": "No live branches.",

      // ---- secrets ---------------------------------------------------------
      "secrets.globalTitle": "Global secrets",
      "secrets.projectTitle": "Secrets — {name}",
      "secrets.help":
        "Values are write-only — the daemon never returns a stored value, only names and scope.",
      "secrets.empty": "No secrets in this scope.",
      "secrets.setTitle": "Set a secret",
      "secrets.namePlaceholder": "NAME",
      "secrets.scope.project": "project",
      "secrets.scope.branch": "branch",
      "secrets.branchPlaceholder": "branch (if scope=branch)",
      "secrets.valuePlaceholder": "value",

      // ---- queue -----------------------------------------------------------
      "queue.title": "Deploy queue",
      "queue.empty": "Empty — every deploy has capacity.",
      "queue.col.position": "#",
      "queue.col.project": "Project",
      "queue.col.branch": "Branch",
      "queue.col.operator": "Operator",
      "queue.col.requested": "Requested",

      // ---- audit -----------------------------------------------------------
      "audit.title": "Audit trail",
      "audit.limitPlaceholder": "limit",
      "audit.searchPlaceholder": "search kind/detail/operator...",
      "audit.col.when": "When",
      "audit.col.kind": "Kind",
      "audit.col.branch": "Branch",
      "audit.col.deployTime": "Deploy time",
      "audit.col.deployTimeTitle":
        "Wall-clock time from the environment's push/creation to this outcome",
      "audit.col.operator": "Operator",
      "audit.col.detail": "Detail",
      "audit.anonymous": "anonymous",
      "audit.envTitle": "environment #{id}",

      // ---- admin -----------------------------------------------------------
      "admin.title": "Admin",
      "admin.rerunWizard": "re-run setup wizard",
      "admin.downloadBackup": "download backup",
      "admin.rotateKey": "rotate master key",
      "admin.tokensTitle": "API tokens",
      "admin.tokenNamePlaceholder": "token name",
      "admin.noTokens": "No tokens yet, or this token isn't the master credential.",
      "admin.revoked": "(revoked)",
      "admin.revoke": "revoke",

      // ---- onboarding wizard ------------------------------------------------
      "wizard.title": "Set up this daemon",
      "wizard.skip": "skip — I'll configure it myself",
      "wizard.intro":
        "Five steps from a fresh install to a deployed project. Everything here can also be done later via the CLI or the pages above.",
      "wizard.step1": "1 · token",
      "wizard.step2": "2 · infrastructure",
      "wizard.step3": "3 · first project",
      "wizard.step4": "4 · webhooks",
      "wizard.step5": "5 · CLI & team",
      "wizard.s1.title": "Connect to the control API",
      "wizard.s1.open":
        "This daemon has no API token configured (loopback/open mode) — you're already authenticated. Continue straight to infrastructure.",
      "wizard.s1.paste":
        "Paste the bearer token below. It's kept in this browser only ({storage}) and sent as {header} on every request.",
      "wizard.s1.autoToken":
        "This daemon runs with {flag}: it already generated one for itself on first start. Fetch and use it directly — no logs to dig through:",
      "wizard.s1.generate": "generate automatically",
      "wizard.s1.orPaste": "— or paste one yourself:",
      "wizard.s1.tokenPlaceholder": "bearer token",
      "wizard.s1.verify": "verify & continue",
      "wizard.s1.noStatus": "Cannot read setup status — is the daemon reachable?",
      "wizard.s2.title": "Infrastructure",
      "wizard.s2.help":
        "Traefik mode routes every branch as {pattern} with wake-on-request. Direct-publish mode (no {var}) is also fine: each environment just gets its own host port.",
      "wizard.s2.checking": "checking…",
      "wizard.s2.directMode":
        "Direct-publish mode — nothing to bootstrap here. Deployed environments will publish a dynamically-chosen host port each. Continue to the first project.",
      "wizard.s2.network": "Docker network",
      "wizard.s2.traefik": "Traefik proxy —",
      "wizard.s2.wired": "Daemon wired into the network",
      "wizard.s2.fix": "fix automatically (idempotent)",
      "wizard.s3.title": "Register your first project",
      "wizard.s3.tabUrl": "git URL",
      "wizard.s3.tabDir": "local path",
      "wizard.s3.urlHelp":
        "The daemon clones the repo itself into its own git cache — no shared filesystem needed. scp-style remotes are normalized automatically; for a private repo add an HTTPS access token below (stored encrypted, never echoed back).",
      "wizard.s3.tokenPlaceholder": "access token (private repos only)",
      "wizard.s3.dirHelp":
        "Path to a checkout the daemon container can read — with the shipped compose file that's anything under the mounted {dir} directory ({inner} inside the container).",
      "wizard.s3.branch": "branch",
      "wizard.s3.register": "register & deploy",
      "wizard.s3.building": "building… first builds may take a few minutes on a cold host.",
      "wizard.s3.live": "is live!",
      "wizard.s3.openEnv": "open environment",
      "wizard.s3.failed": "✗ deploy didn't make it:",
      "wizard.s3.failedHelp":
        "Check the audit page for the build log, fix, then try again from here.",
      "wizard.s4.title": "Wire up push-to-deploy",
      "wizard.s4.help":
        "Add a webhook in your Git host pointing at this URL; every pushed branch deploys itself (and deleting a branch destroys its environment). Requires the master credential.",
      "wizard.s4.secretHint":
        "Webhook secret (auto-generated on this daemon) — paste it as your Git host's webhook secret:",
      "wizard.s4.secretMissing":
        "No webhook secret configured yet. Set {var} (or start with {flag}) and restart the daemon — until then pushes are rejected by design.",
      "wizard.s5.title": "CLI access & team tokens",
      "wizard.s5.point": "Point the {cli} CLI at this daemon:",
      "wizard.s5.share":
        "Sharing with teammates? Mint them scoped tokens instead of handing out the master one — {cmd} (or the admin page). Scoped tokens see nothing outside their projects. Or register projects programmatically:",
      "wizard.finish": "finish → go to environments",
      "wizard.back": "← back",
      "wizard.continue": "continue →",

      // ---- shared actions ---------------------------------------------------
      "action.pause": "pause",
      "action.wake": "wake",
      "action.destroy": "destroy",
      "action.rollback": "rollback",
      "action.open": "open",
      "action.save": "save",
      "action.clear": "clear",
      "action.delete": "delete",
      "action.deploy": "deploy",
      "action.set": "set",
      "action.create": "create",
      "action.copy": "copy",
      "action.copied": "copied!",
      "action.cancel": "cancel",
      "action.confirm": "confirm",
      "action.secrets": "secrets",

      // ---- runtime notices --------------------------------------------------
      "notice.loadFailed": "Failed to load page data: {error}",
      "notice.tokenAccepted": "Token accepted.",
      "notice.tokenRejected":
        "That token was rejected. If OXID_AUTO_TOKEN is on, retrieve the generated one with: docker compose logs oxid-daemon | grep -A2 Generated",
      "notice.tokenGenerated": "Token generated and saved.",
      "notice.tokenGenerateFailed":
        "Couldn't fetch a token automatically — paste one manually below, or check the daemon logs.",
      "notice.masterOnly": "This step needs the master token (scoped tokens can't change infra).",
      "notice.registered": "Project `{name}` registered — deploying...",
      "notice.registerFailed": "Registration failed: {error}",
      "notice.stillBuilding": "Still building after 3 minutes — check the environment page for live logs.",
      "notice.envState": "Environment state: {state}",
      "notice.clipboard": "Clipboard unavailable — copy manually.",
      "notice.paused": "Paused `{branch}`.",
      "notice.woken": "Woke `{branch}`.",
      "notice.destroyed": "Destroyed `{branch}`.",
      "notice.projectDeleted": "Deleted project `{name}`.",
      "notice.settingsFailed": "Updating settings failed: {error}",
      "notice.enterToken": 'Enter a token first, or use "clear" to remove it.',
      "notice.gitTokenSaved": "Saved git token for `{name}`.",
      "notice.gitTokenFailed": "Saving git token failed: {error}",
      "notice.gitTokenCleared": "Cleared git token for `{name}`.",
      "notice.gitTokenClearFailed": "Clearing git token failed: {error}",
      "notice.deployQueued": "`{branch}` queued for capacity (position {position}).",
      "notice.deployStarted": "`{branch}` deployed.",
      "notice.deployFailed": "Deploy failed: {error}",
      "notice.rolledBack": "Rolled back `{branch}`.",
      "notice.rollbackFailed": "Rollback failed: {error}",
      "notice.setupComplete": "Setup complete.",
      "confirm.destroyEnv": "Destroy environment `{branch}`? This cannot be undone.",
      "confirm.deleteProject":
        "Permanently delete project `{name}`? This destroys every environment and all its secrets.",
      "confirm.clearGitToken": "Clear the git token for `{name}`?",
      "notice.settingsUpdated":
        "Updated `{name}`: pause_after={pause} destroy_after={destroy}",
      "notice.secretRequired": "Secret name and value are both required.",
      "notice.secretSet": "Secret `{name}` set.",
      "notice.secretFailed": "Setting secret failed: {error}",
      "confirm.deleteSecret": "Delete secret `{name}` ({scope})?",
      "notice.secretDeleted": "Secret `{name}` deleted.",
      "notice.secretDeleteFailed": "Deleting secret failed: {error}",
      "notice.tokenCreateFailed": "Creating token failed: {error}",
      "confirm.revokeToken": "Revoke token `{name}`?",
      "confirm.rollback": "Roll back `{branch}` to the deploy immediately before this one?",
      "notice.tokenCreated":
        "Token created for `{name}`: {token} — copy it now, it won't be shown again.",
      "notice.tokenRevoked": "Revoked token `{name}`.",
      "confirm.rotateKey":
        "Rotate the master encryption key? Every secret is re-encrypted with zero downtime, but this cannot be undone.",
      "notice.keyRotated": "Master key rotated.",
      "notice.keyRotationFailed": "Key rotation failed: {error}",
      "notice.backupDownloaded": "Backup downloaded.",
      "notice.backupFailed": "Backup failed: {error}",
    },

    es: {
      // ---- chrome -------------------------------------------------------
      "app.tagline": "plano de control",
      "app.title": "Oxid — Plano de control",
      "nav.environments": "entornos",
      "nav.projects": "proyectos",
      "nav.queue": "cola",
      "nav.audit": "auditoría",
      "nav.secrets": "secretos",
      "nav.admin": "admin",
      "nav.setup": "configurar",
      "nav.setupTitle": "Asistente de configuración inicial",
      "conn.connected": "Conectado",
      "conn.unreachable": "Inalcanzable",
      "conn.tokenPlaceholder": "Token bearer (si está configurado)",
      "conn.refresh": "Actualizar ahora",
      "conn.language": "Idioma",
      "banner.authError": "[!] El daemon respondió 401/403 — revisa el token de arriba.",
      "banner.connError": "[!] No se puede contactar con el daemon en",
      "banner.dismiss": "[clic para descartar]",
      "footer.tagline": "Entornos efímeros que respiran.",
      "footer.autoRefresh": "actualización automática {secs}s",

      // ---- stats --------------------------------------------------------
      "stats.projects": "Proyectos",
      "stats.running": "Corriendo",
      "stats.paused": "Dormidos",
      "stats.building": "Construyendo",
      "stats.hibernating": "Hibernando",
      "stats.failed": "Fallidos",
      "stats.queued": "En cola",
      "stats.hostMemory": "Memoria del host",

      // ---- environment states -------------------------------------------
      "state.running": "corriendo",
      "state.paused": "dormido",
      "state.building": "construyendo",
      "state.hibernating": "hibernando",
      "state.build_failed": "build fallido",
      "state.destroyed": "destruido",

      // ---- environments list ---------------------------------------------
      "env.title": "Entornos",
      "env.noTraefik":
        "[~] Sin Traefik configurado ({var} no está definido) — las URLs de abajo apuntan al puerto del host que Docker asigna a cada entorno, en vez de a un subdominio. Docker elige un puerto libre por despliegue, así que varias ramas del mismo proyecto pueden convivir aunque no haya Traefik; lo que no es estable es el puerto entre redespliegues.",
      "env.allProjects": "todos los proyectos",
      "env.allStates": "todos los estados",
      "env.searchBranch": "buscar rama...",
      "env.empty": "Ningún entorno coincide. Ejecuta {cmd}, o limpia los filtros.",
      "env.col.project": "Proyecto",
      "env.col.branch": "Rama",
      "env.col.state": "Estado",
      "env.col.url": "URL",
      "env.col.created": "Creado",
      "env.col.lastSeen": "Última visita",
      "env.col.updated": "Actualizado",

      // ---- environment detail ----------------------------------------------
      "env.notFound": "Entorno no encontrado (destruido, o el id no es correcto).",
      "env.field.id": "id",
      "env.field.url": "url",
      "env.field.note": "nota",
      "env.field.commit": "sha del commit",
      "env.field.created": "creado",
      "env.field.lastSeen": "última visita",
      "env.field.updated": "actualizado",
      "env.directNote":
        "sin Traefik configurado — este es el puerto estable del propio proxy de Oxid para esta rama, no {url}. No cambia entre redespliegues: un push mueve el tráfico al contenedor nuevo sin caída, en vez de cambiar la dirección.",
      "env.tab.overview": "resumen",
      "env.tab.logs": "logs",
      "env.tab.history": "historial",
      "env.noAudit": "Sin eventos de auditoría.",

      // ---- projects ----------------------------------------------------------
      "project.title": "Proyectos",
      "project.col.name": "Nombre",
      "project.col.repo": "Repositorio",
      "project.col.port": "Puerto",
      "project.col.environments": "Entornos",
      "project.notFound": "Proyecto no encontrado.",
      "project.deleteProject": "eliminar proyecto",
      "project.field.containerPort": "puerto del contenedor",
      "project.field.containerPortTitle":
        "Donde escucha la app dentro del contenedor — el puerto publicado en el host se elige dinámicamente por entorno, mira la URL de cada rama abajo",
      "project.field.dockerfile": "dockerfile",
      "project.field.context": "contexto",
      "project.field.memoryLimit": "límite de memoria",
      "project.field.cpuLimit": "límite de cpu",
      "project.field.dependencies": "dependencias",
      "project.lifetime.title": "Política de vida",
      "project.lifetime.help":
        "Temporizadores de inactividad y escalado a cero — {file} solo los siembra una vez, al registrar el proyecto. Los cambios aquí se aplican en el siguiente barrido del recolector, sin redesplegar.",
      "project.lifetime.pausePlaceholder": "p. ej. 30m",
      "project.lifetime.destroyPlaceholder": "p. ej. 7d",
      "project.git.title": "Acceso a repositorio privado",
      "project.git.help":
        "Solo hace falta para un repositorio privado — un token de acceso personal de GitHub (o equivalente) para que este daemon pueda clonarlo por su cuenta. Solo escritura: no se muestra de vuelta una vez guardado.",
      "project.git.placeholder": "p. ej. ghp_..., déjalo vacío para no cambiarlo",
      "project.deployPlaceholder": "rama a desplegar",
      "project.environments": "Entornos",
      "project.noBranches": "No hay ramas vivas.",

      // ---- secrets -----------------------------------------------------------
      "secrets.globalTitle": "Secretos globales",
      "secrets.projectTitle": "Secretos — {name}",
      "secrets.help":
        "Los valores son de solo escritura — el daemon nunca devuelve un valor guardado, solo nombres y ámbito.",
      "secrets.empty": "No hay secretos en este ámbito.",
      "secrets.setTitle": "Definir un secreto",
      "secrets.namePlaceholder": "NOMBRE",
      "secrets.scope.project": "proyecto",
      "secrets.scope.branch": "rama",
      "secrets.branchPlaceholder": "rama (si el ámbito es rama)",
      "secrets.valuePlaceholder": "valor",

      // ---- queue -------------------------------------------------------------
      "queue.title": "Cola de despliegues",
      "queue.empty": "Vacía — todos los despliegues tienen capacidad.",
      "queue.col.position": "#",
      "queue.col.project": "Proyecto",
      "queue.col.branch": "Rama",
      "queue.col.operator": "Operador",
      "queue.col.requested": "Solicitado",

      // ---- audit -------------------------------------------------------------
      "audit.title": "Traza de auditoría",
      "audit.limitPlaceholder": "límite",
      "audit.searchPlaceholder": "buscar tipo/detalle/operador...",
      "audit.col.when": "Cuándo",
      "audit.col.kind": "Tipo",
      "audit.col.branch": "Rama",
      "audit.col.deployTime": "Tiempo de despliegue",
      "audit.col.deployTimeTitle":
        "Tiempo real transcurrido desde el push o la creación del entorno hasta este resultado",
      "audit.col.operator": "Operador",
      "audit.col.detail": "Detalle",
      "audit.anonymous": "anónimo",
      "audit.envTitle": "entorno n.º {id}",

      // ---- admin -------------------------------------------------------------
      "admin.title": "Administración",
      "admin.rerunWizard": "repetir el asistente",
      "admin.downloadBackup": "descargar copia de seguridad",
      "admin.rotateKey": "rotar la clave maestra",
      "admin.tokensTitle": "Tokens de API",
      "admin.tokenNamePlaceholder": "nombre del token",
      "admin.noTokens": "Aún no hay tokens, o este token no es la credencial maestra.",
      "admin.revoked": "(revocado)",
      "admin.revoke": "revocar",

      // ---- onboarding wizard --------------------------------------------------
      "wizard.title": "Configura este daemon",
      "wizard.skip": "omitir — lo configuro yo",
      "wizard.intro":
        "Cinco pasos desde una instalación limpia hasta un proyecto desplegado. Todo esto se puede hacer también más tarde desde el CLI o desde las páginas de arriba.",
      "wizard.step1": "1 · token",
      "wizard.step2": "2 · infraestructura",
      "wizard.step3": "3 · primer proyecto",
      "wizard.step4": "4 · webhooks",
      "wizard.step5": "5 · CLI y equipo",
      "wizard.s1.title": "Conecta con la API de control",
      "wizard.s1.open":
        "Este daemon no tiene token de API configurado (modo loopback/abierto) — ya estás autenticado. Sigue directo a la infraestructura.",
      "wizard.s1.paste":
        "Pega abajo el token bearer. Se guarda solo en este navegador ({storage}) y se envía como {header} en cada petición.",
      "wizard.s1.autoToken":
        "Este daemon corre con {flag}: ya generó uno para sí mismo al arrancar. Recupéralo y úsalo directamente, sin rebuscar en los logs:",
      "wizard.s1.generate": "generar automáticamente",
      "wizard.s1.orPaste": "— o pega uno tú:",
      "wizard.s1.tokenPlaceholder": "token bearer",
      "wizard.s1.verify": "verificar y continuar",
      "wizard.s1.noStatus": "No se puede leer el estado de configuración — ¿el daemon responde?",
      "wizard.s2.title": "Infraestructura",
      "wizard.s2.help":
        "El modo Traefik enruta cada rama como {pattern} con despertar bajo demanda. El modo de publicación directa (sin {var}) también sirve: cada entorno recibe su propio puerto del host.",
      "wizard.s2.checking": "comprobando…",
      "wizard.s2.directMode":
        "Modo de publicación directa — no hay nada que arrancar aquí. Cada entorno desplegado publicará un puerto del host elegido dinámicamente. Sigue al primer proyecto.",
      "wizard.s2.network": "Red de Docker",
      "wizard.s2.traefik": "Proxy Traefik —",
      "wizard.s2.wired": "Daemon conectado a la red",
      "wizard.s2.fix": "arreglar automáticamente (idempotente)",
      "wizard.s3.title": "Registra tu primer proyecto",
      "wizard.s3.tabUrl": "URL de git",
      "wizard.s3.tabDir": "ruta local",
      "wizard.s3.urlHelp":
        "El daemon clona el repositorio él mismo en su propia caché de git — no hace falta un sistema de ficheros compartido. Los remotos estilo scp se normalizan automáticamente; para un repositorio privado añade abajo un token de acceso HTTPS (se guarda cifrado y nunca se devuelve).",
      "wizard.s3.tokenPlaceholder": "token de acceso (solo repositorios privados)",
      "wizard.s3.dirHelp":
        "Ruta a un checkout que el contenedor del daemon pueda leer — con el fichero compose que se distribuye, cualquier cosa bajo el directorio montado {dir} ({inner} dentro del contenedor).",
      "wizard.s3.branch": "rama",
      "wizard.s3.register": "registrar y desplegar",
      "wizard.s3.building":
        "construyendo… el primer build puede tardar unos minutos en un host frío.",
      "wizard.s3.live": "está vivo!",
      "wizard.s3.openEnv": "abrir entorno",
      "wizard.s3.failed": "✗ el despliegue no salió adelante:",
      "wizard.s3.failedHelp":
        "Mira el log del build en la página de auditoría, corrígelo y vuelve a intentarlo desde aquí.",
      "wizard.s4.title": "Conecta el push-to-deploy",
      "wizard.s4.help":
        "Añade un webhook en tu servidor Git apuntando a esta URL; cada rama que se empuje se despliega sola (y borrar una rama destruye su entorno). Requiere la credencial maestra.",
      "wizard.s4.secretHint":
        "Secreto del webhook (generado por este daemon) — pégalo como secreto del webhook en tu servidor Git:",
      "wizard.s4.secretMissing":
        "Todavía no hay secreto de webhook configurado. Define {var} (o arranca con {flag}) y reinicia el daemon — hasta entonces los pushes se rechazan a propósito.",
      "wizard.s5.title": "Acceso por CLI y tokens de equipo",
      "wizard.s5.point": "Apunta el CLI {cli} a este daemon:",
      "wizard.s5.share":
        "¿Vas a compartirlo con el equipo? Emíteles tokens con alcance en vez de repartir el maestro — {cmd} (o desde la página de administración). Un token con alcance no ve nada fuera de sus proyectos. O registra proyectos de forma programática:",
      "wizard.finish": "terminar → ir a entornos",
      "wizard.back": "← atrás",
      "wizard.continue": "continuar →",

      // ---- shared actions -----------------------------------------------------
      "action.pause": "dormir",
      "action.wake": "despertar",
      "action.destroy": "destruir",
      "action.rollback": "revertir",
      "action.open": "abrir",
      "action.save": "guardar",
      "action.clear": "limpiar",
      "action.delete": "borrar",
      "action.deploy": "desplegar",
      "action.set": "definir",
      "action.create": "crear",
      "action.copy": "copiar",
      "action.copied": "¡copiado!",
      "action.cancel": "cancelar",
      "action.confirm": "confirmar",
      "action.secrets": "secretos",

      // ---- runtime notices ----------------------------------------------------
      "notice.loadFailed": "No se pudieron cargar los datos de la página: {error}",
      "notice.tokenAccepted": "Token aceptado.",
      "notice.tokenRejected":
        "Ese token fue rechazado. Si OXID_AUTO_TOKEN está activo, recupera el generado con: docker compose logs oxid-daemon | grep -A2 Generated",
      "notice.tokenGenerated": "Token generado y guardado.",
      "notice.tokenGenerateFailed":
        "No se pudo obtener un token automáticamente — pega uno a mano abajo, o revisa los logs del daemon.",
      "notice.masterOnly":
        "Este paso necesita el token maestro (los tokens con alcance no pueden cambiar la infraestructura).",
      "notice.registered": "Proyecto `{name}` registrado — desplegando...",
      "notice.registerFailed": "Falló el registro: {error}",
      "notice.stillBuilding":
        "Sigue construyendo tras 3 minutos — mira los logs en vivo en la página del entorno.",
      "notice.envState": "Estado del entorno: {state}",
      "notice.clipboard": "Portapapeles no disponible — copia a mano.",
      "notice.paused": "`{branch}` dormido.",
      "notice.woken": "`{branch}` despertado.",
      "notice.destroyed": "`{branch}` destruido.",
      "notice.projectDeleted": "Proyecto `{name}` eliminado.",
      "notice.settingsFailed": "No se pudieron actualizar los ajustes: {error}",
      "notice.enterToken": 'Escribe un token primero, o usa "limpiar" para quitarlo.',
      "notice.gitTokenSaved": "Token de git guardado para `{name}`.",
      "notice.gitTokenFailed": "No se pudo guardar el token de git: {error}",
      "notice.gitTokenCleared": "Token de git borrado para `{name}`.",
      "notice.gitTokenClearFailed": "No se pudo borrar el token de git: {error}",
      "notice.deployQueued": "`{branch}` en cola esperando capacidad (posición {position}).",
      "notice.deployStarted": "`{branch}` desplegado.",
      "notice.deployFailed": "Falló el despliegue: {error}",
      "notice.rolledBack": "`{branch}` revertido.",
      "notice.rollbackFailed": "Falló la reversión: {error}",
      "notice.setupComplete": "Configuración completada.",
      "confirm.destroyEnv": "¿Destruir el entorno `{branch}`? Esto no se puede deshacer.",
      "confirm.deleteProject":
        "¿Eliminar permanentemente el proyecto `{name}`? Esto destruye todos sus entornos y secretos.",
      "confirm.clearGitToken": "¿Borrar el token de git de `{name}`?",
      "notice.settingsUpdated":
        "`{name}` actualizado: pause_after={pause} destroy_after={destroy}",
      "notice.secretRequired": "Hacen falta tanto el nombre como el valor del secreto.",
      "notice.secretSet": "Secreto `{name}` definido.",
      "notice.secretFailed": "No se pudo definir el secreto: {error}",
      "confirm.deleteSecret": "¿Borrar el secreto `{name}` ({scope})?",
      "notice.secretDeleted": "Secreto `{name}` borrado.",
      "notice.secretDeleteFailed": "No se pudo borrar el secreto: {error}",
      "notice.tokenCreateFailed": "No se pudo crear el token: {error}",
      "confirm.revokeToken": "¿Revocar el token `{name}`?",
      "confirm.rollback": "¿Revertir `{branch}` al despliegue inmediatamente anterior?",
      "notice.tokenCreated":
        "Token creado para `{name}`: {token} — cópialo ahora, no se volverá a mostrar.",
      "notice.tokenRevoked": "Token `{name}` revocado.",
      "confirm.rotateKey":
        "¿Rotar la clave maestra de cifrado? Todos los secretos se vuelven a cifrar sin caída de servicio, pero esto no se puede deshacer.",
      "notice.keyRotated": "Clave maestra rotada.",
      "notice.keyRotationFailed": "Falló la rotación de la clave: {error}",
      "notice.backupDownloaded": "Copia de seguridad descargada.",
      "notice.backupFailed": "Falló la copia de seguridad: {error}",
    },
  };

  const FALLBACK = "en";
  const STORAGE_KEY = "oxid.locale";

  /** Locales this build ships, for the switcher. */
  function available() {
    return LOCALES.slice();
  }

  /**
   * The locale to start in: an explicit earlier choice, else the closest
   * match for the browser's own languages, else English. Matching is on the
   * primary subtag, so `es-419` and `es-MX` both land on `es`.
   */
  function detect() {
    let stored = null;
    try {
      stored = window.localStorage.getItem(STORAGE_KEY);
    } catch {
      // Private mode, or site data blocked. Fall through to the browser's
      // languages — a missing preference is not an error worth surfacing.
    }
    if (stored && CATALOG[stored]) {
      return stored;
    }
    const preferred = navigator.languages ?? [navigator.language ?? ""];
    for (const tag of preferred) {
      const primary = String(tag).toLowerCase().split("-")[0];
      if (CATALOG[primary]) {
        return primary;
      }
    }
    return FALLBACK;
  }

  /** Remembers a choice so a reload keeps it. Best-effort. */
  function persist(locale) {
    try {
      window.localStorage.setItem(STORAGE_KEY, locale);
    } catch {
      // Nothing to do: the switcher still works for this page view.
    }
  }

  /**
   * Looks `key` up in `locale`, falling back to English and finally to the
   * key itself — a missing translation shows something recognisable instead
   * of an empty element, and the key is greppable.
   *
   * `{placeholders}` are substituted from `params`. Substitution is textual
   * and the result is only ever assigned through `x-text`/attribute
   * bindings, never `innerHTML`, so a value carrying markup is inert.
   */
  function translate(locale, key, params) {
    const table = CATALOG[locale] ?? CATALOG[FALLBACK];
    let value = table[key] ?? CATALOG[FALLBACK][key] ?? key;
    if (params) {
      for (const [name, replacement] of Object.entries(params)) {
        value = value.split(`{${name}}`).join(String(replacement));
      }
    }
    return value;
  }

  /**
   * Keys a locale is missing relative to English. Not used by the UI —
   * a translator's console check: `OxidI18n.missingKeys('es')`.
   */
  function missingKeys(locale) {
    const table = CATALOG[locale] ?? {};
    return Object.keys(CATALOG[FALLBACK]).filter((key) => !(key in table));
  }

  window.OxidI18n = { available, detect, persist, translate, missingKeys };
})();
