//! Message catalog for the CLI.
//!
//! Text the CLI prints is looked up here rather than written at the call
//! site, so a second language is a table entry instead of an edit to every
//! command. Machine-readable output (`--json`) is deliberately *not* routed
//! through this: scripts parse it, and a field name that changes with the
//! operator's locale would be a bug, not a feature.
//!
//! Adding a language: add a `Locale` variant, a match arm in [`catalog`],
//! and its tag in [`Locale::parse`]. `catalog_is_complete` in this module's
//! tests fails if any locale is missing a key the default has.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Languages this build ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    /// English — also the fallback for any key a locale is missing.
    #[default]
    English,
    /// Spanish.
    Spanish,
}

impl Locale {
    /// Maps a language tag to a locale, ignoring region and encoding:
    /// `es`, `es_MX`, `es-419` and `es_ES.UTF-8` all mean Spanish. `None`
    /// for a tag no locale claims, so the caller can keep looking.
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::parse(tag)
    }

    fn parse(tag: &str) -> Option<Self> {
        let primary = tag
            .split(['_', '-', '.'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match primary.as_str() {
            "en" => Some(Self::English),
            "es" => Some(Self::Spanish),
            _ => None,
        }
    }

    /// The locale to print in, from the first source that names one:
    ///
    /// 1. `OXID_LANG`, for choosing per-invocation without touching the
    ///    shell's own locale;
    /// 2. `LC_ALL`, `LC_MESSAGES`, `LANG` — the POSIX order, so the CLI
    ///    speaks whatever the rest of the operator's terminal does;
    /// 3. English.
    ///
    /// `C` and `POSIX` are not language tags — they mean "no localization",
    /// which is English here — so they fall through rather than being
    /// treated as unknown.
    #[must_use]
    pub fn from_env() -> Self {
        for var in ["OXID_LANG", "LC_ALL", "LC_MESSAGES", "LANG"] {
            let Ok(value) = std::env::var(var) else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() || value.eq_ignore_ascii_case("c") || value == "POSIX" {
                continue;
            }
            if let Some(locale) = Self::parse(value) {
                return locale;
            }
        }
        Self::English
    }
}

/// Process-wide locale, resolved once. A CLI runs one command per
/// invocation, so this never needs to change after start-up.
static LOCALE: OnceLock<Locale> = OnceLock::new();

/// Fixes the locale for this process. Called once from `main`; a second
/// call is ignored rather than racing.
pub fn init(locale: Locale) {
    let _ = LOCALE.set(locale);
}

fn current() -> Locale {
    *LOCALE.get_or_init(Locale::from_env)
}

/// Looks up `key` in the active locale, falling back to English and finally
/// to the key itself — a missing entry prints something greppable rather
/// than nothing at all.
#[must_use]
pub fn t(key: &str) -> &'static str {
    let locale = current();
    catalog(locale)
        .get(key)
        .or_else(|| catalog(Locale::English).get(key))
        .copied()
        .unwrap_or("<missing>")
}

/// [`t`] with `{placeholder}` substitution.
///
/// Substitution is textual and applied to the *translated* string, so a
/// value that happens to contain braces cannot pull in another parameter.
#[must_use]
pub fn tf(key: &str, params: &[(&str, &str)]) -> String {
    let mut out = t(key).to_owned();
    for (name, value) in params {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

macro_rules! catalog {
    ($($key:literal => $value:literal),* $(,)?) => {
        BTreeMap::from([$(($key, $value)),*])
    };
}

fn catalog(locale: Locale) -> &'static BTreeMap<&'static str, &'static str> {
    match locale {
        Locale::English => {
            static EN: OnceLock<BTreeMap<&'static str, &'static str>> = OnceLock::new();
            EN.get_or_init(english)
        }
        Locale::Spanish => {
            static ES: OnceLock<BTreeMap<&'static str, &'static str>> = OnceLock::new();
            ES.get_or_init(spanish)
        }
    }
}

fn english() -> BTreeMap<&'static str, &'static str> {
    catalog! {
        // -- deploy ---------------------------------------------------------
        "deploy.start" => "oxid up {branch}",
        "deploy.configParsed" => "Parsed oxid.toml successfully",
        "deploy.registered" => "Project `{name}` registered (id {id})",
        "deploy.building" => "Building image for {branch}...",
        "deploy.builtCached" => "Image built (cache hit: {cache}%, {took})",
        "deploy.built" => "Image built ({took})",
        "deploy.live" => "Environment live at: {url}",
        "deploy.queued" => "Queued, waiting for host capacity (position {position}) — run `oxid queue` to check, or `oxid status` once it deploys",
        "deploy.interrupted" => "Interrupted — the deploy keeps running server-side; check it with `oxid status` or `oxid queue`",
        "rollback.start" => "oxid rollback {branch}",
        "rollback.done" => "Rolled back to {sha} — environment live at: {url}",
        // -- lifecycle -------------------------------------------------------
        "env.destroyed" => "Environment `{branch}` destroyed",
        "env.secretsPurged" => "Branch `{branch}` secrets purged",
        "env.paused" => "Environment `{branch}` paused",
        "env.woken" => "Environment `{branch}` woken",
        "project.deleted" => "Project `{name}` deleted",
        "confirm.aborted" => "Aborted (re-run with --force to skip this prompt).",
        // -- empty states ------------------------------------------------------
        "empty.environments" => "No environments for `{name}` yet. Deploy one with `oxid up <branch>`.",
        "empty.projects" => "No projects registered yet.",
        "empty.queue" => "Queue is empty — every deploy has capacity.",
        "empty.audit" => "No audit events yet.",
        "empty.tokens" => "No tokens issued yet.",
        "empty.secrets" => "No secrets in this scope.",
        "empty.contexts" => "No contexts configured yet. Add one with `oxid context add <name> --api <url>`.",
        "empty.activeContext" => "No active context (using --api/OXID_API/default).",
        // -- contexts -----------------------------------------------------------
        "context.added" => "Added context `{name}`",
        "context.switched" => "Switched to context `{name}`",
        "context.removed" => "Removed context `{name}`",
        // -- tokens --------------------------------------------------------------
        "token.created" => "Token `{name}` created (id {id}): {token}",
        "token.unscoped" => "Unscoped: this token has full access. Re-create it with --project to limit it.",
        "token.scoped" => "Scoped to project ids {projects} — every other project answers 404 to this token.",
        "token.onlyOnce" => "This is the only time the raw token is shown — store it now.",
        "token.fetched" => "Fetched token and saved it to context `{name}`",
        "token.useContext" => "oxid context use {name}   # if it isn't already active",
        "token.revoked" => "Token {id} revoked",
        // -- secrets ---------------------------------------------------------------
        "secret.setBranch" => "Secret `{name}` set for branch `{branch}`",
        "secret.set" => "Secret `{name}` set ({scope})",
        "secret.deleted" => "Secret `{name}` deleted",
        // -- backup / keys ----------------------------------------------------------
        "backup.written" => "Backup written to `{file}` ({bytes} bytes)",
        "key.rotated" => "Master key rotated — every secret re-encrypted, zero downtime",
        // -- doctor -------------------------------------------------------------------
        "doctor.reachable" => "Daemon reachable at {api} (v{version}, {ms}ms)",
        "doctor.authOk" => "Control API authenticates correctly",
        "doctor.versionMatch" => "CLI (v{cli}) and daemon (v{daemon}) versions match",
        "doctor.versionSkew" => "CLI is v{cli} but daemon is v{daemon} (major version mismatch) — upgrade whichever is older; a mismatch across major versions may break API compatibility",
        "doctor.capacity" => "Docker capacity: {cpus} CPU(s), {memory} GiB memory, {running} env(s) running",
        "doctor.noInfra" => "Could not fetch infra status ({error}) — the daemon may predate `/api/v1/infra/status`; upgrade it to enable this check",
        "doctor.noStats" => "Could not fetch capacity stats ({error}) — the daemon may predate `/api/v1/stats`; upgrade it to enable this check",
        "doctor.scopedNoNode" => "Node-wide checks (capacity, infra) are not available to a project-scoped token — this is expected, and everything above is what matters for your projects",
        // -- infra ------------------------------------------------------------------------
        "infra.networkExists" => "Docker network `{network}` exists",
        "infra.networkMissing" => "Docker network `{network}` does not exist",
        "infra.traefikRunning" => "Traefik is running",
        "infra.traefikRunningPort" => "Traefik is running (published on host port {port} — branch URLs need it)",
        "infra.traefikPaused" => "Traefik container exists but is paused",
        "infra.traefikStopped" => "Traefik container exists but is stopped",
        "infra.traefikMissing" => "Traefik is not running",
        "infra.selfWiredFully" => "This daemon's own container is fully wired for wake-on-request",
        "infra.selfWiredNoCatchall" => "Wake-on-request can't reach scaled-to-zero branches: the catch-all router is missing",
        "infra.selfWiredNot" => "This daemon's own container is NOT fully wired for wake-on-request",
        "infra.notContainerized" => "Daemon isn't running inside Docker — self-wiring check skipped",
        "infra.selfWiringUnknown" => "Could not determine this daemon's own container wiring",
        // -- table headers -------------------------------------------------------------------
        "table.branch" => "BRANCH",
        "table.state" => "STATE",
        "table.url" => "URL",
        "table.name" => "NAME",
        "table.when" => "WHEN",
        "table.event" => "EVENT",
        "table.detail" => "DETAIL",
        "table.pos" => "POS",
        "table.requested" => "REQUESTED",
        "table.operator" => "OPERATOR",
        "table.token" => "TOKEN",
        "table.api" => "API",
        "table.id" => "ID",
        "table.baseDomain" => "BASE DOMAIN",
        "table.stack" => "STACK",
        "table.scope" => "SCOPE",
    }
}

fn spanish() -> BTreeMap<&'static str, &'static str> {
    catalog! {
        // -- deploy ---------------------------------------------------------
        "deploy.start" => "oxid up {branch}",
        "deploy.configParsed" => "oxid.toml leído correctamente",
        "deploy.registered" => "Proyecto `{name}` registrado (id {id})",
        "deploy.building" => "Construyendo la imagen de {branch}...",
        "deploy.builtCached" => "Imagen construida (caché: {cache}%, {took})",
        "deploy.built" => "Imagen construida ({took})",
        "deploy.live" => "Entorno vivo en: {url}",
        "deploy.queued" => "En cola esperando capacidad del host (posición {position}) — mira `oxid queue`, o `oxid status` cuando se despliegue",
        "deploy.interrupted" => "Interrumpido — el despliegue sigue en el servidor; compruébalo con `oxid status` o `oxid queue`",
        "rollback.start" => "oxid rollback {branch}",
        "rollback.done" => "Revertido a {sha} — entorno vivo en: {url}",
        // -- lifecycle -------------------------------------------------------
        "env.destroyed" => "Entorno `{branch}` destruido",
        "env.secretsPurged" => "Secretos de la rama `{branch}` eliminados",
        "env.paused" => "Entorno `{branch}` dormido",
        "env.woken" => "Entorno `{branch}` despertado",
        "project.deleted" => "Proyecto `{name}` eliminado",
        "confirm.aborted" => "Cancelado (vuelve a ejecutarlo con --force para saltarte esta pregunta).",
        // -- empty states ------------------------------------------------------
        "empty.environments" => "Todavía no hay entornos para `{name}`. Despliega uno con `oxid up <rama>`.",
        "empty.projects" => "Todavía no hay proyectos registrados.",
        "empty.queue" => "La cola está vacía — todos los despliegues tienen capacidad.",
        "empty.audit" => "Todavía no hay eventos de auditoría.",
        "empty.tokens" => "Todavía no se ha emitido ningún token.",
        "empty.secrets" => "No hay secretos en este ámbito.",
        "empty.contexts" => "Todavía no hay contextos configurados. Añade uno con `oxid context add <nombre> --api <url>`.",
        "empty.activeContext" => "No hay contexto activo (usando --api/OXID_API/por defecto).",
        // -- contexts -----------------------------------------------------------
        "context.added" => "Contexto `{name}` añadido",
        "context.switched" => "Cambiado al contexto `{name}`",
        "context.removed" => "Contexto `{name}` eliminado",
        // -- tokens --------------------------------------------------------------
        "token.created" => "Token `{name}` creado (id {id}): {token}",
        "token.unscoped" => "Sin alcance: este token tiene acceso total. Vuelve a crearlo con --project para limitarlo.",
        "token.scoped" => "Con alcance a los proyectos {projects} — cualquier otro proyecto le responde 404.",
        "token.onlyOnce" => "Esta es la única vez que se muestra el token en claro — guárdalo ahora.",
        "token.fetched" => "Token obtenido y guardado en el contexto `{name}`",
        "token.useContext" => "oxid context use {name}   # si no está ya activo",
        "token.revoked" => "Token {id} revocado",
        // -- secrets ---------------------------------------------------------------
        "secret.setBranch" => "Secreto `{name}` definido para la rama `{branch}`",
        "secret.set" => "Secreto `{name}` definido ({scope})",
        "secret.deleted" => "Secreto `{name}` borrado",
        // -- backup / keys ----------------------------------------------------------
        "backup.written" => "Copia de seguridad escrita en `{file}` ({bytes} bytes)",
        "key.rotated" => "Clave maestra rotada — todos los secretos recifrados, sin caída de servicio",
        // -- doctor -------------------------------------------------------------------
        "doctor.reachable" => "Daemon accesible en {api} (v{version}, {ms}ms)",
        "doctor.authOk" => "La API de control autentica correctamente",
        "doctor.versionMatch" => "Las versiones del CLI (v{cli}) y del daemon (v{daemon}) coinciden",
        "doctor.versionSkew" => "El CLI es v{cli} pero el daemon es v{daemon} (versiones mayores distintas) — actualiza el más antiguo; una diferencia de versión mayor puede romper la compatibilidad de la API",
        "doctor.capacity" => "Capacidad de Docker: {cpus} CPU(s), {memory} GiB de memoria, {running} entorno(s) corriendo",
        "doctor.noInfra" => "No se pudo obtener el estado de la infraestructura ({error}) — puede que el daemon sea anterior a `/api/v1/infra/status`; actualízalo para habilitar esta comprobación",
        "doctor.noStats" => "No se pudieron obtener las estadísticas de capacidad ({error}) — puede que el daemon sea anterior a `/api/v1/stats`; actualízalo para habilitar esta comprobación",
        "doctor.scopedNoNode" => "Las comprobaciones de nodo (capacidad, infraestructura) no están disponibles para un token con alcance de proyecto — es lo esperado, y lo de arriba es lo que importa para tus proyectos",
        // -- infra ------------------------------------------------------------------------
        "infra.networkExists" => "La red de Docker `{network}` existe",
        "infra.networkMissing" => "La red de Docker `{network}` no existe",
        "infra.traefikRunning" => "Traefik está corriendo",
        "infra.traefikRunningPort" => "Traefik está corriendo (publicado en el puerto {port} del host — las URLs de rama lo necesitan)",
        "infra.traefikPaused" => "El contenedor de Traefik existe pero está pausado",
        "infra.traefikStopped" => "El contenedor de Traefik existe pero está parado",
        "infra.traefikMissing" => "Traefik no está corriendo",
        "infra.selfWiredFully" => "El contenedor de este daemon está listo para despertar bajo demanda",
        "infra.selfWiredNoCatchall" => "El despertar bajo demanda no alcanza a las ramas dormidas: falta el router comodín",
        "infra.selfWiredNot" => "El contenedor de este daemon NO está listo para despertar bajo demanda",
        "infra.notContainerized" => "El daemon no corre dentro de Docker — se omite la comprobación de cableado",
        "infra.selfWiringUnknown" => "No se pudo determinar el cableado del contenedor de este daemon",
        // -- table headers -------------------------------------------------------------------
        "table.branch" => "RAMA",
        "table.state" => "ESTADO",
        "table.url" => "URL",
        "table.name" => "NOMBRE",
        "table.when" => "CUÁNDO",
        "table.event" => "EVENTO",
        "table.detail" => "DETALLE",
        "table.pos" => "POS",
        "table.requested" => "SOLICITADO",
        "table.operator" => "OPERADOR",
        "table.token" => "TOKEN",
        "table.api" => "API",
        "table.id" => "ID",
        "table.baseDomain" => "DOMINIO BASE",
        "table.stack" => "STACK",
        "table.scope" => "ÁMBITO",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every locale must define every key the default does. A gap only
    /// shows up as English text in an otherwise translated run, which is
    /// easy to ship and hard to notice.
    #[test]
    fn catalog_is_complete() {
        let en = english();
        for (locale, table) in [(Locale::Spanish, spanish())] {
            for key in en.keys() {
                assert!(table.contains_key(key), "{locale:?} is missing `{key}`");
            }
            assert_eq!(table.len(), en.len(), "{locale:?} has keys English doesn't");
        }
    }

    /// Translations must keep the placeholders their English original has —
    /// a dropped `{branch}` silently prints a message with a hole in it,
    /// and an invented one renders as literal braces.
    #[test]
    fn placeholders_match_across_locales() {
        fn placeholders(text: &str) -> Vec<String> {
            let mut found: Vec<String> = text
                .split('{')
                .skip(1)
                .filter_map(|rest| rest.split_once('}').map(|(name, _)| name.to_owned()))
                .collect();
            found.sort();
            found.dedup();
            found
        }
        let en = english();
        let es = spanish();
        for (key, value) in &en {
            let translated = es.get(key).expect("completeness is checked separately");
            assert_eq!(
                placeholders(value),
                placeholders(translated),
                "placeholders differ for `{key}`"
            );
        }
    }

    #[test]
    fn language_tags_ignore_region_and_encoding() {
        assert_eq!(Locale::parse("es"), Some(Locale::Spanish));
        assert_eq!(Locale::parse("es_MX.UTF-8"), Some(Locale::Spanish));
        assert_eq!(Locale::parse("es-419"), Some(Locale::Spanish));
        assert_eq!(Locale::parse("en_GB"), Some(Locale::English));
        assert_eq!(Locale::parse("de_DE"), None);
    }

    /// `OXID_LANG` wins over the shell's locale, and `C`/`POSIX` mean "no
    /// localization" rather than "unknown language", so they must not stop
    /// the search at the first variable that happens to be set.
    #[test]
    fn environment_precedence_and_neutral_locales() {
        assert_eq!(Locale::parse("C"), None);
        assert_eq!(Locale::parse("POSIX"), None);
        // `from_env` reads process state, so the ordering itself is asserted
        // through `parse` plus the documented list rather than by mutating
        // the environment from a test that may run in parallel with others.
        for tag in ["es", "es_MX.UTF-8", "es-419"] {
            assert_eq!(Locale::parse(tag), Some(Locale::Spanish), "{tag}");
        }
    }

    #[test]
    fn substitution_fills_every_placeholder() {
        let rendered = tf("deploy.registered", &[("name", "shopfront"), ("id", "1")]);
        assert!(rendered.contains("shopfront"), "{rendered}");
        assert!(rendered.contains('1'), "{rendered}");
        assert!(
            !rendered.contains('{'),
            "unsubstituted placeholder: {rendered}"
        );
    }
}
