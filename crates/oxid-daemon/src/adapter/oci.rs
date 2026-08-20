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
use bollard::image::{BuildImageOptions, RemoveImageOptions};
use bollard::models::{
    EndpointSettings, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::network::{CreateNetworkOptions, ListNetworksOptions};
use bytes::Bytes;
use futures_util::StreamExt;
use oxid_core::{
    BuildSpec, ContainerPort, ContainerSpec, ContainerStatus, HostCapacity, LogStream,
    NetworkStatus, OciError, SelfWiringStatus, TraefikSpec, TraefikStatus,
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
        builder.append_dir_all(".", dir).map_err(|e| {
            OciError::Failure(format!("cannot tar build context `{}`: {e}", dir.display()))
        })?;
        builder
            .finish()
            .map_err(|e| OciError::Failure(format!("cannot finalize build context tar: {e}")))?;
    }
    Ok(Bytes::from(buf))
}

impl ContainerPort for DockerClient {
    async fn build(&self, spec: &BuildSpec) -> Result<(), OciError> {
        let options = BuildImageOptions {
            dockerfile: spec.dockerfile.clone(),
            t: spec.image.clone(),
            rm: true,
            ..Default::default()
        };
        let context = tar_context(&spec.context)?;
        let mut stream = self.docker.build_image(options, None, Some(context));
        while let Some(item) = stream.next().await {
            item.map_err(map_err)?;
        }
        Ok(())
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

        Ok(SelfWiringStatus::Detected {
            container_id: hostname,
            joined_network,
            has_traefik_enable_label,
            references_oxid_wake,
        })
    }
}

impl DockerClient {
    /// Creates and starts a brand-new Traefik container from `spec`. Only
    /// called by `ensure_traefik` when no container by that name exists yet.
    async fn create_and_start_traefik(&self, spec: &TraefikSpec) -> Result<(), OciError> {
        let port_key = format!("{}/tcp", spec.http_port);
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

        let config = Config {
            image: Some(spec.image.clone()),
            exposed_ports: Some(exposed_ports),
            cmd: Some(vec![
                "--providers.docker=true".to_owned(),
                format!("--providers.docker.network={}", spec.network),
                "--providers.docker.exposedbydefault=false".to_owned(),
                "--entrypoints.web.address=:80".to_owned(),
            ]),
            host_config: Some(HostConfig {
                port_bindings: Some(port_bindings),
                binds: Some(vec![format!(
                    "{}:/var/run/docker.sock:ro",
                    spec.docker_socket_path
                )]),
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
