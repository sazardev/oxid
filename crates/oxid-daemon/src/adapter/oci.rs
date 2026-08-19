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
use bollard::models::{EndpointSettings, HostConfig, PortBinding};
use bytes::Bytes;
use futures_util::StreamExt;
use oxid_core::{BuildSpec, ContainerPort, ContainerSpec, LogStream, OciError};

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

    async fn run(&self, spec: &ContainerSpec) -> Result<(), OciError> {
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(format!("{}/tcp", spec.container_port), HashMap::new());

        // When a Traefik network is configured, the container is reached
        // directly over that network and no host port is published — two
        // branches of the same project can then run concurrently. Without
        // it, fall back to publishing `host_port` for direct local access.
        let port_bindings = spec.network.is_none().then(|| {
            let mut bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
            bindings.insert(
                format!("{}/tcp", spec.container_port),
                Some(vec![PortBinding {
                    host_port: Some(spec.host_port.to_string()),
                    ..Default::default()
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
        Ok(())
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
            host_port: 0,
            labels: std::collections::BTreeMap::default(),
            network: None,
            memory_limit_mb: None,
            cpu_limit_millicores: None,
        };
        // `run` publishes `host_port`; 0 lets Docker pick a free one so this
        // test doesn't collide with anything else on the machine.
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
}
