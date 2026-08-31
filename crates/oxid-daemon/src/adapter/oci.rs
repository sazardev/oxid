//! OCI/container adapter (SPEC.md §2.2 "Orquestación OCI").
//!
//! Talks to the local Docker socket via [`bollard`]. All methods map Docker
//! errors to [`OciError`].

use std::collections::HashMap;
use std::path::Path;

use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, LogsOptions, NetworkingConfig, RemoveContainerOptions,
    StopContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::image::{BuildImageOptions, BuilderVersion, RemoveImageOptions};
use bollard::models::{
    BuildInfo, EndpointSettings, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::network::{CreateNetworkOptions, ListNetworksOptions};
use bytes::Bytes;
use futures_util::StreamExt;
use oxid_core::services::tls::{
    ACME_MOUNT, TraefikRuntime, WEBSECURE_ENTRYPOINT_PORT, traefik_cmd,
};
use oxid_core::{
    AcmeChallenge, BuildReport, BuildSpec, ContainerPort, ContainerSpec, ContainerStatus,
    HostCapacity, LogStream, NetworkStatus, OciError, SelfWiringStatus, TraefikSpec, TraefikStatus,
};

/// Backed by a Docker connection (default socket).
#[derive(Debug, Clone)]
pub struct DockerClient {
    docker: Docker,
}

impl DockerClient {
    /// Connects using the default Docker socket (`/var/run/docker.sock`).
    ///
    /// # Errors
    /// Returns [`OciError::Failure`] if the socket cannot be reached.
    pub fn connect() -> Result<Self, OciError> {
        Docker::connect_with_defaults()
            .map(|docker| Self { docker })
            .map_err(map_err)
    }
}

fn map_err(err: bollard::errors::Error) -> OciError {
    match err {
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message,
        } => OciError::NotFound(message),
        other => OciError::Failure(other.to_string()),
    }
}

/// Tars `dir` so it can be streamed to the Docker build endpoint.
fn tar_context(dir: &Path) -> Result<Bytes, OciError> {
    let mut buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut buf);
        // `append_dir_all`'s default `follow_symlinks(true)` calls
        // `fs::metadata` (which *dereferences*) on every entry — a single
        // dangling symlink anywhere in the tree (a real repo had one under
        // `.claude/skills/`, unrelated to anything the Dockerfile even
        // touches) then fails the *entire* build with a bare "No such file
        // or directory" that doesn't even name the offending path.
        // `docker build`'s own context upload doesn't require symlink
        // targets to resolve; match that by storing the symlink entry
        // itself (target string, no dereference) instead of failing.
        builder.follow_symlinks(false);
        builder.append_dir_all(".", dir).map_err(|e| {
            OciError::Failure(format!("cannot tar build context `{}`: {e}", dir.display()))
        })?;
        builder
            .finish()
            .map_err(|e| OciError::Failure(format!("cannot finalize build context tar: {e}")))?;
    }
    Ok(Bytes::from(buf))
}

/// Accumulates cache-effectiveness data from a build's progress stream.
///
/// With `BuildKit` (the only builder we request), progress arrives as
/// *structured* solve events in `BuildInfo.aux`: each
/// [`bollard::models::BuildInfoAux::BuildKit`] payload carries `vertexes`,
/// one per step, with its Dockerfile-step `name` (`[stage-0 2/2] RUN ...`),
/// a stable `digest`, and whether it was served from cache. Only those
/// bracketed names are counted — `[internal] load ...`, frontend resolves,
/// and image export are plumbing, not user-visible steps.
///
/// For anything that still emits classic text progress (an engine without
/// `BuildKit`, a future builder change), lines like `#12 CACHED` /
/// `#12 DONE 0.1s` fall back to the same accumulator under a synthetic
/// `"#12"` key. Either way an empty result means "nothing observable" and
/// consumers hide the ratio rather than show a bogus 0%.
#[derive(Default)]
struct BuildProgress {
    /// Step key (digest or synthetic id) → served-from-cache?
    steps: HashMap<String, bool>,
}

impl BuildProgress {
    fn observe(&mut self, info: &BuildInfo) {
        if let Some(bollard::models::BuildInfoAux::BuildKit(status)) = info.aux.as_ref() {
            for vertex in &status.vertexes {
                // Dockerfile steps are named `[<stage> <i/j>] INSTRUCTION`;
                // `[internal] load ...` is bracketed plumbing and must be
                // excluded explicitly, along with resolves/exports.
                if !vertex.name.starts_with('[') || vertex.name.starts_with("[internal") {
                    continue;
                }
                let cached = self.steps.entry(vertex.digest.clone()).or_insert(false);
                *cached |= vertex.cached;
            }
        } else if let Some(chunk) = info.stream.as_deref() {
            self.observe_text(chunk);
        }
    }

    fn observe_text(&mut self, chunk: &str) {
        for line in chunk.lines() {
            let Some(rest) = line.trim().strip_prefix('#') else {
                continue;
            };
            let (id_str, tail) = match rest.split_once(' ') {
                Some(pair) => pair,
                None => (rest, ""),
            };
            // Only well-formed numbered ids; everything else is noise.
            if !id_str.bytes().all(|b| b.is_ascii_digit()) || id_str.is_empty() {
                continue;
            }
            let tail = tail.trim_start();
            let key = format!("#{id_str}");
            if tail.starts_with('[') {
                self.steps.entry(key).or_insert(false);
            } else if tail.starts_with("CACHED") {
                self.steps.insert(key, true);
            }
        }
    }

    /// `(steps_total, steps_cached)`
    fn summary(&self) -> (u32, u32) {
        let total = u32::try_from(self.steps.len()).unwrap_or(u32::MAX);
        let cached = u32::try_from(self.steps.values().filter(|cached| **cached).count())
            .unwrap_or(u32::MAX);
        (total, cached)
    }
}

impl ContainerPort for DockerClient {
    async fn pull_image(&self, image: &str) -> Result<(), OciError> {
        // `create_image` is Docker's pull. An image already present is not
        // re-downloaded — the daemon answers from its own store — so this
        // is cheap to call on every registration.
        let options = bollard::image::CreateImageOptions {
            from_image: image.to_owned(),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(options), None, None);
        while let Some(item) = stream.next().await {
            item.map_err(|e| OciError::Failure(format!("pulling `{image}` failed: {e}")))?;
        }
        Ok(())
    }

    async fn build(&self, spec: &BuildSpec) -> Result<BuildReport, OciError> {
        let options = BuildImageOptions {
            dockerfile: spec.dockerfile.clone(),
            t: spec.image.clone(),
            rm: true,
            // BuildKit, not the legacy V1 builder: upstream has deprecated
            // V1 and BuildKit is what makes `RUN --mount=type=cache,...`
            // work in user Dockerfiles (SPEC.md §4.5's shared dependency
            // caches). Layer cache for unchanged instructions comes free
            // either way; with BuildKit the caches survive even when an
            // early `COPY . .` layer changes. Requires bollard's
            // `buildkit` cargo feature (workspace `Cargo.toml`) plus a
            // unique session id per build — bollard uses it to open the
            // accompanying gRPC session, and Docker rejects the request
            // without one ("Buildkit requires a unique session").
            version: BuilderVersion::BuilderBuildKit,
            session: Some(uuid::Uuid::new_v4().to_string()),
            ..Default::default()
        };
        let context = tar_context(&spec.context)?;
        let started = std::time::Instant::now();
        let mut progress = BuildProgress::default();
        let mut stream = self.docker.build_image(options, None, Some(context));
        while let Some(item) = stream.next().await {
            match item {
                // bollard folds a terminal `BuildInfo.error` into
                // `DockerStreamError`, whose `Display` is just "Docker
                // stream error" — unwrap the real message so a broken
                // Dockerfile reports *why* it broke.
                Err(bollard::errors::Error::DockerStreamError { error }) => {
                    return Err(OciError::Failure(format!("image build failed: {error}")));
                }
                Ok(info) => progress.observe(&info),
                Err(e) => return Err(map_err(e)),
            }
        }
        let (steps_total, steps_cached) = progress.summary();
        Ok(BuildReport {
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            steps_total,
            steps_cached,
        })
    }

    async fn run(&self, spec: &ContainerSpec) -> Result<Option<u16>, OciError> {
        let container_port_key = format!("{}/tcp", spec.container_port);
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(container_port_key.clone(), HashMap::new());

        // When a Traefik network is configured, the container is reached
        // directly over that network and no host port is published — two
        // branches of the same project can then run concurrently. Without
        // it, publish `container_port` on a host port Docker picks itself
        // (`host_port: None` — an empty `HostPort` is Docker's own way of
        // saying "any free one") instead of a fixed port that could already
        // be taken by another branch of this same project (or anything else
        // on the host): a deploy should never fail just because a specific
        // port happened to be busy. The actual bound port is read back below
        // via `inspect_container` once the container is running.
        let port_bindings = spec.network.is_none().then(|| {
            let mut bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
            bindings.insert(
                container_port_key.clone(),
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_owned()),
                    host_port: None,
                }]),
            );
            bindings
        });

        let networking_config: Option<NetworkingConfig<String>> =
            spec.network.as_ref().map(|network| NetworkingConfig {
                endpoints_config: HashMap::from([(network.clone(), EndpointSettings::default())]),
            });

        let config = Config {
            image: Some(spec.image.clone()),
            env: Some(spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            exposed_ports: Some(exposed_ports),
            labels: Some(
                spec.labels
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            host_config: Some(HostConfig {
                port_bindings,
                memory: spec
                    .memory_limit_mb
                    .map(|mb| (mb * 1_048_576).cast_signed()),
                nano_cpus: spec
                    .cpu_limit_millicores
                    .map(|millicores| i64::from(millicores) * 1_000_000),
                // `unless-stopped`: Docker brings the container back on its
                // own after a crash, an OOM-kill, or the host rebooting —
                // without this, a restarted host left every preview
                // environment `Exited` until someone noticed and ran
                // `oxid wake`/redeployed by hand. Doesn't fight an
                // intentional `oxid down`/`pause`, which stop it via the
                // Docker API directly (the "unless the user has manually
                // stopped it" carve-out).
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                ..Default::default()
            }),
            networking_config,
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: spec.name.clone(),
            ..Default::default()
        };
        self.docker
            .create_container(Some(options), config)
            .await
            .map_err(map_err)?;
        self.docker
            .start_container::<String>(&spec.name, None)
            .await
            .map_err(map_err)?;

        if spec.network.is_some() {
            return Ok(None);
        }
        let bound_port = self
            .published_port(&spec.name, spec.container_port)
            .await?
            .ok_or_else(|| {
                OciError::Failure(format!(
                    "container `{}` started but Docker never reported a bound host port for \
                     `{container_port_key}`",
                    spec.name
                ))
            })?;
        Ok(Some(bound_port))
    }

    async fn published_port(
        &self,
        name: &str,
        container_port: u16,
    ) -> Result<Option<u16>, OciError> {
        let info = self
            .docker
            .inspect_container(name, None)
            .await
            .map_err(map_err)?;
        let key = format!("{container_port}/tcp");
        Ok(info
            .network_settings
            .and_then(|settings| settings.ports)
            .and_then(|ports| ports.get(&key).cloned().flatten())
            .and_then(|bindings| bindings.into_iter().next())
            .and_then(|binding| binding.host_port)
            .and_then(|port| port.parse::<u16>().ok()))
    }

    async fn start(&self, name: &str) -> Result<(), OciError> {
        self.docker
            .start_container::<String>(name, None)
            .await
            .map_err(map_err)
    }

    async fn pause(&self, name: &str) -> Result<(), OciError> {
        self.docker.pause_container(name).await.map_err(map_err)
    }

    async fn unpause(&self, name: &str) -> Result<(), OciError> {
        self.docker.unpause_container(name).await.map_err(map_err)
    }

    async fn stop(&self, name: &str) -> Result<(), OciError> {
        // Docker's default stop grace period is 10s (SIGTERM, wait, then
        // SIGKILL) — fine for a production service, but these are ephemeral
        // dev containers being hibernated/destroyed by the GC sweep, where
        // that 10s directly delays every `Hibernate`/`Destroy` action.
        // Measured live: destroying a `Paused` environment took ~10s longer
        // than expected because of this, working against the "Scale-to-Zero"
        // pitch (SPEC.md §3.2's ~300ms unpause target has a mirror image on
        // the teardown side).
        self.docker
            .stop_container(name, Some(StopContainerOptions { t: 2 }))
            .await
            .map_err(map_err)
    }

    async fn remove(&self, name: &str) -> Result<(), OciError> {
        self.docker
            .remove_container(
                name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(map_err)
    }

    async fn remove_image(&self, image: &str) -> Result<(), OciError> {
        self.docker
            .remove_image(
                image,
                Some(RemoveImageOptions {
                    force: true,
                    ..Default::default()
                }),
                None,
            )
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn logs(&self, name: &str) -> Result<String, OciError> {
        let options = LogsOptions {
            follow: false,
            stdout: true,
            stderr: true,
            timestamps: false,
            tail: "200".to_owned(),
            ..Default::default()
        };
        let mut stream = self.docker.logs(name, Some(options));
        let mut out = String::new();
        while let Some(line) = stream.next().await {
            let line = line.map_err(map_err)?;
            out.push_str(line.to_string().trim_end());
            out.push('\n');
        }
        Ok(out)
    }

    // Building the stream is synchronous — `bollard::logs` hands back a
    // `Stream` without awaiting anything, and the awaiting happens later, in
    // whoever consumes it. The `async` is still required: this implements
    // `ContainerPort::stream_logs`, whose signature belongs to the port
    // trait (declared `#[trait_variant::make(Send)]`), not to this adapter.
    // Flagged by `clippy::unused_async_trait_impl`, new in Rust 1.98.
    #[allow(clippy::unused_async_trait_impl)]
    async fn stream_logs(&self, name: &str) -> Result<LogStream, OciError> {
        let options = LogsOptions {
            follow: true,
            stdout: true,
            stderr: true,
            timestamps: false,
            tail: "50".to_owned(),
            ..Default::default()
        };
        let stream = self.docker.logs(name, Some(options)).map(|item| {
            item.map(|line| line.to_string().trim_end().to_owned())
                .map_err(map_err)
        });
        Ok(Box::pin(stream))
    }

    async fn exec(&self, name: &str, command: &str) -> Result<(), OciError> {
        let exec = self
            .docker
            .create_exec(
                name,
                CreateExecOptions {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(vec!["/bin/sh", "-c", command]),
                    ..Default::default()
                },
            )
            .await
            .map_err(map_err)?;

        let mut captured = String::new();
        if let StartExecResults::Attached { mut output, .. } = self
            .docker
            .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
            .await
            .map_err(map_err)?
        {
            while let Some(item) = output.next().await {
                captured.push_str(&item.map_err(map_err)?.to_string());
            }
        }

        // `start_exec` succeeding only means the command *ran*, not that it
        // exited zero — draining its output stream without checking this
        // silently swallowed failing `on_start` hooks (e.g. a broken
        // migration), which is exactly what SPEC.md and this port's contract
        // ("`OciError::Failure` if the command exits non-zero") promise.
        let inspected = self.docker.inspect_exec(&exec.id).await.map_err(map_err)?;
        match inspected.exit_code {
            Some(0) | None => Ok(()),
            Some(code) => Err(OciError::Failure(format!(
                "command `{command}` exited with status {code}: {}",
                captured.trim_end()
            ))),
        }
    }

    async fn container_status(&self, name: &str) -> Result<ContainerStatus, OciError> {
        match self.docker.inspect_container(name, None).await {
            Ok(info) => {
                let state = info.state.unwrap_or_default();
                if state.paused.unwrap_or(false) {
                    Ok(ContainerStatus::Paused)
                } else if state.running.unwrap_or(false) {
                    Ok(ContainerStatus::Running)
                } else {
                    Ok(ContainerStatus::Stopped)
                }
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(ContainerStatus::Missing),
            Err(e) => Err(map_err(e)),
        }
    }

    async fn host_capacity(&self) -> Result<HostCapacity, OciError> {
        let info = self.docker.info().await.map_err(map_err)?;
        Ok(HostCapacity {
            total_memory_bytes: info.mem_total.unwrap_or(0).max(0).cast_unsigned(),
            cpu_count: u32::try_from(info.ncpu.unwrap_or(0).max(0)).unwrap_or(0),
        })
    }

    async fn network_exists(&self, name: &str) -> Result<bool, OciError> {
        // The `name` filter is a substring match, so an exact-name check on
        // the results is still needed — otherwise `oxid-net` would report
        // "already exists" just because `oxid-net-2` does.
        let filters = HashMap::from([("name".to_owned(), vec![name.to_owned()])]);
        let existing = self
            .docker
            .list_networks(Some(ListNetworksOptions { filters }))
            .await
            .map_err(map_err)?;
        Ok(existing.iter().any(|n| n.name.as_deref() == Some(name)))
    }

    async fn ensure_network(&self, name: &str) -> Result<NetworkStatus, OciError> {
        if self.network_exists(name).await? {
            return Ok(NetworkStatus::AlreadyExisted);
        }
        self.docker
            .create_network(CreateNetworkOptions {
                name: name.to_owned(),
                ..Default::default()
            })
            .await
            .map_err(map_err)?;
        Ok(NetworkStatus::Created)
    }

    async fn ensure_traefik(&self, spec: TraefikSpec) -> Result<TraefikStatus, OciError> {
        match self
            .docker
            .inspect_container(&spec.container_name, None)
            .await
        {
            Ok(info) => {
                let running = info.state.and_then(|s| s.running).unwrap_or(false);
                if running {
                    return Ok(TraefikStatus::AlreadyRunning);
                }
                self.docker
                    .start_container::<String>(&spec.container_name, None)
                    .await
                    .map_err(map_err)?;
                Ok(TraefikStatus::StartedFromStopped)
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                self.create_and_start_traefik(&spec).await?;
                Ok(TraefikStatus::Created)
            }
            Err(e) => Err(map_err(e)),
        }
    }

    async fn traefik_runtime(&self, name: &str) -> Result<Option<TraefikRuntime>, OciError> {
        let inspect = match self.docker.inspect_container(name, None).await {
            Ok(inspect) => inspect,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(None),
            Err(e) => return Err(map_err(e)),
        };
        // Published *host* ports, which is what the spec names — the
        // container side is fixed at 80/443.
        let published_ports = inspect
            .network_settings
            .as_ref()
            .and_then(|n| n.ports.as_ref())
            .map(|ports| {
                ports
                    .values()
                    .flatten()
                    .flatten()
                    .filter_map(|b| b.host_port.as_ref()?.parse::<u16>().ok())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(TraefikRuntime {
            cmd: inspect
                .config
                .as_ref()
                .and_then(|c| c.cmd.clone())
                .unwrap_or_default(),
            published_ports,
            mount_targets: inspect
                .mounts
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| m.destination)
                .collect(),
        }))
    }

    async fn ensure_volume(&self, name: &str) -> Result<(), OciError> {
        // Docker's volume create is idempotent: an existing volume comes
        // back unchanged rather than erroring, so there is nothing to
        // check first.
        self.docker
            .create_volume(bollard::volume::CreateVolumeOptions {
                name: name.to_owned(),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(map_err)
    }

    async fn self_wiring_status(&self, network: &str) -> Result<SelfWiringStatus, OciError> {
        // Docker sets `HOSTNAME` to the short container id by default when
        // nothing overrides it — the same trick `docker inspect $HOSTNAME`
        // uses from inside a container to identify itself.
        let hostname = match std::env::var("HOSTNAME") {
            Ok(hostname) if !hostname.trim().is_empty() => hostname,
            _ => return Ok(SelfWiringStatus::NotContainerized),
        };
        // Not found (hostname doesn't resolve to a real container, e.g.
        // running Docker-in-Docker or a non-Docker container runtime) or any
        // other inspection failure: this is diagnostics, not a correctness
        // gate, so report "can't tell" rather than erroring the whole
        // status/bootstrap call.
        let Ok(info) = self.docker.inspect_container(&hostname, None).await else {
            return Ok(SelfWiringStatus::Unknown);
        };

        let joined_network = info
            .network_settings
            .and_then(|settings| settings.networks)
            .is_some_and(|networks| networks.contains_key(network));

        let labels = info
            .config
            .and_then(|config| config.labels)
            .unwrap_or_default();
        let has_traefik_enable_label =
            labels.get("traefik.enable").map(String::as_str) == Some("true");
        let references_oxid_wake = labels
            .iter()
            .any(|(k, v)| k.contains("oxid-wake") || v.contains("oxid-wake"));
        // The catch-all router is what lets a *stopped* environment be
        // woken at all, so it is checked separately from the `oxid-wake`
        // service above: a daemon can carry the service labels (wired
        // before this existed) and still 404 every scaled-to-zero branch.
        let has_wake_catchall = labels
            .keys()
            .any(|k| k.contains("routers.oxid-wake-catchall"));

        Ok(SelfWiringStatus::Detected {
            container_id: hostname,
            joined_network,
            has_traefik_enable_label,
            references_oxid_wake,
            has_wake_catchall,
        })
    }
}

/// Port Traefik's `web` entrypoint listens on *inside* its container. Fixed:
/// only the host port it is published on is an operator's choice.
const TRAEFIK_ENTRYPOINT_PORT: u16 = 80;

impl DockerClient {
    /// Creates and starts a brand-new Traefik container from `spec`. Only
    /// called by `ensure_traefik` when no container by that name exists yet.
    async fn create_and_start_traefik(&self, spec: &TraefikSpec) -> Result<(), OciError> {
        // Pull first: creating a container from an image the host has never
        // seen fails with a bare `No such image`, and nothing else in this
        // path would have fetched it.
        //
        // The Docker install hid this because its compose file pulls Traefik
        // before the daemon ever runs. `install.sh --server` has no compose
        // file, so on a fresh machine `oxid infra setup` — the one command
        // that is supposed to build the topology — failed on the image it
        // was about to start. Docker answers a pull for an image already
        // present from its own store, so this is cheap on every later call.
        ContainerPort::pull_image(self, &spec.image).await?;

        // The container side is always 80 — that is where Traefik's `web`
        // entrypoint listens (`--entrypoints.web.address=:80` below), and it
        // has nothing to do with which host port the operator publishes it
        // on. Both sides used to be `spec.http_port`, which happened to work
        // only because the port was hardcoded to 80: any other value
        // published a host port onto a container port nothing was listening
        // on, so Traefik came up and answered nothing.
        let port_key = format!("{TRAEFIK_ENTRYPOINT_PORT}/tcp");
        let mut exposed_ports = HashMap::new();
        // Docker's API represents "expose this port" as a mapping to an
        // empty JSON object (`{"80/tcp": {}}`) — same shape `run` above uses
        // for environment containers.
        #[allow(clippy::zero_sized_map_values)]
        exposed_ports.insert(port_key.clone(), HashMap::<(), ()>::new());

        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        port_bindings.insert(
            port_key,
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_owned()),
                host_port: Some(spec.http_port.to_string()),
            }]),
        );

        // TLS: a second entrypoint, its own published port, and a place to
        // keep the certificates.
        let mut binds = vec![format!(
            "{}:/var/run/docker.sock:ro",
            spec.docker_socket_path
        )];
        let mut env = vec!["DOCKER_API_VERSION=1.41".to_owned()];
        if let Some(https_port) = spec.https_port {
            let https_key = format!("{WEBSECURE_ENTRYPOINT_PORT}/tcp");
            #[allow(clippy::zero_sized_map_values)]
            exposed_ports.insert(https_key.clone(), HashMap::<(), ()>::new());
            port_bindings.insert(
                https_key,
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_owned()),
                    host_port: Some(https_port.to_string()),
                }]),
            );
        }
        if let Some(acme) = spec.acme.as_ref() {
            // A named volume, never a host path: Traefik refuses to start
            // when `acme.json` is not 0600, and a bind mount an operator
            // created is almost always 0644.
            binds.push(format!("{}:{ACME_MOUNT}", acme.storage_volume));
            if let AcmeChallenge::Dns01 { env_keys, .. } = &acme.challenge {
                // Only the *names* travel through the domain; the values are
                // read from this daemon's own environment here, at the last
                // possible moment, so a credential is never held in a struct
                // that could be serialized into a response or a log.
                for key in env_keys {
                    match std::env::var(key) {
                        Ok(value) => env.push(format!("{key}={value}")),
                        Err(_) => {
                            return Err(OciError::Failure(format!(
                                "the DNS-01 challenge needs `{key}`, which is not set on this                                  daemon — set it, or drop it from OXID_ACME_DNS_ENV"
                            )));
                        }
                    }
                }
            }
        }

        let config = Config {
            image: Some(spec.image.clone()),
            exposed_ports: Some(exposed_ports),
            // Kept in step with `docker-compose.yml`'s traefik service —
            // an operator who runs `oxid infra setup` instead of using the
            // compose file must get the same working proxy, and these flags
            // are what make wake-on-request work at all rather than merely
            // faster. See the comments there for the measurements.
            // Generated by `oxid_core::services::tls::traefik_cmd`, which
            // is also what `infra status` compares the running container
            // against. Two hand-maintained lists is how a Traefik ends up
            // running without the flags an operator thinks it has.
            cmd: Some(traefik_cmd(spec)),
            env: Some(env),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                binds: Some(binds),
                // Same rationale as environment containers (see `run`):
                // Traefik should come back on its own after a crash or host
                // reboot without anyone noticing.
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                ..Default::default()
            }),
            networking_config: Some(NetworkingConfig {
                endpoints_config: HashMap::from([(
                    spec.network.clone(),
                    EndpointSettings::default(),
                )]),
            }),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: spec.container_name.clone(),
            ..Default::default()
        };
        self.docker
            .create_container(Some(options), config)
            .await
            .map_err(map_err)?;
        self.docker
            .start_container::<String>(&spec.container_name, None)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCKER_SOCKET: &str = "/var/run/docker.sock";

    /// Integration test gated on a running Docker daemon; ignored by default.
    ///
    /// Run with `cargo test -p oxid-daemon -- --ignored` on a machine with
    /// Docker available.
    #[tokio::test]
    #[ignore = "requires a running Docker daemon"]
    async fn connects_and_lists_images() {
        let client = DockerClient::connect().unwrap();
        let _images = client.docker.list_images::<String>(None).await.unwrap();
    }

    #[test]
    #[ignore = "requires a running Docker daemon"]
    fn docker_socket_present() {
        // Sanity helper for ignored tests: fail early if no daemon is around.
        assert!(
            std::path::Path::new(DOCKER_SOCKET).exists(),
            "Docker socket `{DOCKER_SOCKET}` not found"
        );
    }

    /// The structured `BuildKit` path: only `[stage N/M]` Dockerfile steps
    /// count — internal loads, frontend resolves and image export are
    /// plumbing — and a vertex observed `cached` marks its digest.
    #[test]
    fn build_progress_parses_buildkit_vertexes() {
        use bollard::models::BuildInfoAux;
        let mk = |digest: &str, name: &str, cached: bool| bollard::models::BuildInfo {
            id: Some("moby.buildkit.trace".to_owned()),
            stream: None,
            error: None,
            error_detail: None,
            status: None,
            progress: None,
            progress_detail: None,
            aux: Some(BuildInfoAux::BuildKit(
                bollard::moby::buildkit::v1::StatusResponse {
                    vertexes: vec![bollard::moby::buildkit::v1::Vertex {
                        digest: digest.to_owned(),
                        inputs: vec![],
                        name: name.to_owned(),
                        cached,
                        ..Default::default()
                    }],
                    statuses: vec![],
                    logs: vec![],
                    warnings: vec![],
                },
            )),
        };
        let mut progress = BuildProgress::default();
        progress.observe(&mk(
            "d-from",
            "[stage-0 1/2] FROM docker.io/library/alpine:latest",
            false,
        ));
        progress.observe(&mk(
            "d-run",
            "[stage-0 2/2] RUN --mount=type=cache,target=/cache echo hi",
            false,
        ));
        progress.observe(&mk(
            "d-internal",
            "[internal] load remote build context",
            true,
        ));
        progress.observe(&mk("d-export", "exporting to image", true));
        progress.observe(&mk(
            "d-run",
            "[stage-0 2/2] RUN --mount=type=cache,target=/cache echo hi",
            true,
        ));
        // FROM and RUN count; the bracketed internal vertex must NOT.
        assert_eq!(progress.summary(), (2, 1));
    }

    /// The classic text fallback still works for engines that stream lines
    /// instead of structured events.
    #[test]
    fn build_progress_parses_cached_vs_executed_steps() {
        let mut progress = BuildProgress::default();
        progress.observe(&bollard::models::BuildInfo {
            stream: Some(
                concat!(
                    "#5 [1/3] FROM alpine:latest\n",
                    "#5 CACHED\n",
                    "#8 [2/3] RUN echo hi\n",
                    "#8 DONE 0.1s\n",
                    "#13 exporting to image\n",
                    "#13 DONE 0.0s\n",
                )
                .to_owned(),
            ),
            ..Default::default()
        });
        // FROM + RUN count (one cached); the unbracketed export step is
        // plumbing and doesn't.
        assert_eq!(progress.summary(), (2, 1));
    }

    /// Non-progress chatter (empty lines, ANSI-framed classic-builder
    /// output, unnumbered lines) contributes nothing and never panics.
    #[test]
    fn build_progress_ignores_unparseable_lines() {
        let mut progress = BuildProgress::default();
        progress.observe(&bollard::models::BuildInfo {
            stream: Some(
                "\n---\u{1b}-> Running in abc123\nStep 1/2 : FROM alpine\n#notanid CACHED\n"
                    .to_owned(),
            ),
            ..Default::default()
        });
        assert_eq!(progress.summary(), (0, 0));
    }

    /// Proves the reason builds go through `BuildKit` (`BuilderBuildKit` +
    /// bollard's `buildkit` feature, SPEC.md §4.5): `RUN
    /// --mount=type=cache` cache mounts persist data *across* `build`
    /// calls. Build #1 plants a marker inside the cache mount; build #2
    /// (whose Dockerfile was touched, forcing its RUN to actually
    /// re-execute instead of replaying from layer cache) fails outright if
    /// the marker is gone. Impossible on the legacy V1 builder, which has
    /// no cache mounts at all.
    #[tokio::test]
    #[ignore = "requires a running Docker daemon"]
    async fn buildkit_cache_mounts_persist_across_builds() {
        const CACHE_ID: &str = "oxid-test-buildkit-cache";
        const IMAGE: &str = "oxid-test/buildkit-cache";
        let client = DockerClient::connect().unwrap();
        // Best-effort: start clean so the test proves persistence within
        // itself rather than inheriting state from a previous run.
        let _ = client.docker.remove_volume(CACHE_ID, None).await;
        let dir = tempfile::tempdir().unwrap();
        let image = IMAGE.to_owned();

        // A trailing comment is enough to change the Dockerfile (and thus
        // the layer cache key of every instruction below it) between
        // builds while keeping the RUN itself byte-identical.
        let dockerfile_for = |touch: bool| -> String {
            format!(
                concat!(
                    "# syntax=docker/dockerfile:1.7\n",
                    "FROM alpine\n",
                    "RUN --mount=type=cache,target=/cache,id={CACHE_ID} ",
                    "[ -f /cache/marker ] || date +%s%N > /cache/marker\n",
                    "{touch}"
                ),
                CACHE_ID = CACHE_ID,
                touch = if touch { "# touched\n" } else { "" }
            )
        };
        let spec = |dockerfile: String| {
            std::fs::write(dir.path().join("Dockerfile"), dockerfile).unwrap();
            BuildSpec {
                context: dir.path().to_owned(),
                dockerfile: "Dockerfile".to_owned(),
                image: image.clone(),
            }
        };

        // Build #1: cold cache mount — plants the marker, must succeed.
        let cold = client
            .build(&spec(dockerfile_for(false)))
            .await
            .expect("first (cold-cache) build should succeed");
        assert!(
            cold.steps_total >= 1,
            "no steps parsed from a real BuildKit stream: {cold:?}"
        );

        // Build #2: warm cache mount — the marker must still be there.
        let warm = client
            .build(&spec(dockerfile_for(true)))
            .await
            .expect("cache mount did not persist data across builds");
        assert!(
            warm.steps_cached <= warm.steps_total,
            "incoherent build report: {warm:?}"
        );
        println!("build reports: cold={cold:?} warm={warm:?}");

        let _ = client.docker.remove_volume(CACHE_ID, None).await;
        let _ = client.remove_image(&image).await;
    }

    /// Documents SPEC.md §3.2's "<300ms unpause" target with real numbers:
    /// times only the *wake* operation — `unpause` for a `Paused`
    /// container, `start` for a `Hibernating` (stopped) one, matching what
    /// `ControlPlane::wake` actually performs — over repeated cycles,
    /// prints p50/p95/p99, and asserts both paths' p95 under their bars.
    /// Prior informal measurements ran 25–36ms; this makes that claim
    /// reproducible instead of anecdotal. Not part of CI — it needs Docker
    /// and measures wall-clock latency, which is machine-dependent; treat
    /// a local failure as "this host is slow", not "the code regressed".
    ///
    /// The two paths get different bars on purpose: the 300ms target was
    /// written about `unpause`, which resumes the already-initialized
    /// container in place. `start` boots a stopped container from scratch
    /// (runtime init, entrypoint exec) — same order of magnitude, but
    /// structurally slower, so its assertion carries more headroom.
    #[tokio::test]
    #[ignore = "requires a running Docker daemon"]
    async fn pause_wake_latency_stays_under_the_300ms_target() {
        const UNPAUSE_CYCLES: usize = 20;
        // Fewer cycles for the start path: each re-arm `stop` waits out
        // Docker's 2s grace period by design, so 10 cycles ≈ half a minute
        // of wall clock — enough samples for a p95 without a glacial test.
        const START_CYCLES: usize = 10;
        const IMAGE: &str = "oxid-test/wake-latency";
        let client = DockerClient::connect().unwrap();
        let name = "oxid-test-wake-latency";
        let _ = client.remove(name).await;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Dockerfile"),
            "FROM alpine\nCMD [\"sleep\", \"3600\"]\n",
        )
        .unwrap();
        client
            .build(&BuildSpec {
                context: dir.path().to_owned(),
                dockerfile: "Dockerfile".to_owned(),
                image: IMAGE.to_owned(),
            })
            .await
            .unwrap();
        let spec = ContainerSpec {
            name: name.to_owned(),
            image: IMAGE.to_owned(),
            env: std::collections::BTreeMap::default(),
            container_port: 8080,
            labels: std::collections::BTreeMap::default(),
            network: None,
            memory_limit_mb: None,
            cpu_limit_millicores: None,
        };
        client.run(&spec).await.unwrap();

        // Each cycle: re-arm the suspended state (untimed), then time only
        // the wake call itself — that alone is what a user waiting on a
        // woken URL experiences.
        let mut unpauses_ms = Vec::with_capacity(UNPAUSE_CYCLES);
        client.pause(name).await.expect("initial pause");
        for _ in 0..UNPAUSE_CYCLES {
            let start = std::time::Instant::now();
            client.unpause(name).await.expect("unpause");
            unpauses_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            client.pause(name).await.expect("re-arm pause");
        }

        // Re-arm = `stop`, which waits out Docker's grace period by
        // design (2s here) — deliberately outside the timed window.
        let mut starts_ms = Vec::with_capacity(START_CYCLES);
        client.stop(name).await.expect("initial stop");
        for _ in 0..START_CYCLES {
            let start = std::time::Instant::now();
            client.start(name).await.expect("start");
            starts_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            client.stop(name).await.expect("re-arm stop");
        }
        // Leave the container running so removal sees a clean state.
        client.start(name).await.expect("final start");

        let unpause_p95 = report_percentiles("unpause", &mut unpauses_ms);
        let start_p95 = report_percentiles("start", &mut starts_ms);

        client.remove(name).await.unwrap();
        let _ = client.remove_image(IMAGE).await;

        assert!(
            unpause_p95 < 300.0,
            "unpause p95 over the SPEC §3.2 300ms target: {unpause_p95:.1}ms"
        );
        assert!(
            start_p95 < 1000.0,
            "hibernating-start p95 unexpectedly slow: {start_p95:.1}ms"
        );
    }

    /// Sorts, prints p50/p95/p99, returns the p95.
    fn report_percentiles(label: &str, samples_ms: &mut [f64]) -> f64 {
        samples_ms.sort_by(f64::total_cmp);
        // Integer permille arithmetic for percentile indexes — no
        // float→usize casts.
        let pct = |permille: usize| {
            let n = samples_ms.len();
            let idx = ((permille * (n - 1)) + 500) / 1000;
            samples_ms[idx.min(n - 1)]
        };
        println!(
            "{label}: n={} p50={:.1}ms p95={:.1}ms p99={:.1}ms",
            samples_ms.len(),
            pct(500),
            pct(950),
            pct(990)
        );
        pct(950)
    }

    /// Regression test for a real bug found via manual E2E testing: `exec`
    /// drained a command's output and returned `Ok(())` regardless of its
    /// exit code, so a failing `[build].on_start` hook (e.g. a broken
    /// migration) was silently treated as a successful deploy.
    #[tokio::test]
    #[ignore = "requires a running Docker daemon"]
    async fn exec_reports_non_zero_exit_as_failure() {
        let client = DockerClient::connect().unwrap();
        let name = "oxid-test-exec-exit-code";
        let _ = client.remove(name).await;

        // `alpine`'s default CMD exits immediately with no TTY attached, so
        // `exec` would have nothing to attach to. Build a tiny image whose
        // CMD stays alive, matching how a real deploy builds before running.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Dockerfile"),
            "FROM alpine\nCMD [\"sleep\", \"3600\"]\n",
        )
        .unwrap();
        let image = "oxid-test/exec-exit-code".to_owned();
        client
            .build(&BuildSpec {
                context: dir.path().to_owned(),
                dockerfile: "Dockerfile".to_owned(),
                image: image.clone(),
            })
            .await
            .unwrap();

        let spec = ContainerSpec {
            name: name.to_owned(),
            image,
            env: std::collections::BTreeMap::default(),
            container_port: 8080,
            labels: std::collections::BTreeMap::default(),
            network: None,
            memory_limit_mb: None,
            cpu_limit_millicores: None,
        };
        // `run` always lets Docker pick the published host port itself now,
        // so this can never collide with anything else on the machine.
        client.run(&spec).await.unwrap();

        let ok = client.exec(name, "exit 0").await;
        assert!(ok.is_ok(), "{ok:?}");

        let failed = client.exec(name, "echo boom >&2; exit 7").await;
        let err = failed.unwrap_err();
        assert!(
            matches!(&err, OciError::Failure(m) if m.contains('7') && m.contains("boom")),
            "{err:?}"
        );

        client.remove(name).await.unwrap();
    }

    /// Regression test for the exact real-world complaint that motivated
    /// this: without Traefik, two branches of the same project both
    /// wanting the same `container_port` used to mean the second deploy
    /// failed outright ("port is already allocated") the moment the first
    /// was still up. `run` now always lets Docker choose the host port
    /// itself, so both should start successfully with two *different*
    /// bound ports and no conflict at all.
    #[tokio::test]
    #[ignore = "requires a running Docker daemon"]
    async fn run_assigns_distinct_host_ports_for_concurrent_containers_on_the_same_port() {
        let client = DockerClient::connect().unwrap();
        let name_a = "oxid-test-dynamic-port-a";
        let name_b = "oxid-test-dynamic-port-b";
        let _ = client.remove(name_a).await;
        let _ = client.remove(name_b).await;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Dockerfile"),
            "FROM alpine\nCMD [\"sleep\", \"3600\"]\n",
        )
        .unwrap();
        let image = "oxid-test/dynamic-port".to_owned();
        client
            .build(&BuildSpec {
                context: dir.path().to_owned(),
                dockerfile: "Dockerfile".to_owned(),
                image: image.clone(),
            })
            .await
            .unwrap();

        let spec = |name: &str| ContainerSpec {
            name: name.to_owned(),
            image: image.clone(),
            env: std::collections::BTreeMap::default(),
            container_port: 8080,
            labels: std::collections::BTreeMap::default(),
            network: None,
            memory_limit_mb: None,
            cpu_limit_millicores: None,
        };

        let port_a = client.run(&spec(name_a)).await.unwrap();
        let port_b = client.run(&spec(name_b)).await.unwrap();

        assert!(port_a.is_some(), "{port_a:?}");
        assert!(port_b.is_some(), "{port_b:?}");
        assert_ne!(port_a, port_b, "both containers got the same host port");

        client.remove(name_a).await.unwrap();
        client.remove(name_b).await.unwrap();
    }

    #[test]
    fn tars_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM alpine\n").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), "hi").unwrap();

        let tar = tar_context(dir.path()).unwrap();
        assert!(!tar.is_empty());

        let mut archive = tar::Archive::new(tar.as_ref());
        let entries = archive.entries().unwrap().count();
        assert!(
            entries >= 2,
            "expected Dockerfile + sub/file.txt, got {entries}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn tars_a_directory_containing_a_dangling_symlink() {
        // Regression: a real repo had a dangling symlink under
        // `.claude/skills/` (its target predates a cleanup commit) —
        // `follow_symlinks(true)` (the `tar` crate's default) dereferences
        // every entry via `fs::metadata`, so that one unrelated broken link
        // used to fail the *entire* build context tar with a bare ENOENT.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM alpine\n").unwrap();
        std::os::unix::fs::symlink("does/not/exist", dir.path().join("dangling")).unwrap();

        let tar = tar_context(dir.path()).expect("a dangling symlink must not fail the build");

        let mut archive = tar::Archive::new(tar.as_ref());
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("dangling")),
            "expected the symlink itself to be archived, got {names:?}"
        );
    }

    /// A Traefik with certificates configured must actually start, and its
    /// argv and mounts must match what `traefik_drift` will later compare
    /// against — otherwise `infra status` reports drift on a container Oxid
    /// itself just created.
    #[tokio::test]
    #[ignore = "requires a running Docker daemon"]
    async fn a_traefik_with_acme_starts_and_reports_no_drift_against_its_own_spec() {
        use oxid_core::services::tls::{TraefikRuntime, traefik_drift};
        use oxid_core::{AcmeChallenge, AcmeConfig};

        let client = DockerClient::connect().unwrap();
        let network = "oxid-test-acme-net";
        let container_name = "oxid-test-acme-traefik";
        let volume = "oxid-test-acme-store";

        let _ = client.remove(container_name).await;
        let _ = client.docker.remove_network(network).await;

        client.ensure_network(network).await.unwrap();
        let spec = TraefikSpec {
            network: network.to_owned(),
            container_name: container_name.to_owned(),
            http_port: 18_081,
            ..TraefikSpec::new(network)
        }
        .with_acme(
            AcmeConfig {
                email: "ops@example.com".to_owned(),
                // HTTP-01 deliberately: it needs no credentials, so this
                // test asserts the container shape without depending on
                // the daemon's environment. The DNS-01 argv is covered by
                // the pure tests in `oxid_core::services::tls`.
                challenge: AcmeChallenge::Http01,
                // Staging: this test must never touch production rate
                // limits, and it never completes a challenge anyway.
                ca_directory: Some(
                    "https://acme-staging-v02.api.letsencrypt.org/directory".to_owned(),
                ),
                storage_volume: volume.to_owned(),
                resolver_name: "oxid".to_owned(),
                http_redirect: true,
            },
            18_443,
        );

        assert_eq!(
            client.ensure_traefik(spec.clone()).await.unwrap(),
            TraefikStatus::Created
        );
        assert_eq!(
            client.container_status(container_name).await.unwrap(),
            ContainerStatus::Running,
            "a Traefik configured for TLS must actually start"
        );

        // What `infra status` will compare: the container Oxid just made
        // must satisfy the spec Oxid made it from.
        let inspect = client
            .docker
            .inspect_container(container_name, None)
            .await
            .unwrap();
        let actual = TraefikRuntime {
            cmd: inspect
                .config
                .as_ref()
                .and_then(|c| c.cmd.clone())
                .unwrap_or_default(),
            published_ports: vec![spec.http_port, spec.https_port.unwrap()],
            mount_targets: inspect
                .mounts
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| m.destination)
                .collect(),
        };
        assert!(
            traefik_drift(&spec, &actual).is_empty(),
            "a freshly created Traefik must not report drift: {:?}",
            traefik_drift(&spec, &actual)
        );

        let _ = client.remove(container_name).await;
        let _ = client.docker.remove_network(network).await;
        let _ = client.docker.remove_volume(volume, None).await;
    }

    /// Exercises `ensure_network`/`ensure_traefik`/`network_exists` against
    /// a real Docker daemon, asserting the idempotency `oxid infra setup`
    /// depends on: running either twice in a row must be a no-op the second
    /// time, not a failure or a duplicate.
    #[tokio::test]
    #[ignore = "requires a running Docker daemon"]
    async fn ensure_network_and_traefik_are_idempotent() {
        let client = DockerClient::connect().unwrap();
        let network = "oxid-test-infra-net";
        let container_name = "oxid-test-infra-traefik";

        // Clean up anything left over from a prior failed run before
        // asserting a clean starting state.
        let _ = client.remove(container_name).await;
        let _ = client.docker.remove_network(network).await;

        assert!(!client.network_exists(network).await.unwrap());

        let first_network = client.ensure_network(network).await.unwrap();
        assert_eq!(first_network, NetworkStatus::Created);
        assert!(client.network_exists(network).await.unwrap());

        let second_network = client.ensure_network(network).await.unwrap();
        assert_eq!(second_network, NetworkStatus::AlreadyExisted);

        let spec = TraefikSpec {
            network: network.to_owned(),
            image: "traefik:v3.3".to_owned(),
            container_name: container_name.to_owned(),
            http_port: 18_080,
            docker_socket_path: "/var/run/docker.sock".to_owned(),
            https_port: None,
            acme: None,
        };

        let first_traefik = client.ensure_traefik(spec.clone()).await.unwrap();
        assert_eq!(first_traefik, TraefikStatus::Created);
        assert_eq!(
            client.container_status(container_name).await.unwrap(),
            ContainerStatus::Running
        );

        let second_traefik = client.ensure_traefik(spec).await.unwrap();
        assert_eq!(second_traefik, TraefikStatus::AlreadyRunning);

        client.remove(container_name).await.unwrap();
        client.docker.remove_network(network).await.unwrap();
    }
}
