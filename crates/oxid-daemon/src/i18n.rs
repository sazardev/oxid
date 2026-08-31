//! Message catalog for text the API returns to a person.
//!
//! The locale travels the same way the request id does — a
//! `tokio::task_local!` set by middleware from `Accept-Language` — so a
//! message built deep inside `ControlPlane` can be phrased in the caller's
//! language without threading a locale parameter through every method that
//! might produce one. See [`crate::request_context`] for why that is sound
//! here: a request's whole call chain runs inside its own task.
//!
//! **What is not translated, deliberately:**
//!
//! - Anything wrapping a `git2`, `bollard` or `sqlx` error. Those strings
//!   are produced by those libraries, in English, and are what an operator
//!   actually searches for when something breaks. Rewording them would make
//!   them harder to look up, not easier to read.
//! - Log output. Logs are read by operators and shipped to aggregators that
//!   match on their text; a log line whose wording depended on whoever
//!   happened to send the request would be unsearchable.
//! - Anything under `--json` on the CLI, for the same reason.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::OnceLock;

/// Languages the daemon can answer in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    /// English — also the fallback for any key a locale is missing.
    #[default]
    English,
    /// Spanish.
    Spanish,
}

impl Locale {
    /// Picks a locale from an `Accept-Language` header value.
    ///
    /// Honours quality values, so `es;q=0.9, en;q=0.8` prefers Spanish and
    /// `en, es;q=0.9` prefers English regardless of the order they appear
    /// in. Region and script subtags are ignored (`es-419` is Spanish), and
    /// a tag no locale claims is skipped rather than ending the search —
    /// `de, es` should answer in Spanish, not fall back to English.
    #[must_use]
    pub fn from_accept_language(header: &str) -> Self {
        let mut best: Option<(f32, Self)> = None;
        for entry in header.split(',') {
            let mut parts = entry.split(';');
            let tag = parts.next().unwrap_or_default().trim();
            // `*` means "anything", which is already what the default is.
            if tag.is_empty() || tag == "*" {
                continue;
            }
            let quality = parts
                .find_map(|p| p.trim().strip_prefix("q=").map(str::trim))
                .and_then(|q| q.parse::<f32>().ok())
                .unwrap_or(1.0);
            let Some(locale) = Self::from_tag(tag) else {
                continue;
            };
            if best.is_none_or(|(best_q, _)| quality > best_q) {
                best = Some((quality, locale));
            }
        }
        best.map_or(Self::English, |(_, locale)| locale)
    }

    /// Maps a single language tag, ignoring region and script.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag
            .split('-')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "en" => Some(Self::English),
            "es" => Some(Self::Spanish),
            _ => None,
        }
    }
}

tokio::task_local! {
    static LOCALE: Locale;
}

/// Runs `fut` with `locale` available to [`t`]/[`tf`] throughout, however
/// deep the call chain goes.
pub async fn scope<F: Future>(locale: Locale, fut: F) -> F::Output {
    LOCALE.scope(locale, fut).await
}

/// The locale for the request being handled, or English outside one — the
/// scheduler's background sweeps have no caller to answer to.
#[must_use]
pub fn current() -> Locale {
    LOCALE.try_with(|locale| *locale).unwrap_or_default()
}

/// Looks `key` up in the current locale, falling back to English and
/// finally to the key itself, so a gap is greppable rather than blank.
#[must_use]
pub fn t(key: &str) -> &'static str {
    catalog(current())
        .get(key)
        .or_else(|| catalog(Locale::English).get(key))
        .copied()
        .unwrap_or("<missing>")
}

/// [`t`] with `{placeholder}` substitution against the translated string.
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
        // -- auth ------------------------------------------------------------
        "auth.missing" => "missing or invalid bearer token",
        "auth.invalid" => "invalid API token",
        "auth.revoked" => "this API token has been revoked",
        "auth.masterOnly" => "token management requires the master OXID_API_TOKEN",
        "auth.unscopedOnly" => "this endpoint requires an unscoped credential (the master OXID_API_TOKEN or a named token without --project scopes)",
        "auth.outOfScope" => "this token has no access to that project",
        "auth.rateLimited" => "too many requests — slow down",
        // -- not found ----------------------------------------------------------
        "notFound.project" => "project `{id}` not found",
        "notFound.environment" => "environment `{id}` not found",
        "notFound.secret" => "secret `{name}` not found",
        "notFound.token" => "token `{id}` not found",
        "notFound.queued" => "no deploy with id `{id}` is waiting in the queue",
        "infra.noNetwork" => "OXID_DOCKER_NETWORK is not set on this daemon — set it first, then restart, before running `oxid infra status`/`setup`",
        "notFound.branch" => "branch `{branch}` has no environment in this project",
        "notFound.repo" => "no project registered for `{repo}`",
        // -- validation ------------------------------------------------------------
        "invalid.branch" => "`{branch}` is not a valid branch name",
        "invalid.json" => "invalid JSON payload: {error}",
        "invalid.emptyScope" => "an empty project list would create a token that can do nothing; omit the field for full access",
        "invalid.duplicateRepo" => "more than one project is registered for `{repo}`; remove the duplicate registration so pushes route to exactly one project",
        "invalid.missingField" => "webhook payload is missing `{field}`",
        // -- deploy ---------------------------------------------------------------------
        "deploy.subdomainTaken" => "branch `{branch}` resolves to `{url}`, which branch `{other}` is already using — DNS labels can't tell `/`, `_` and `.` apart from `-`. Rename one of the two branches, or destroy the other environment first.",
        "deploy.containerGone" => "container `{name}` no longer exists; redeploy this branch to recreate it",
        "deploy.notReady" => "new instance never became ready: {detail}",
        "deploy.noCapacity" => "insufficient host capacity: {detail}",
        "deploy.dependencyUnconfigured" => "project `{project}` declares a `{kind}` dependency but {var} is not configured on this daemon",
        // -- webhooks -------------------------------------------------------------------------
        "webhook.noSecret" => "webhook secret is not configured; set OXID_WEBHOOK_SECRET",
        "webhook.missingSignature" => "missing `{header}` header",
        "webhook.badSignature" => "signature mismatch",
        "webhook.badToken" => "token mismatch",
    }
}

fn spanish() -> BTreeMap<&'static str, &'static str> {
    catalog! {
        // -- auth ------------------------------------------------------------
        "auth.missing" => "falta el token bearer o no es válido",
        "auth.invalid" => "token de API no válido",
        "auth.revoked" => "este token de API ha sido revocado",
        "auth.masterOnly" => "la gestión de tokens requiere el OXID_API_TOKEN maestro",
        "auth.unscopedOnly" => "este endpoint requiere una credencial sin alcance (el OXID_API_TOKEN maestro, o un token con nombre sin alcances --project)",
        "auth.outOfScope" => "este token no tiene acceso a ese proyecto",
        "auth.rateLimited" => "demasiadas peticiones — baja el ritmo",
        // -- not found ----------------------------------------------------------
        "notFound.project" => "proyecto `{id}` no encontrado",
        "notFound.environment" => "entorno `{id}` no encontrado",
        "notFound.secret" => "secreto `{name}` no encontrado",
        "notFound.token" => "token `{id}` no encontrado",
        "notFound.queued" => "no hay ningún despliegue con id `{id}` esperando en la cola",
        "infra.noNetwork" => "OXID_DOCKER_NETWORK no está definida en este daemon — defínela, reinicia, y luego ejecuta `oxid infra status`/`setup`",
        "notFound.branch" => "la rama `{branch}` no tiene entorno en este proyecto",
        "notFound.repo" => "no hay ningún proyecto registrado para `{repo}`",
        // -- validation ------------------------------------------------------------
        "invalid.branch" => "`{branch}` no es un nombre de rama válido",
        "invalid.json" => "cuerpo JSON no válido: {error}",
        "invalid.emptyScope" => "una lista de proyectos vacía crearía un token que no puede hacer nada; omite el campo para acceso total",
        "invalid.duplicateRepo" => "hay más de un proyecto registrado para `{repo}`; elimina el registro duplicado para que los pushes vayan a un solo proyecto",
        "invalid.missingField" => "al cuerpo del webhook le falta `{field}`",
        // -- deploy ---------------------------------------------------------------------
        "deploy.subdomainTaken" => "la rama `{branch}` resuelve a `{url}`, que ya usa la rama `{other}` — las etiquetas DNS no distinguen `/`, `_` ni `.` de `-`. Renombra una de las dos ramas, o destruye primero el otro entorno.",
        "deploy.containerGone" => "el contenedor `{name}` ya no existe; vuelve a desplegar esta rama para recrearlo",
        "deploy.notReady" => "la nueva instancia nunca llegó a estar lista: {detail}",
        "deploy.noCapacity" => "capacidad insuficiente en el host: {detail}",
        "deploy.dependencyUnconfigured" => "el proyecto `{project}` declara una dependencia `{kind}` pero {var} no está configurado en este daemon",
        // -- webhooks -------------------------------------------------------------------------
        "webhook.noSecret" => "el secreto del webhook no está configurado; define OXID_WEBHOOK_SECRET",
        "webhook.missingSignature" => "falta la cabecera `{header}`",
        "webhook.badSignature" => "la firma no coincide",
        "webhook.badToken" => "el token no coincide",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_complete() {
        let en = english();
        let es = spanish();
        for key in en.keys() {
            assert!(es.contains_key(key), "Spanish is missing `{key}`");
        }
        assert_eq!(es.len(), en.len(), "Spanish has keys English doesn't");
    }

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
        let (en, es) = (english(), spanish());
        for (key, value) in &en {
            assert_eq!(
                placeholders(value),
                placeholders(es[key]),
                "placeholders differ for `{key}`"
            );
        }
    }

    /// Quality values decide, not header order — a client listing
    /// `en, es;q=0.9` wants English even though Spanish appears later, and
    /// `es;q=0.9, en;q=0.8` wants Spanish even though English does.
    #[test]
    fn accept_language_honours_quality_and_skips_unknown_tags() {
        use Locale::{English, Spanish};
        for (header, expected) in [
            ("es", Spanish),
            ("es-419,es;q=0.9", Spanish),
            ("en-US,en;q=0.9", English),
            ("en, es;q=0.9", English),
            ("es;q=0.9, en;q=0.8", Spanish),
            // An unknown tag must not end the search.
            ("de, es", Spanish),
            ("de-DE", English),
            ("*", English),
            ("", English),
        ] {
            assert_eq!(
                Locale::from_accept_language(header),
                expected,
                "header `{header}`"
            );
        }
    }

    #[tokio::test]
    async fn messages_follow_the_scoped_locale() {
        assert_eq!(t("auth.invalid"), "invalid API token");
        let spanish = scope(Locale::Spanish, async { t("auth.invalid") }).await;
        assert_eq!(spanish, "token de API no válido");
        // Outside a scope it is English again, which is what the background
        // scheduler gets.
        assert_eq!(t("auth.invalid"), "invalid API token");
    }
}
