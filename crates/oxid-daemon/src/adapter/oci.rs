//! OCI/container adapter (SPEC.md §2.2 "Orquestación OCI").
//!
//! Talks to the local Docker socket via [`bollard`]. All methods map Docker
//! errors to [`OciError`].

use std::collections::HashMap;
use std::path::Path;

use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, LogsOptions, RemoveContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::image::BuildImageOptions;
use bollard::models::{HostConfig, PortBinding};
use bytes::Bytes;
use futures_util::StreamExt;
use oxid_core::{BuildSpec, ContainerPort, ContainerSpec, OciError};

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

        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        port_bindings.insert(
            format!("{}/tcp", spec.container_port),
            Some(vec![PortBinding {
                host_port: Some(spec.host_port.to_string()),
                ..Default::default()
            }]),
        );

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
                port_bindings: Some(port_bindings),
                ..Default::default()
            }),
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

    async fn pause(&self, name: &str) -> Result<(), OciError> {
        self.docker.pause_container(name).await.map_err(map_err)
    }

    async fn unpause(&self, name: &str) -> Result<(), OciError> {
        self.docker.unpause_container(name).await.map_err(map_err)
    }

    async fn stop(&self, name: &str) -> Result<(), OciError> {
        self.docker
            .stop_container(name, None)
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

        match self
            .docker
            .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
            .await
            .map_err(map_err)?
        {
            StartExecResults::Attached { mut output, .. } => {
                while let Some(item) = output.next().await {
                    item.map_err(map_err)?;
                }
                Ok(())
            }
            StartExecResults::Detached => Ok(()),
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
