//! Turning a Compose file into a deployment plan.
//!
//! `IDEA.md` promises that if a repository has a `docker-compose.yml`, Oxid
//! knows what to do with it. It used to take the first service with a
//! `build:` key and silently drop the rest — so an `api` + `worker` + `db`
//! stack deployed the api alone, with no warning and no error, and the app
//! failed at runtime on a connection nobody had told it would not exist.
//!
//! Deciding what to do with each service is three rules, and all three are
//! pure. Keeping them here rather than in the YAML adapter means they are
//! testable without a file, and that the adapter's only job is to say what
//! the file said.
//!
//! ## The three rules
//!
//! 1. **A service that builds is deployed.** It is the repository's own
//!    code, and there is nothing to share it with.
//! 2. **A service that is only an image, and matches a pool Oxid runs, is
//!    multiplexed.** A `postgres:16` in a compose file is a *local dev*
//!    convenience; per branch it becomes a logical database on the shared
//!    instance instead of a container (`SPEC.md` §3.1 — not booting a
//!    database per branch is the entire point of the product).
//! 3. **Anything else is run as it is.** A `rabbitmq` has no pool to be
//!    folded into, and refusing the deploy would turn a great many real
//!    compose files into an error message. Scale-to-zero bounds the cost.
//!
//! And one more, about addressing: **exactly one service is public.** It
//! takes the branch's URL; the others are reachable only from inside the
//! environment, by their compose service name. A worker has no URL because
//! it has no port, and inventing one for it would be inventing a hostname
//! nobody asked for.

use crate::domain::resource_pool::PoolKind;

/// One service as the compose file described it.
///
/// Deliberately not the adapter's type: this is the subset the rules
/// consult, so a change to what the YAML parser collects cannot silently
/// change what the rules decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeService {
    /// The key under `services:` — `api`, `worker`, `db`. This is also the
    /// hostname siblings resolve it by, which is why it survives parsing at
    /// all: the previous version discarded it.
    pub name: String,
    /// Build context and Dockerfile, when the service declares `build:`.
    pub build: Option<ComposeBuild>,
    /// The `image:` reference, when it declares one.
    pub image: Option<String>,
    /// Container-side port from the first `ports:` entry.
    pub port: Option<u16>,
    /// The service's `environment:` block, in file order.
    ///
    /// Read for one reason: to find out what the application already calls
    /// a dependency. A compose file that says
    /// `DATABASE_URL: postgres://app@db:5432/app` has *told* us which
    /// variable points at the `db` service, and honouring that is better
    /// than guessing a conventional name and hoping.
    pub environment: Vec<(String, String)>,
}

/// A service's `build:` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeBuild {
    /// Context directory, relative to the compose file.
    pub context: String,
    /// Dockerfile path, relative to `context`.
    pub dockerfile: String,
}

/// What Oxid will do with one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Built from the repository and run per branch.
    Build(ComposeBuild),
    /// Folded into a shared instance: no container, a lease and an injected
    /// connection URL instead.
    Multiplex {
        /// Which shared pool it becomes a slice of.
        kind: PoolKind,
        /// The environment variable the connection string is injected as.
        inject_as: String,
    },
    /// Run from its image, per branch, as written.
    RunAsIs(String),
}

/// One planned service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedService {
    /// Compose service name; the hostname siblings use.
    pub name: String,
    /// What to do with it.
    pub disposition: Disposition,
    /// Container-side port, when it has one.
    pub port: Option<u16>,
    /// Whether this service takes the branch's URL. Exactly one is `true`
    /// in a plan that has any buildable service.
    pub is_primary: bool,
}

/// The whole plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePlan {
    /// Every service, in the order the compose file listed them.
    pub services: Vec<PlannedService>,
}

impl ServicePlan {
    /// The service that takes the branch URL, if any.
    #[must_use]
    pub fn primary(&self) -> Option<&PlannedService> {
        self.services.iter().find(|s| s.is_primary)
    }

    /// Services Oxid will build from the repository.
    pub fn built(&self) -> impl Iterator<Item = &PlannedService> {
        self.services
            .iter()
            .filter(|s| matches!(s.disposition, Disposition::Build(_)))
    }

    /// Services that become a slice of a shared instance rather than a
    /// container: the service name, the pool, and the variable its
    /// connection string is injected as.
    pub fn multiplexed(&self) -> impl Iterator<Item = (&str, PoolKind, &str)> {
        self.services.iter().filter_map(|s| match &s.disposition {
            Disposition::Multiplex { kind, inject_as } => {
                Some((s.name.as_str(), *kind, inject_as.as_str()))
            }
            _ => None,
        })
    }
}

/// Recognises a shared-pool dependency from a compose `image:` reference.
///
/// Matched on the repository part only, so `postgres:16-alpine`,
/// `docker.io/library/postgres` and `bitnami/postgresql` all land in the
/// same place. Deliberately a short, explicit list rather than a fuzzy
/// match: guessing that some unknown image is "probably a database" and
/// silently *not* deploying it is the kind of cleverness that produces a
/// broken environment nobody can explain.
#[must_use]
pub fn pool_for_image(image: &str) -> Option<PoolKind> {
    let repository = image.split('@').next()?.rsplit('/').next()?;
    let name = repository.split(':').next()?.to_ascii_lowercase();
    match name.as_str() {
        "postgres" | "postgresql" => Some(PoolKind::Postgres),
        "redis" | "valkey" => Some(PoolKind::Redis),
        _ => None,
    }
}

/// The variable a pool's connection string is injected as when the compose
/// file gives no hint. The names all but every framework already reads.
#[must_use]
pub const fn conventional_variable(kind: PoolKind) -> &'static str {
    match kind {
        PoolKind::Postgres => "DATABASE_URL",
        PoolKind::Redis => "REDIS_URL",
    }
}

/// Finds what the application already calls a service.
///
/// A compose file that says `DATABASE_URL: postgres://app@db:5432/app` has
/// *told* us which variable points at `db`, and honouring that beats
/// guessing a conventional name and hoping. Matched on the host position of
/// a URL — `@db:`, `//db:`, `//db/` — so a variable that merely mentions the
/// word elsewhere (`DB_POOL_SIZE=10`) is not mistaken for a connection
/// string.
fn declared_variable(services: &[ComposeService], target: &str) -> Option<String> {
    let needles = [
        format!("@{target}:"),
        format!("@{target}/"),
        format!("//{target}:"),
        format!("//{target}/"),
    ];
    services
        .iter()
        .flat_map(|s| s.environment.iter())
        .find_map(|(key, value)| {
            needles
                .iter()
                .any(|needle| value.contains(needle.as_str()))
                .then(|| key.clone())
        })
}

/// Builds the plan.
///
/// `preferred_primary` is an explicit `oxid.toml` choice and always wins.
/// Without one, the primary is the first buildable service that publishes a
/// port; failing that, the first buildable service at all — a stack whose
/// only service forgot its `ports:` should still get a URL rather than
/// none.
///
/// `available_pools` is what this daemon can actually multiplex onto. A
/// `postgres:16` is only folded into a shared instance if there *is* one:
/// otherwise it is deployed as written, because a compose file that used to
/// deploy must not start failing because an operator never configured
/// `OXID_POSTGRES_URL`. Multiplexing is an optimisation, and an
/// optimisation that turns a working deploy into an error is a bug.
#[must_use]
pub fn plan(
    services: &[ComposeService],
    preferred_primary: Option<&str>,
    available_pools: &[PoolKind],
) -> ServicePlan {
    let dispositions: Vec<Disposition> = services
        .iter()
        .map(|service| match (&service.build, &service.image) {
            // A `build:` wins even when an `image:` sits beside it — that
            // pair means "build this and tag it thus", not "pull this".
            (Some(build), _) => Disposition::Build(build.clone()),
            (None, Some(image)) => pool_for_image(image)
                .filter(|kind| available_pools.contains(kind))
                .map_or_else(
                    || Disposition::RunAsIs(image.clone()),
                    |kind| Disposition::Multiplex {
                        kind,
                        inject_as: declared_variable(services, &service.name)
                            .unwrap_or_else(|| conventional_variable(kind).to_owned()),
                    },
                ),
            // Neither: nothing to run. Compose would reject this file
            // itself, so it is not worth a special error here.
            (None, None) => Disposition::RunAsIs(String::new()),
        })
        .collect();

    let buildable = |i: usize| matches!(dispositions[i], Disposition::Build(_));

    let primary = preferred_primary
        .and_then(|wanted| services.iter().position(|s| s.name == wanted))
        .or_else(|| (0..services.len()).find(|&i| buildable(i) && services[i].port.is_some()))
        .or_else(|| (0..services.len()).find(|&i| buildable(i)));

    ServicePlan {
        services: services
            .iter()
            .zip(dispositions)
            .enumerate()
            .map(|(i, (service, disposition))| PlannedService {
                name: service.name.clone(),
                disposition,
                port: service.port,
                is_primary: primary == Some(i),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built(name: &str, port: Option<u16>) -> ComposeService {
        ComposeService {
            name: name.to_owned(),
            build: Some(ComposeBuild {
                context: ".".to_owned(),
                dockerfile: "Dockerfile".to_owned(),
            }),
            image: None,
            port,
            environment: Vec::new(),
        }
    }

    fn image(name: &str, image: &str) -> ComposeService {
        ComposeService {
            name: name.to_owned(),
            build: None,
            image: Some(image.to_owned()),
            port: None,
            environment: Vec::new(),
        }
    }

    /// Every pool a fully configured daemon offers.
    const ALL: [PoolKind; 2] = [PoolKind::Postgres, PoolKind::Redis];

    /// The case the old parser got wrong: three services, one deployed.
    #[test]
    fn every_buildable_service_is_planned_not_just_the_first() {
        let plan = plan(
            &[
                built("api", Some(3000)),
                built("worker", None),
                image("db", "postgres:16"),
            ],
            None,
            &ALL,
        );
        assert_eq!(plan.services.len(), 3);
        assert_eq!(plan.built().count(), 2);
        assert_eq!(plan.primary().unwrap().name, "api");
    }

    /// A database in a compose file is a local-dev convenience. Per branch
    /// it becomes a logical database on the shared instance — not booting
    /// one per branch is the whole product.
    #[test]
    fn a_known_database_image_is_multiplexed_rather_than_run() {
        let plan = plan(
            &[built("api", Some(80)), image("db", "postgres:16")],
            None,
            &ALL,
        );
        assert_eq!(
            plan.multiplexed().collect::<Vec<_>>(),
            [("db", PoolKind::Postgres, "DATABASE_URL")]
        );
    }

    /// An image with no pool to fold into is deployed as written. Refusing
    /// would turn a great many real compose files into an error message.
    #[test]
    fn an_image_with_no_pool_is_run_as_written() {
        let plan = plan(
            &[built("api", Some(80)), image("mq", "rabbitmq:3")],
            None,
            &ALL,
        );
        let mq = &plan.services[1];
        assert_eq!(
            mq.disposition,
            Disposition::RunAsIs("rabbitmq:3".to_owned())
        );
        assert_eq!(plan.multiplexed().count(), 0);
    }

    /// An optimisation that turns a working deploy into an error is a bug.
    /// Without a shared instance to fold it into, the database is deployed
    /// as written — which is what the compose file asked for anyway.
    #[test]
    fn without_a_pool_to_fold_into_a_database_is_deployed_as_written() {
        let services = [built("api", Some(80)), image("db", "postgres:16")];
        assert_eq!(
            plan(&services, None, &[]).services[1].disposition,
            Disposition::RunAsIs("postgres:16".to_owned())
        );
        // Redis configured but not Postgres: only the one there is a home
        // for is folded in.
        let plan = plan(&services, None, &[PoolKind::Redis]);
        assert_eq!(
            plan.services[1].disposition,
            Disposition::RunAsIs("postgres:16".to_owned())
        );
        assert_eq!(plan.multiplexed().count(), 0);
    }

    /// The compose file usually says what the application calls its
    /// database. Honouring that beats guessing a conventional name.
    #[test]
    fn the_variable_the_app_already_names_wins_over_the_convention() {
        let mut api = built("api", Some(80));
        api.environment = vec![
            ("DB_POOL_SIZE".to_owned(), "10".to_owned()),
            (
                "MAIN_DATABASE".to_owned(),
                "postgres://app@db:5432/app".to_owned(),
            ),
        ];
        let plan = plan(&[api, image("db", "postgres:16")], None, &ALL);
        assert_eq!(
            plan.multiplexed().collect::<Vec<_>>(),
            [("db", PoolKind::Postgres, "MAIN_DATABASE")],
            "the variable that actually points at the service, not the one that merely says `DB`"
        );
    }

    /// No hint in the file means the name all but every framework reads.
    #[test]
    fn with_no_hint_the_conventional_variable_is_used() {
        let plan = plan(
            &[built("api", Some(80)), image("cache", "redis:7")],
            None,
            &ALL,
        );
        assert_eq!(
            plan.multiplexed().collect::<Vec<_>>(),
            [("cache", PoolKind::Redis, "REDIS_URL")]
        );
    }

    /// A variable that merely mentions the service name is not a connection
    /// string, and injecting over it would replace someone's real setting.
    #[test]
    fn a_variable_that_only_mentions_the_name_is_not_matched() {
        let mut api = built("api", Some(80));
        api.environment = vec![("DB_KIND".to_owned(), "db".to_owned())];
        let plan = plan(&[api, image("db", "postgres:16")], None, &ALL);
        assert_eq!(
            plan.multiplexed().collect::<Vec<_>>(),
            [("db", PoolKind::Postgres, "DATABASE_URL")]
        );
    }

    /// The registry and tag must not stop a database being recognised.
    #[test]
    fn pool_images_are_matched_on_the_repository_not_the_whole_reference() {
        for reference in [
            "postgres",
            "postgres:16-alpine",
            "docker.io/library/postgres:16",
            "bitnami/postgresql:16",
        ] {
            assert_eq!(
                pool_for_image(reference),
                Some(PoolKind::Postgres),
                "{reference}"
            );
        }
        assert_eq!(pool_for_image("redis:7"), Some(PoolKind::Redis));
        assert_eq!(pool_for_image("valkey/valkey:9"), Some(PoolKind::Redis));
        assert_eq!(pool_for_image("rabbitmq:3"), None);
        // Not a fuzzy match: an image that merely mentions a database is
        // still deployed, because guessing wrong means silently not
        // deploying something the app needs.
        assert_eq!(pool_for_image("my-postgres-exporter:1"), None);
    }

    /// Exactly one service is public, and a worker is not it.
    #[test]
    fn the_primary_is_the_first_buildable_service_with_a_port() {
        let plan = plan(
            &[built("worker", None), built("api", Some(3000))],
            None,
            &ALL,
        );
        assert_eq!(plan.primary().unwrap().name, "api");
        assert_eq!(plan.services.iter().filter(|s| s.is_primary).count(), 1);
    }

    /// A stack whose only service forgot its `ports:` should still get a
    /// URL rather than none.
    #[test]
    fn a_buildable_service_without_a_port_still_becomes_primary_alone() {
        let plan = plan(&[built("app", None)], None, &ALL);
        assert_eq!(plan.primary().unwrap().name, "app");
    }

    #[test]
    fn an_explicit_choice_wins_over_the_convention() {
        let services = [built("api", Some(3000)), built("web", Some(80))];
        assert_eq!(plan(&services, None, &ALL).primary().unwrap().name, "api");
        assert_eq!(
            plan(&services, Some("web"), &ALL).primary().unwrap().name,
            "web"
        );
        // Naming something that is not there falls back rather than
        // producing a stack nobody can reach.
        assert_eq!(
            plan(&services, Some("ghost"), &ALL).primary().unwrap().name,
            "api"
        );
    }

    /// A stack of nothing but images has no public service — and must not
    /// pick one, because none of them is this repository's code.
    #[test]
    fn a_stack_with_nothing_to_build_has_no_primary() {
        let plan = plan(
            &[image("db", "postgres:16"), image("mq", "rabbitmq:3")],
            None,
            &ALL,
        );
        assert!(plan.primary().is_none());
        assert_eq!(plan.built().count(), 0);
    }

    /// `build:` beside `image:` means "build this and tag it thus".
    #[test]
    fn build_wins_over_a_sibling_image_tag() {
        let service = ComposeService {
            name: "api".to_owned(),
            build: Some(ComposeBuild {
                context: ".".to_owned(),
                dockerfile: "Dockerfile".to_owned(),
            }),
            image: Some("postgres:16".to_owned()),
            port: Some(80),
            environment: Vec::new(),
        };
        let plan = plan(&[service], None, &ALL);
        assert!(matches!(
            plan.services[0].disposition,
            Disposition::Build(_)
        ));
    }
}
