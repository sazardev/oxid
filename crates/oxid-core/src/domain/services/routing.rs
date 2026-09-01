//! Traefik's dynamic configuration, generated from environment rows.
//!
//! The Docker label provider only sees containers on the socket Traefik is
//! reading, which is the control plane's. It is structurally incapable of
//! ever learning about a container on another machine — and that is the
//! reason this exists, not a preference for one provider over another.
//!
//! So a second provider is added: Traefik polls this daemon over HTTP and
//! is handed a router per environment, built from the database. Three
//! consequences follow, and the third is the interesting one:
//!
//! 1. **A branch on a remote node gets a route.** Its container publishes a
//!    port on that node; the control plane's own per-branch proxy bridges a
//!    stable local port to it, and the router points at that.
//! 2. **The route is stable across a redeploy.** It names the branch's
//!    `public_port`, which is bound once and reused, rather than the
//!    container's `host_port`, which changes every deploy. Pointing Traefik
//!    straight at `node.address:host_port` looks simpler and reintroduces
//!    the gap migration `0007` removed: the HTTP provider polls, so every
//!    redeploy would leave a window with a stale route.
//! 3. **A stopped environment still has a router**, because the router comes
//!    from a *row*, not from a running container. That is what makes
//!    wake-on-request work without the fragile lowest-priority
//!    `oxid-wake-catchall` on the daemon's own container: the request
//!    reaches the branch's own router, the proxy has no target, Traefik sees
//!    a 502, the `errors` middleware fires, and the environment wakes.
//!
//! Pure and `serde`-only, so `traefik_labels` and this share **one tested
//! set of rules** rather than deriving them separately and drifting.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::infra::TraefikSpec;
use crate::domain::state::EnvironmentState;

/// One environment, as routing needs to see it.
///
/// Deliberately not `Environment`: routing needs a hostname, a port and
/// whether the branch is alive, and taking the entity would drag branch
/// commits and timestamps into a signature that has no use for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedEnvironment {
    /// A name unique across the fleet, used for the router, the service and
    /// the middlewares. The container name is the natural choice: it is
    /// already unique per deployment.
    pub name: String,
    /// The hostname the `Host()` rule matches.
    pub url: String,
    /// The stable port on the control plane that reaches this branch — the
    /// branch proxy's `public_port`.
    pub public_port: u16,
    /// Lifecycle state, so a destroyed environment is not routed to.
    pub state: EnvironmentState,
    /// The owning project's `[routing].base_domain`, used only for the
    /// wildcard certificate under DNS-01.
    ///
    /// Per environment rather than one value for the whole document,
    /// because `base_domain` is a *project* setting: a daemon hosting
    /// `app.example.dev` and `api.other.dev` has two of them, and naming
    /// one wildcard for both would request a certificate that covers half
    /// the fleet and silently fail to cover the rest.
    pub base_domain: String,
}

/// Traefik's `http` dynamic-configuration document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DynamicConfig {
    /// The `http` section — the only one Oxid generates.
    pub http: HttpConfig,
}

/// Routers, services and middlewares.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HttpConfig {
    /// One per routed environment, keyed by name.
    pub routers: BTreeMap<String, Router>,
    /// One per routed environment, keyed by name.
    pub services: BTreeMap<String, Service>,
    /// Two per routed environment (heartbeat and wake), plus nothing else.
    pub middlewares: BTreeMap<String, Middleware>,
}

/// A `Host()` router.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Router {
    /// A Traefik host rule naming this environment's hostname.
    pub rule: String,
    /// Which service handles it.
    pub service: String,
    /// `web`, or `websecure` when certificates are configured.
    ///
    /// Traefik spells it `entryPoints`, and the rename is load-bearing:
    /// a key Traefik does not recognise is ignored in silence, so getting
    /// this wrong produces routers that exist, answer nothing, and log no
    /// complaint anywhere. Pinned by
    /// `the_document_serialises_to_the_shape_traefik_expects`, which is how
    /// it was caught.
    #[serde(rename = "entryPoints")]
    pub entry_points: Vec<String>,
    /// Heartbeat then wake, in that order — see [`dynamic_config`].
    pub middlewares: Vec<String>,
    /// Present only under ACME.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<RouterTls>,
}

/// A router's TLS block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RouterTls {
    /// The ACME resolver's name.
    #[serde(rename = "certResolver")]
    pub cert_resolver: String,
    /// One wildcard entry under DNS-01; empty under HTTP-01, where Traefik
    /// requests a certificate per hostname.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<TlsDomain>,
}

/// A certificate's main name and its SANs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TlsDomain {
    /// `*.example.dev`.
    pub main: String,
    /// The apex, so one certificate covers both.
    pub sans: Vec<String>,
}

/// A load balancer with exactly one server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Service {
    /// Where the traffic goes.
    #[serde(rename = "loadBalancer")]
    pub load_balancer: LoadBalancer,
}

/// The one server behind a service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadBalancer {
    /// Always a single entry: the branch's proxy on the control plane.
    pub servers: Vec<Server>,
}

/// A backend URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Server {
    /// `http://127.0.0.1:{public_port}`.
    pub url: String,
}

/// Either half of the pair every environment gets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Middleware {
    /// The unauthenticated heartbeat that feeds idle detection.
    ForwardAuth {
        /// Wraps the daemon's `/api/v1/heartbeat`.
        #[serde(rename = "forwardAuth")]
        forward_auth: ForwardAuth,
    },
    /// Wake-on-request.
    Errors {
        /// Wraps the daemon's `/api/v1/wake`.
        errors: Errors,
    },
}

/// The heartbeat middleware's body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardAuth {
    /// Absolute URL of the daemon's heartbeat endpoint.
    pub address: String,
}

/// The wake middleware's body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Errors {
    /// `502-504`.
    pub status: Vec<String>,
    /// The daemon's own wake service, declared on its container.
    pub service: String,
    /// `/api/v1/wake`.
    pub query: String,
}

/// Status codes wake-on-request reacts to.
///
/// Gateway errors only, deliberately not the whole 5xx range. These are the
/// codes Traefik itself produces when it cannot reach a backend: 502 on
/// connection refused, 504 on a dial or response timeout, 503 when a router
/// has no healthy server. An absent container can only surface as one of
/// those.
///
/// A plain 500 is the opposite — it can only come from an app that is
/// running and answering. Catching it too meant a branch whose code threw
/// showed its developer a "Waking up…" page reloading every two seconds
/// forever, instead of the stack trace the environment exists to show: the
/// product hiding exactly the information it was built to surface.
pub const WAKE_STATUS_RANGE: &str = "502-504";

/// The daemon's own wake service, declared on its container's labels.
pub const WAKE_SERVICE: &str = "oxid-wake";

/// Whether an environment should have a route at all.
///
/// Everything but `Destroyed` and `BuildFailed`, and the inclusions matter
/// more than the exclusions: a `Paused` or `Hibernating` branch **must**
/// keep its router, because that router is what catches the request that
/// wakes it. Routing only what is running would recreate the exact hole the
/// catch-all exists to paper over.
///
/// `BuildFailed` is excluded because there is no container and never was —
/// routing to it would answer a 502 and trigger a wake for an environment
/// that cannot come up, turning a broken push into an infinite retry.
#[must_use]
pub const fn is_routable(state: EnvironmentState) -> bool {
    matches!(
        state,
        EnvironmentState::Running
            | EnvironmentState::Paused
            | EnvironmentState::Hibernating
            | EnvironmentState::Building
    )
}

/// Builds the whole dynamic configuration.
///
/// `daemon_url` is where Traefik reaches this daemon from inside the shared
/// network.
#[must_use]
pub fn dynamic_config(
    environments: &[RoutedEnvironment],
    daemon_url: &str,
    spec: &TraefikSpec,
) -> DynamicConfig {
    let mut config = DynamicConfig::default();
    let secure = spec.acme.is_some();

    for env in environments.iter().filter(|e| is_routable(e.state)) {
        let name = &env.name;
        let heartbeat = format!("{name}-heartbeat");
        let wake = format!("{name}-wake");

        config.http.routers.insert(
            name.clone(),
            Router {
                rule: format!("Host(`{}`)", env.url),
                service: name.clone(),
                entry_points: vec![if secure { "websecure" } else { "web" }.to_owned()],
                // Heartbeat first: it must run on every request, including
                // the ones the wake middleware is about to turn into a
                // wake-up page. Reversing them means a branch that is asleep
                // never records the traffic that woke it, so it is
                // immediately eligible to be put back to sleep.
                middlewares: vec![heartbeat.clone(), wake.clone()],
                tls: secure.then(|| router_tls(&env.base_domain, spec)),
            },
        );

        config.http.services.insert(
            name.clone(),
            Service {
                load_balancer: LoadBalancer {
                    servers: vec![Server {
                        // Loopback, and always loopback: this is the control
                        // plane's own per-branch proxy, which is what makes
                        // the address survive a redeploy. The node the
                        // container actually runs on is the *proxy's*
                        // business, not Traefik's.
                        url: format!("http://127.0.0.1:{}", env.public_port),
                    }],
                },
            },
        );

        config.http.middlewares.insert(
            heartbeat,
            Middleware::ForwardAuth {
                forward_auth: ForwardAuth {
                    address: format!("{daemon_url}/api/v1/heartbeat"),
                },
            },
        );
        config.http.middlewares.insert(
            wake,
            Middleware::Errors {
                errors: Errors {
                    status: vec![WAKE_STATUS_RANGE.to_owned()],
                    service: WAKE_SERVICE.to_owned(),
                    query: "/api/v1/wake".to_owned(),
                },
            },
        );
    }

    config
}

fn router_tls(base_domain: &str, spec: &TraefikSpec) -> RouterTls {
    let Some(acme) = spec.acme.as_ref() else {
        return RouterTls::default();
    };
    RouterTls {
        cert_resolver: acme.resolver_name.clone(),
        // One wildcard entry is what makes a single certificate serve every
        // branch. Without it Traefik asks for one per hostname even under
        // DNS-01, which is the rate limit DNS-01 was chosen to escape.
        domains: if acme.is_wildcard() {
            vec![TlsDomain {
                main: crate::domain::infra::AcmeConfig::wildcard_for(base_domain),
                sans: vec![base_domain.to_owned()],
            }]
        } else {
            Vec::new()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::infra::{AcmeChallenge, AcmeConfig};

    fn env(name: &str, state: EnvironmentState) -> RoutedEnvironment {
        RoutedEnvironment {
            name: name.to_owned(),
            url: format!("{name}.app.example.dev"),
            public_port: 40100,
            state,
            base_domain: "app.example.dev".to_owned(),
        }
    }

    fn plain_spec() -> TraefikSpec {
        TraefikSpec {
            container_name: "oxid-traefik".to_owned(),
            network: "oxid".to_owned(),
            image: "traefik:v3.1".to_owned(),
            docker_socket_path: "/var/run/docker.sock".to_owned(),
            http_port: 80,
            https_port: Some(443),
            acme: None,
            http_provider: None,
        }
    }

    fn acme_spec(challenge: AcmeChallenge) -> TraefikSpec {
        let mut spec = plain_spec();
        spec.acme = Some(AcmeConfig {
            email: "ops@example.dev".to_owned(),
            resolver_name: "oxid".to_owned(),
            challenge,
            ca_directory: None,
            storage_volume: "oxid-acme".to_owned(),
            http_redirect: true,
        });
        spec
    }

    #[test]
    fn a_running_environment_gets_a_router_a_service_and_both_middlewares() {
        let config = dynamic_config(
            &[env("oxid-app-feat-1", EnvironmentState::Running)],
            "http://oxid-daemon:8080",
            &plain_spec(),
        );

        let router = &config.http.routers["oxid-app-feat-1"];
        assert_eq!(router.rule, "Host(`oxid-app-feat-1.app.example.dev`)");
        assert_eq!(router.entry_points, ["web"]);
        assert_eq!(
            router.middlewares,
            ["oxid-app-feat-1-heartbeat", "oxid-app-feat-1-wake"]
        );
        assert!(router.tls.is_none());

        assert_eq!(
            config.http.services["oxid-app-feat-1"]
                .load_balancer
                .servers[0]
                .url,
            "http://127.0.0.1:40100"
        );
        assert_eq!(config.http.middlewares.len(), 2);
    }

    /// The whole reason the HTTP provider improves on labels: a stopped
    /// container has no labels and therefore no router, which is why the
    /// fragile lowest-priority catch-all had to exist. A router built from a
    /// row exists whether the container runs or not.
    #[test]
    fn a_sleeping_environment_keeps_its_router() {
        for state in [
            EnvironmentState::Paused,
            EnvironmentState::Hibernating,
            EnvironmentState::Building,
        ] {
            let config = dynamic_config(
                &[env("oxid-app-feat-1", state)],
                "http://oxid-daemon:8080",
                &plain_spec(),
            );
            assert_eq!(
                config.http.routers.len(),
                1,
                "{state:?} must keep a router — it is what catches the request that wakes it"
            );
        }
    }

    /// A destroyed environment has nothing to route to, and a failed build
    /// never had a container at all: routing it would answer 502, fire the
    /// wake middleware, and turn a broken push into an endless retry.
    #[test]
    fn destroyed_and_failed_environments_are_not_routed() {
        let config = dynamic_config(
            &[
                env("gone", EnvironmentState::Destroyed),
                env("broken", EnvironmentState::BuildFailed),
            ],
            "http://oxid-daemon:8080",
            &plain_spec(),
        );
        assert!(config.http.routers.is_empty());
        assert!(config.http.services.is_empty());
        assert!(config.http.middlewares.is_empty());
    }

    /// Heartbeat before wake. Reversed, a sleeping branch never records the
    /// traffic that woke it and is immediately eligible to sleep again.
    #[test]
    fn the_heartbeat_runs_before_the_wake() {
        let config = dynamic_config(
            &[env("a", EnvironmentState::Paused)],
            "http://oxid-daemon:8080",
            &plain_spec(),
        );
        let mws = &config.http.routers["a"].middlewares;
        assert_eq!(mws[0], "a-heartbeat");
        assert_eq!(mws[1], "a-wake");
    }

    /// Gateway errors only. A 500 comes from an app that is running and
    /// answering, and catching it hid the stack trace behind a "Waking up…"
    /// page that reloaded forever.
    #[test]
    fn wake_reacts_to_gateway_errors_only() {
        let config = dynamic_config(
            &[env("a", EnvironmentState::Running)],
            "http://oxid-daemon:8080",
            &plain_spec(),
        );
        let Middleware::Errors { errors } = &config.http.middlewares["a-wake"] else {
            panic!("the wake middleware must be an `errors` middleware");
        };
        assert_eq!(errors.status, ["502-504"]);
        assert_eq!(errors.service, "oxid-wake");
        assert_eq!(errors.query, "/api/v1/wake");
    }

    #[test]
    fn the_heartbeat_points_at_this_daemon() {
        let config = dynamic_config(
            &[env("a", EnvironmentState::Running)],
            "http://oxid-daemon:9999",
            &plain_spec(),
        );
        let Middleware::ForwardAuth { forward_auth } = &config.http.middlewares["a-heartbeat"]
        else {
            panic!("the heartbeat must be a `forwardAuth` middleware");
        };
        assert_eq!(
            forward_auth.address,
            "http://oxid-daemon:9999/api/v1/heartbeat"
        );
    }

    /// Under DNS-01 one wildcard certificate serves every branch. Without
    /// the explicit domain entry Traefik asks per hostname, which is the
    /// rate limit DNS-01 exists to escape.
    #[test]
    fn dns_challenges_request_one_wildcard_for_the_whole_domain() {
        let config = dynamic_config(
            &[env("a", EnvironmentState::Running)],
            "http://oxid-daemon:8080",
            &acme_spec(AcmeChallenge::Dns01 {
                provider: "cloudflare".to_owned(),
                env_keys: Vec::new(),
            }),
        );
        let router = &config.http.routers["a"];
        assert_eq!(router.entry_points, ["websecure"]);
        let tls = router.tls.as_ref().unwrap();
        assert_eq!(tls.cert_resolver, "oxid");
        assert_eq!(tls.domains[0].main, "*.app.example.dev");
        assert_eq!(tls.domains[0].sans, ["app.example.dev"]);
    }

    /// HTTP-01 cannot answer a wildcard challenge, so Traefik is left to
    /// request one certificate per hostname — naming a wildcard here would
    /// produce a resolver that never succeeds.
    #[test]
    fn http_challenges_name_no_wildcard() {
        let config = dynamic_config(
            &[env("a", EnvironmentState::Running)],
            "http://oxid-daemon:8080",
            &acme_spec(AcmeChallenge::Http01),
        );
        let tls = config.http.routers["a"].tls.as_ref().unwrap();
        assert_eq!(tls.cert_resolver, "oxid");
        assert!(tls.domains.is_empty());
    }

    /// Traefik reads this document; the field names are its API, not ours.
    /// A rename would produce a configuration Traefik silently ignores,
    /// which is a fleet with no routes and no error anywhere.
    #[test]
    fn the_document_serialises_to_the_shape_traefik_expects() {
        let config = dynamic_config(
            &[env("a", EnvironmentState::Running)],
            "http://oxid-daemon:8080",
            &acme_spec(AcmeChallenge::Http01),
        );
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(
            json["http"]["routers"]["a"]["rule"],
            "Host(`a.app.example.dev`)"
        );
        assert_eq!(json["http"]["routers"]["a"]["entryPoints"][0], "websecure");
        assert_eq!(json["http"]["routers"]["a"]["tls"]["certResolver"], "oxid");
        assert_eq!(
            json["http"]["services"]["a"]["loadBalancer"]["servers"][0]["url"],
            "http://127.0.0.1:40100"
        );
        assert_eq!(
            json["http"]["middlewares"]["a-heartbeat"]["forwardAuth"]["address"],
            "http://oxid-daemon:8080/api/v1/heartbeat"
        );
        assert_eq!(
            json["http"]["middlewares"]["a-wake"]["errors"]["service"],
            "oxid-wake"
        );
    }

    /// Two branches must not share a middleware, a service or a router:
    /// one heartbeat shared between them would attribute both branches'
    /// traffic to whichever the map happened to keep.
    #[test]
    fn every_environment_gets_its_own_entities() {
        let config = dynamic_config(
            &[
                env("a", EnvironmentState::Running),
                env("b", EnvironmentState::Paused),
            ],
            "http://oxid-daemon:8080",
            &plain_spec(),
        );
        assert_eq!(config.http.routers.len(), 2);
        assert_eq!(config.http.services.len(), 2);
        assert_eq!(config.http.middlewares.len(), 4);
    }
}
